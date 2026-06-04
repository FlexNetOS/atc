//! `atc watch` — Watch running agent sessions and emit structured events.
//!
//! Monitors tmux session liveness, tails log files for new events, and emits
//! structured NDJSON events for consumption by AI harnesses or human terminals.

use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::post_completion;
use atc_core::registry::{Registry, StatusFilter};
use atc_core::stream_json;
use atc_core::terminal_text::{display_text, terminal_safe_json};
use atc_core::types::Status;
use serde::Serialize;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;
use tracing::{info, warn};

/// Events emitted by the watcher.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WatchEvent {
    /// Dispatch started (emitted once at watch start).
    Started {
        id: String,
        task: Option<String>,
        directive: String,
        worktree: String,
    },
    /// New log line parsed.
    LogLine {
        id: String,
        #[serde(rename = "type")]
        event_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
    },
    /// Cost crossed configurable threshold.
    CostThreshold {
        id: String,
        cost_usd: f64,
        threshold: f64,
    },
    /// Agent completed (result event seen in log).
    Completed {
        id: String,
        status: String,
        cost_usd: Option<f64>,
        pr_url: Option<String>,
        summary: Option<String>,
    },
    /// Agent failed.
    Failed {
        id: String,
        status: String,
        subtype: String,
    },
    /// Tmux session died without result event.
    SessionDied { id: String },
}

/// State for tracking a single dispatch's log file.
struct DispatchWatcher {
    id: String,
    log_file: PathBuf,
    /// PR URLs known at dispatch time (from registry record).
    pr_urls: Vec<String>,
    lines_read: usize,
    last_len: u64,
    cost_threshold_fired: bool,
    saw_result: bool,
    finalized: bool,
}

impl DispatchWatcher {
    fn new(id: String, log_file: PathBuf, pr_urls: Vec<String>) -> Self {
        Self {
            id,
            log_file,
            pr_urls,
            lines_read: 0,
            last_len: 0,
            cost_threshold_fired: false,
            saw_result: false,
            finalized: false,
        }
    }

    /// Read new lines from the log file and emit events.
    fn poll_log(&mut self, cost_threshold: f64) -> Vec<WatchEvent> {
        let mut events = Vec::new();

        // Reset tail state if the log file was truncated or rotated
        if let Ok(meta) = std::fs::metadata(&self.log_file) {
            let new_len = meta.len();
            if new_len < self.last_len {
                self.lines_read = 0;
            }
            self.last_len = new_len;
        }

        let file = match std::fs::File::open(&self.log_file) {
            Ok(f) => f,
            Err(_) => return events,
        };
        let reader = std::io::BufReader::new(file);

        for (i, line) in reader.lines().enumerate() {
            if i < self.lines_read {
                continue;
            }
            self.lines_read = i + 1;

            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };

            for event in stream_json::parse_stream_events(&line) {
                match &event {
                    stream_json::StreamEvent::AssistantText(text) => {
                        events.push(WatchEvent::LogLine {
                            id: self.id.clone(),
                            event_type: "assistant".to_string(),
                            text: Some(truncate(text, 500)),
                            tool: None,
                        });
                    }
                    stream_json::StreamEvent::ToolUse { name, input } => {
                        events.push(WatchEvent::LogLine {
                            id: self.id.clone(),
                            event_type: "tool_use".to_string(),
                            text: Some(truncate(input, 200)),
                            tool: Some(name.clone()),
                        });
                    }
                    stream_json::StreamEvent::Result(r) => {
                        self.saw_result = true;

                        // Emit cost threshold before completion so consumers see it first
                        if let Some(cost) = r.total_cost_usd {
                            if cost > cost_threshold && !self.cost_threshold_fired {
                                self.cost_threshold_fired = true;
                                events.push(WatchEvent::CostThreshold {
                                    id: self.id.clone(),
                                    cost_usd: cost,
                                    threshold: cost_threshold,
                                });
                            }
                        }

                        if r.subtype == "success" {
                            events.push(WatchEvent::Completed {
                                id: self.id.clone(),
                                status: "done".to_string(),
                                cost_usd: r.total_cost_usd,
                                pr_url: self.pr_urls.first().cloned(),
                                summary: None,
                            });
                        } else {
                            events.push(WatchEvent::Failed {
                                id: self.id.clone(),
                                status: "failed".to_string(),
                                subtype: r.subtype.clone(),
                            });
                        }
                    }
                    stream_json::StreamEvent::Skip => {}
                }
            }
        }

        events
    }
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((byte_idx, _)) => format!("{}…", &s[..byte_idx]),
        None => s.to_string(),
    }
}

/// Check if a tmux session exists.
async fn tmux_session_alive(session: &str) -> bool {
    tokio::process::Command::new("tmux")
        .args(["has-session", "-t", session])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Output format for the watcher.
enum OutputFormat {
    /// Raw JSONL — one JSON object per line. Default.
    Ndjson,
    /// Human-readable with icons and indentation.
    Human,
    /// Pretty: concise one-line-per-event with color (like --pretty for logs).
    Pretty,
}

fn render_event_lines(event: &WatchEvent, format: &OutputFormat) -> (Vec<String>, Vec<String>) {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    match format {
        OutputFormat::Ndjson => {
            stdout.push(terminal_safe_json(event).unwrap_or_default());
        }
        OutputFormat::Human => match event {
            WatchEvent::Started {
                id,
                task,
                directive,
                ..
            } => {
                let label = task.as_deref().unwrap_or(id);
                stderr.push(format!(
                    "▶ watching {} ({})",
                    display_text(label),
                    display_text(directive)
                ));
            }
            WatchEvent::LogLine {
                event_type,
                text,
                tool,
                ..
            } => match event_type.as_str() {
                "assistant" => {
                    if let Some(t) = text {
                        for line in t.lines() {
                            stdout.push(format!(">>> {}", display_text(line)));
                        }
                    }
                }
                "tool_use" => {
                    let tool_name = tool.as_deref().unwrap_or("?");
                    let input = text.as_deref().unwrap_or("");
                    stdout.push(format!(
                        "  [{}] {}",
                        display_text(tool_name),
                        display_text(input)
                    ));
                }
                _ => {}
            },
            WatchEvent::CostThreshold {
                id,
                cost_usd,
                threshold,
            } => {
                stderr.push(format!(
                    "⚠ {}: cost ${cost_usd:.2} exceeds ${threshold:.2} threshold",
                    display_text(id)
                ));
            }
            WatchEvent::Completed {
                id,
                cost_usd,
                pr_url,
                ..
            } => {
                let cost = cost_usd
                    .map(|c| format!("${c:.2}"))
                    .unwrap_or_else(|| "-".to_string());
                let pr = pr_url.as_deref().unwrap_or("no PR");
                stderr.push(format!(
                    "✅ {}: done ({cost}) {}",
                    display_text(id),
                    display_text(pr)
                ));
            }
            WatchEvent::Failed { id, subtype, .. } => {
                stderr.push(format!(
                    "❌ {}: failed ({})",
                    display_text(id),
                    display_text(subtype)
                ));
            }
            WatchEvent::SessionDied { id } => {
                stderr.push(format!(
                    "💀 {}: session died without result event",
                    display_text(id)
                ));
            }
        },
        OutputFormat::Pretty => match event {
            WatchEvent::Started {
                id,
                task,
                directive,
                ..
            } => {
                let label = task.as_deref().unwrap_or(id);
                stderr.push(format!(
                    "▶ {} ({})",
                    display_text(label),
                    display_text(directive)
                ));
            }
            WatchEvent::LogLine {
                id,
                event_type,
                text,
                tool,
                ..
            } => match event_type.as_str() {
                "assistant" => {
                    if let Some(t) = text {
                        let first = t.lines().next().unwrap_or("");
                        stdout.push(format!(
                            "  [{}] {}",
                            display_text(id),
                            truncate(&display_text(first), 120)
                        ));
                    }
                }
                "tool_use" => {
                    let name = tool.as_deref().unwrap_or("?");
                    let input = text.as_deref().unwrap_or("");
                    stdout.push(format!(
                        "  [{}] \x1b[2m[{}]\x1b[0m {}",
                        display_text(id),
                        display_text(name),
                        truncate(&display_text(input), 120)
                    ));
                }
                _ => {}
            },
            WatchEvent::CostThreshold {
                id,
                cost_usd,
                threshold,
                ..
            } => {
                stderr.push(format!(
                    "  \x1b[33m{}: ⚠ cost ${cost_usd:.2} > ${threshold:.2}\x1b[0m",
                    display_text(id)
                ));
            }
            WatchEvent::Completed {
                id,
                cost_usd,
                pr_url,
                ..
            } => {
                let cost = cost_usd
                    .map(|c| format!("${c:.2}"))
                    .unwrap_or_else(|| "-".into());
                let pr = pr_url.as_deref().unwrap_or("");
                stderr.push(format!(
                    "  \x1b[32m{}: ✓ done ({cost}) {}\x1b[0m",
                    display_text(id),
                    display_text(pr)
                ));
            }
            WatchEvent::Failed { id, subtype, .. } => {
                stderr.push(format!(
                    "  \x1b[31m{}: ✗ failed ({})\x1b[0m",
                    display_text(id),
                    display_text(subtype)
                ));
            }
            WatchEvent::SessionDied { id } => {
                stderr.push(format!(
                    "  \x1b[31m{}: ✗ session died\x1b[0m",
                    display_text(id)
                ));
            }
        },
    }

    (stdout, stderr)
}

/// Main entry point for `atc watch`.
pub async fn run_watch(
    config: &AtcConfig,
    registry: Arc<dyn Registry>,
    id: Option<&str>,
    all_running: bool,
    format: &str,
    socket: Option<PathBuf>,
) -> Result<()> {
    let output_format = match format {
        "ndjson" | "json" => OutputFormat::Ndjson,
        "human" => OutputFormat::Human,
        "pretty" => OutputFormat::Pretty,
        "auto" => {
            // Default to JSONL always — pipe through jq for formatting
            OutputFormat::Ndjson
        }
        other => {
            anyhow::bail!("unknown format: {other} (expected ndjson, json, pretty, human, or auto)")
        }
    };

    let poll_interval = std::time::Duration::from_secs(config.watch.poll_interval_secs);
    let cost_threshold = config.watch.cost_threshold;

    // If socket mode, set up a broadcast channel and listener
    let (tx, _) = broadcast::channel::<String>(1024);
    let tx_clone = tx.clone();

    if let Some(ref socket_path) = socket {
        // Remove existing socket
        let _ = std::fs::remove_file(socket_path);
        let listener = tokio::net::UnixListener::bind(socket_path)?;
        info!(path = %socket_path.display(), "listening on Unix socket");

        let tx_socket = tx.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let mut rx = tx_socket.subscribe();
                        tokio::spawn(async move {
                            let mut stream = stream;
                            while let Ok(line) = rx.recv().await {
                                let data = format!("{line}\n");
                                if stream.write_all(data.as_bytes()).await.is_err() {
                                    break;
                                }
                            }
                        });
                    }
                    Err(e) => {
                        warn!(error = %e, "socket accept failed");
                    }
                }
            }
        });
    }

    // Resolve dispatches to watch
    let records = if all_running {
        registry
            .list(StatusFilter::by_status(Status::Running))
            .await?
    } else if let Some(id) = id {
        match registry.get(id).await? {
            Some(r) => vec![r],
            None => anyhow::bail!("dispatch not found: {id}"),
        }
    } else {
        // Most recent running
        let all = registry
            .list(StatusFilter::by_status(Status::Running))
            .await?;
        if all.is_empty() {
            anyhow::bail!("no running dispatches found");
        }
        vec![all.into_iter().next().unwrap()]
    };

    if records.is_empty() {
        anyhow::bail!("no dispatches to watch");
    }

    // Initialize watchers
    let mut watchers: HashMap<String, DispatchWatcher> = HashMap::new();
    for record in &records {
        // Emit Started event
        let started = WatchEvent::Started {
            id: record.id.clone(),
            task: record.task_slug.clone(),
            directive: record.directive.as_str().to_string(),
            worktree: record.worktree_path.to_string_lossy().to_string(),
        };
        emit_event(&started, &output_format, &tx_clone);
        watchers.insert(
            record.id.clone(),
            DispatchWatcher::new(
                record.id.clone(),
                record.log_file.clone(),
                record.pr_urls.clone(),
            ),
        );
    }

    // Main poll loop
    loop {
        let mut all_done = true;

        for (id, watcher) in watchers.iter_mut() {
            if watcher.finalized {
                continue;
            }
            all_done = false;

            // Poll log for new events
            let events = watcher.poll_log(cost_threshold);
            for event in events {
                emit_event(&event, &output_format, &tx_clone);
            }

            // Check tmux session liveness
            let record = records.iter().find(|r| r.id == *id).unwrap();
            if !tmux_session_alive(&record.session).await && !watcher.finalized {
                // Wait a moment for final log lines to flush
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;

                // Final log poll
                let events = watcher.poll_log(cost_threshold);
                for event in events {
                    emit_event(&event, &output_format, &tx_clone);
                }

                // Re-read registry — only finalize if the dispatch is still non-terminal.
                // The tmux-side `atc post-complete` may have already succeeded, or the
                // user may have stopped the dispatch manually.
                let needs_finalize = registry
                    .get(id)
                    .await?
                    .is_some_and(|r| !r.status.is_terminal());

                if needs_finalize {
                    if !watcher.saw_result {
                        emit_event(
                            &WatchEvent::SessionDied { id: id.clone() },
                            &output_format,
                            &tx_clone,
                        );
                    }

                    // Trigger post-completion for non-terminal dispatch
                    let input = atc_core::post_completion::PostCompleteInput {
                        dispatch_id: id.clone(),
                        exit_code: None,
                        log_file: Some(watcher.log_file.clone()),
                        skip_cleanup: false,
                    };
                    if let Err(e) =
                        post_completion::run_post_completion(&input, registry.as_ref(), config)
                            .await
                    {
                        warn!(id = %id, error = %e, "post-completion failed for dead session");
                        continue;
                    }
                }

                watcher.finalized = true;
            }
        }

        if all_done {
            break;
        }

        tokio::time::sleep(poll_interval).await;
    }

    // Clean up socket
    if let Some(ref socket_path) = socket {
        let _ = std::fs::remove_file(socket_path);
    }

    Ok(())
}

fn emit_event(event: &WatchEvent, format: &OutputFormat, tx: &broadcast::Sender<String>) {
    let json = terminal_safe_json(event).unwrap_or_default();

    // Broadcast to socket consumers
    let _ = tx.send(json.clone());

    if matches!(format, OutputFormat::Ndjson) {
        println!("{json}");
        return;
    }

    let (stdout, stderr) = render_event_lines(event, format);
    for line in stdout {
        println!("{line}");
    }
    for line in stderr {
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(stdout: &[String], stderr: &[String]) -> String {
        stdout
            .iter()
            .chain(stderr.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn assert_no_raw_terminal_controls(output: &str) {
        assert!(!output.contains('\x1b'), "raw ESC in output: {output:?}");
        assert!(!output.contains('\x07'), "raw BEL in output: {output:?}");
        assert!(
            !output.contains('\u{202e}'),
            "raw bidi control in output: {output:?}"
        );
    }

    #[test]
    fn human_renderer_escapes_started_event_fields() {
        let event = WatchEvent::Started {
            id: "disp-\x1b[2J".to_string(),
            task: Some("tasks/evil\x07\u{202e}gpj.exe".to_string()),
            directive: "implement\x1b[31m".to_string(),
            worktree: "/tmp/worktree".to_string(),
        };

        let (stdout, stderr) = render_event_lines(&event, &OutputFormat::Human);
        let output = joined(&stdout, &stderr);

        assert_no_raw_terminal_controls(&output);
        assert!(output.contains("\\x1b"));
        assert!(output.contains("\\x07"));
        assert!(output.contains("\\u{202e}"));
    }

    #[test]
    fn human_renderer_escapes_log_line_payloads() {
        let assistant = WatchEvent::LogLine {
            id: "disp".to_string(),
            event_type: "assistant".to_string(),
            text: Some("first\x1b[2J\nsecond\x07\u{202e}".to_string()),
            tool: None,
        };
        let tool = WatchEvent::LogLine {
            id: "disp".to_string(),
            event_type: "tool_use".to_string(),
            text: Some("{\"cmd\":\"rm -rf /\x1b[31m\"}".to_string()),
            tool: Some("Bash\u{202e}".to_string()),
        };

        let (stdout_a, stderr_a) = render_event_lines(&assistant, &OutputFormat::Human);
        let (stdout_t, stderr_t) = render_event_lines(&tool, &OutputFormat::Human);
        let output = format!(
            "{}\n{}",
            joined(&stdout_a, &stderr_a),
            joined(&stdout_t, &stderr_t)
        );

        assert_no_raw_terminal_controls(&output);
        assert!(output.contains("\\x1b"));
        assert!(output.contains("\\x07"));
        assert!(output.contains("\\u{202e}"));
    }

    #[test]
    fn ndjson_renderer_escapes_bidi_but_preserves_decoded_values() {
        let event = WatchEvent::LogLine {
            id: "disp-\u{202e}gpj.exe".to_string(),
            event_type: "assistant".to_string(),
            text: Some("hello\u{202e}gpj.exe".to_string()),
            tool: None,
        };

        let (stdout, stderr) = render_event_lines(&event, &OutputFormat::Ndjson);

        assert!(stderr.is_empty());
        assert_eq!(stdout.len(), 1);
        assert!(!stdout[0].contains('\u{202e}'));
        assert!(stdout[0].contains("\\u202e"));

        let decoded: serde_json::Value = serde_json::from_str(&stdout[0]).unwrap();
        assert_eq!(decoded["id"], "disp-\u{202e}gpj.exe");
        assert_eq!(decoded["text"], "hello\u{202e}gpj.exe");
    }

    #[test]
    fn emit_event_broadcasts_terminal_safe_json() {
        let event = WatchEvent::Failed {
            id: "disp-\u{202e}gpj.exe".to_string(),
            status: "failed".to_string(),
            subtype: "error-\u{202e}".to_string(),
        };
        let (tx, mut rx) = broadcast::channel(4);
        let _subscription = tx.subscribe();

        emit_event(&event, &OutputFormat::Ndjson, &tx);

        let json = rx.try_recv().unwrap();
        assert!(!json.contains('\u{202e}'));
        assert!(json.contains("\\u202e"));

        let decoded: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded["id"], "disp-\u{202e}gpj.exe");
        assert_eq!(decoded["subtype"], "error-\u{202e}");
    }

    #[test]
    fn pretty_renderer_escapes_user_bidi_inside_colored_lines() {
        let event = WatchEvent::Failed {
            id: "disp-\u{202e}gpj.exe".to_string(),
            status: "failed".to_string(),
            subtype: "error-\u{202e}".to_string(),
        };

        let (stdout, stderr) = render_event_lines(&event, &OutputFormat::Pretty);
        let output = joined(&stdout, &stderr);

        assert!(!output.contains('\u{202e}'));
        assert!(output.contains("\\u{202e}"));
    }
}
