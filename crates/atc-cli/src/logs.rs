//! `atc logs [-f] <slug-or-session>` — tail stream-json log for a dispatch.

use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::registry::{Registry, StatusFilter};
use atc_core::stream_json::{parse_stream_events, StreamEvent};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;

/// Resolve the log file path from a slug-or-session argument.
///
/// Lookup priority:
/// 1. Try registry.get(arg) by slug (PRIMARY KEY)
/// 2. If not found: scan all records for record.session == arg
/// 3. If still not found: attempt <log_dir>/<arg>.jsonl as path fallback
async fn resolve_log_path(
    registry: &dyn Registry,
    config: &AtcConfig,
    arg: &str,
) -> Result<PathBuf> {
    // 1. Try by slug
    if let Some(record) = registry.get(arg).await? {
        return Ok(record.log_file);
    }

    // 2. Scan for session match
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
fn print_existing_lines(path: &std::path::Path) -> Result<()> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        for event in parse_stream_events(&line) {
            render_event(&event);
        }
    }
    Ok(())
}

pub async fn run_logs(
    registry: Arc<dyn Registry>,
    config: &AtcConfig,
    arg: &str,
    follow: bool,
) -> Result<()> {
    let log_path = resolve_log_path(registry.as_ref(), config, arg).await?;

    if !log_path.exists() {
        anyhow::bail!("No log file: {}", log_path.display());
    }

    // Print existing content
    print_existing_lines(&log_path)?;

    if !follow {
        return Ok(());
    }

    // Follow mode: use notify for file change events with poll fallback
    follow_log(&log_path).await
}

/// Follow a log file, printing new lines as they appear.
async fn follow_log(path: &std::path::Path) -> Result<()> {
    use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
    use tokio::sync::mpsc;

    let file = tokio::fs::File::open(path).await?;
    let file_len = file.metadata().await?.len();
    let reader = tokio::io::BufReader::new(file);
    let mut lines = reader.lines();

    // Skip to end — we already printed existing content
    let mut pos = 0u64;
    while let Some(line) = lines.next_line().await? {
        pos += line.len() as u64 + 1; // +1 for newline
    }
    // Ensure we start from the right position if file was shorter
    if pos < file_len {
        pos = file_len;
    }

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
                // Check for new content
                let metadata = tokio::fs::metadata(path).await?;
                let new_len = metadata.len();
                if new_len > pos {
                    // Seek to where we left off and read only new bytes
                    use tokio::io::AsyncSeekExt;
                    let mut file = tokio::fs::File::open(path).await?;
                    file.seek(std::io::SeekFrom::Start(pos)).await?;
                    let reader = tokio::io::BufReader::new(file);
                    let mut lines = reader.lines();

                    while let Some(line) = lines.next_line().await? {
                        for event in parse_stream_events(&line) {
                            render_event(&event);
                        }
                    }
                    pos = new_len;
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
        // Should not panic
        print_existing_lines(&path).unwrap();
    }

    #[test]
    fn test_print_existing_lines_with_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        std::fs::write(
            &path,
            "not json\n{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}\n{broken\n",
        )
        .unwrap();
        // Should not panic — invalid lines are silently skipped
        print_existing_lines(&path).unwrap();
    }
}
