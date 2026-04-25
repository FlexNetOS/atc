//! `atc logs [-f] <slug-or-session>` — tail stream-json log for a dispatch.

use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::registry::{Registry, StatusFilter};
use atc_core::stream_json::{parse_stream_events, StreamEvent};
use std::path::PathBuf;
use std::sync::Arc;

/// Resolve the log file path from an id-or-session argument.
///
/// Lookup priority:
/// 1. Try registry.get(arg) by ID (PRIMARY KEY)
/// 2. Try registry.find_latest_for_task(arg) by task slug
/// 3. If not found: scan all records for record.session == arg
/// 4. If still not found: attempt <log_dir>/<arg>.jsonl as path fallback
async fn resolve_log_path(
    registry: &dyn Registry,
    config: &AtcConfig,
    arg: &str,
) -> Result<PathBuf> {
    // 1. Try by ID
    if let Some(record) = registry.get(arg).await? {
        return Ok(record.log_file);
    }

    // 2. Try by task slug
    if let Some(record) = registry.find_latest_for_task(arg).await? {
        return Ok(record.log_file);
    }

    // 3. Scan for session match
    let all_records = registry.list(StatusFilter::All).await?;
    for record in &all_records {
        if record.session == arg {
            return Ok(record.log_file.clone());
        }
    }

    // 3. Path fallback — sanitize to prevent directory traversal
    let log_dir = config.dispatch.resolved_log_dir();
    let sanitized = std::path::Path::new(arg)
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("invalid log argument: {arg}"))?;
    let fallback = log_dir.join(format!("{}.jsonl", sanitized.to_string_lossy()));
    if fallback.exists() {
        return Ok(fallback);
    }

    anyhow::bail!("No log file: {}", fallback.display());
}

/// Render a single stream event to stdout.
fn render_event(event: &StreamEvent) {
    match event {
        StreamEvent::AssistantText(text) => {
            for line in text.lines() {
                println!(">>> {line}");
            }
        }
        StreamEvent::ToolUse { name, input } => {
            let display_input = if input.chars().count() > 120 {
                let truncated: String = input.chars().take(120).collect();
                format!("{truncated}\u{2026}")
            } else {
                input.clone()
            };
            println!("  [tool] {name}: {display_input}");
        }
        StreamEvent::Result(r) => {
            let cost = r
                .total_cost_usd
                .map(|c| format!("${c}"))
                .unwrap_or_else(|| "-".to_string());
            let turns = r
                .num_turns
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".to_string());
            let duration = r
                .duration_ms
                .map(|ms| format!("{}s", ms / 1000))
                .unwrap_or_else(|| "-".to_string());
            println!();
            println!(
                "=== RESULT: {} | cost={} | turns={} | duration={} ===",
                r.subtype, cost, turns, duration
            );
        }
        StreamEvent::Skip => {}
    }
}

/// Print all existing lines from a log file using buffered I/O.
/// Uses lossy UTF-8 decoding so partially-written or binary-corrupted lines
/// don't abort the replay.
/// Returns the byte offset at end of file, so follow mode can continue from
/// exactly where replay left off without skipping events.
fn print_existing_lines(path: &std::path::Path) -> Result<u64> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut pos = 0u64;
    loop {
        let mut bytes = Vec::new();
        let n = reader.read_until(b'\n', &mut bytes)?;
        if n == 0 {
            break;
        }
        pos += n as u64;
        let line = String::from_utf8_lossy(&bytes);
        let line = line.trim_end_matches('\n').trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        for event in parse_stream_events(line) {
            render_event(&event);
        }
    }
    Ok(pos)
}

pub async fn run_logs(
    registry: Arc<dyn Registry>,
    config: &AtcConfig,
    arg: &str,
    follow: bool,
) -> Result<()> {
    let log_path = resolve_log_path(registry.as_ref(), config, arg).await?;

    if !log_path.exists() {
        anyhow::bail!(
            "No log file: {}\nhint: try `atc info {arg}` to verify the dispatch exists.",
            log_path.display()
        );
    }

    if follow {
        // Follow mode bypasses the pager — output should stream directly.
        let start_pos = print_existing_lines(&log_path)?;
        return follow_log(&log_path, start_pos).await;
    }

    // Non-follow mode: route through the pager for long replay.
    let _pager = crate::pager::setup_pager(Some(&config.pager));
    print_existing_lines(&log_path)?;
    Ok(())
}

/// Follow a log file, printing new lines as they appear.
/// `start_pos` is the byte offset where replay ended, so we don't skip events
/// appended between replay and follow startup.
async fn follow_log(path: &std::path::Path, start_pos: u64) -> Result<()> {
    use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
    use tokio::sync::mpsc;

    // Use the replay offset, but clamp to current file length in case the file
    // was truncated between replay and follow startup.
    let current_len = tokio::fs::metadata(path).await?.len();
    let mut pos = if start_pos > current_len {
        current_len
    } else {
        start_pos
    };
    let mut pending = String::new();

    let (tx, mut rx) = mpsc::channel::<()>(16);
    let watched_path = path.to_path_buf();

    // Set up file watcher with poll fallback
    let _watcher = {
        let tx = tx.clone();
        let mut watcher: RecommendedWatcher =
            notify::recommended_watcher(move |res: std::result::Result<Event, notify::Error>| {
                if let Ok(event) = res {
                    if matches!(
                        event.kind,
                        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Any
                    ) {
                        let _ = tx.blocking_send(());
                    }
                }
            })?;
        watcher.watch(
            watched_path.parent().unwrap_or(std::path::Path::new(".")),
            RecursiveMode::NonRecursive,
        )?;
        watcher
    };

    // Also spawn a poll fallback every 200ms
    let poll_tx = tx.clone();
    let poll_handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if poll_tx.send(()).await.is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            _ = rx.recv() => {
                // Check for new content.
                // Handle transient NotFound (file rotation/recreation) by
                // resetting position and waiting for the file to reappear.
                let metadata = match tokio::fs::metadata(path).await {
                    Ok(m) => m,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        pos = 0;
                        pending.clear();
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                };
                let new_len = metadata.len();
                if new_len < pos {
                    // File was truncated or rotated; restart from beginning.
                    pos = 0;
                    pending.clear();
                }
                if new_len > pos {
                    // Seek to where we left off and read only new bytes
                    use tokio::io::{AsyncReadExt, AsyncSeekExt};
                    let mut file = match tokio::fs::File::open(path).await {
                        Ok(f) => f,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            pos = 0;
                            pending.clear();
                            continue;
                        }
                        Err(e) => return Err(e.into()),
                    };
                    file.seek(std::io::SeekFrom::Start(pos)).await?;
                    let mut buf = Vec::new();
                    file.read_to_end(&mut buf).await?;
                    pos += buf.len() as u64;

                    // Accumulate into pending buffer and process complete lines
                    pending.push_str(&String::from_utf8_lossy(&buf));
                    while let Some(i) = pending.find('\n') {
                        let line: String = pending.drain(..=i).collect();
                        let line = line.trim_end();
                        if !line.is_empty() {
                            for event in parse_stream_events(line) {
                                render_event(&event);
                            }
                        }
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                break;
            }
        }
    }

    poll_handle.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atc_core::stream_json::StreamEvent;

    #[test]
    fn test_render_assistant_text() {
        // Just verify the render function doesn't panic
        render_event(&StreamEvent::AssistantText("Hello world".to_string()));
    }

    #[test]
    fn test_render_tool_use_truncation() {
        let long_input = "x".repeat(200);
        render_event(&StreamEvent::ToolUse {
            name: "Bash".to_string(),
            input: long_input,
        });
    }

    #[test]
    fn test_render_result() {
        use atc_core::stream_json::ResultEvent;
        render_event(&StreamEvent::Result(ResultEvent {
            subtype: "success".to_string(),
            total_cost_usd: Some(1.23),
            num_turns: Some(10),
            duration_ms: Some(60_000),
        }));
    }

    #[test]
    fn test_print_existing_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"}]}}
{"type":"result","subtype":"success","total_cost_usd":1.0,"num_turns":5,"duration_ms":30000}
"#,
        )
        .unwrap();
        // Should not panic, and should return byte count
        let pos = print_existing_lines(&path).unwrap();
        assert!(pos > 0);
    }

    #[test]
    fn test_print_existing_lines_with_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let content =
            "not json\n{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}\n{broken\n";
        std::fs::write(&path, content).unwrap();
        // Should not panic — invalid lines are silently skipped
        let pos = print_existing_lines(&path).unwrap();
        assert_eq!(pos, content.len() as u64);
    }

    #[test]
    fn test_print_existing_lines_returns_exact_byte_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let content = "line one\nline two\n";
        std::fs::write(&path, content).unwrap();
        let pos = print_existing_lines(&path).unwrap();
        assert_eq!(pos, content.len() as u64);
    }

    #[test]
    fn test_print_existing_lines_handles_partial_last_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        // No trailing newline — simulates a file still being written
        let content = "line one\npartial";
        std::fs::write(&path, content).unwrap();
        let pos = print_existing_lines(&path).unwrap();
        // Should read all bytes including the partial line
        assert_eq!(pos, content.len() as u64);
    }

    #[test]
    fn test_print_existing_lines_lossy_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        // Write invalid UTF-8 bytes followed by a valid line
        let mut bytes: Vec<u8> = vec![0xFF, 0xFE, b'\n'];
        bytes.extend_from_slice(b"{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}\n");
        std::fs::write(&path, &bytes).unwrap();
        // Should not panic — lossy decoding handles invalid UTF-8
        let pos = print_existing_lines(&path).unwrap();
        assert_eq!(pos, bytes.len() as u64);
    }
}
