//! `atc watch` — Watch running agent sessions and emit structured events.
//!
//! Monitors tmux session liveness, tails log files for new events, and emits
//! structured NDJSON events for consumption by AI harnesses or human terminals.

use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::post_completion;
use atc_core::registry::{Registry, StatusFilter};
use atc_core::stream_json;
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
        mode: String,
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
    lines_read: usize,
    cost_threshold_fired: bool,
    completed: bool,
}

impl DispatchWatcher {
    fn new(id: String, log_file: PathBuf) -> Self {
        Self {
            id,
            log_file,
            lines_read: 0,
            cost_threshold_fired: false,
            completed: false,
        }
    }

    /// Read new lines from the log file and emit events.
    fn poll_log(&mut self, cost_threshold: f64) -> Vec<WatchEvent> {
        let mut events = Vec::new();

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
                        self.completed = true;
                        if r.subtype == "success" {
                            events.push(WatchEvent::Completed {
                                id: self.id.clone(),
                                status: "done".to_string(),
                                cost_usd: r.total_cost_usd,
                                pr_url: None, // Would need artifact extraction
                                summary: None,
                            });
                        } else {
                            events.push(WatchEvent::Failed {
                                id: self.id.clone(),
                                status: "failed".to_string(),
                                subtype: r.subtype.clone(),
                            });
                        }

                        // Check cost threshold
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
                    }
                    stream_json::StreamEvent::Skip => {}
                }
            }
        }

        events
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
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
    Ndjson,
    Human,
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
        "ndjson" => OutputFormat::Ndjson,
        "human" => OutputFormat::Human,
        "auto" => {
            if atty::is(atty::Stream::Stdout) {
                OutputFormat::Human
            } else {
                OutputFormat::Ndjson
            }
        }
        other => anyhow::bail!("unknown format: {other} (expected ndjson, human, or auto)"),
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
            mode: record.mode.as_str().to_string(),
            worktree: record.worktree_path.to_string_lossy().to_string(),
        };
        emit_event(&started, &output_format, &tx_clone);
        watchers.insert(
            record.id.clone(),
            DispatchWatcher::new(record.id.clone(), record.log_file.clone()),
        );
    }

    // Main poll loop
    loop {
        let mut all_done = true;

        for (id, watcher) in watchers.iter_mut() {
            if watcher.completed {
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
            if !tmux_session_alive(&record.session).await {
                if !watcher.completed {
                    // Wait a moment for final log lines to flush
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

                    // Final log poll
                    let events = watcher.poll_log(cost_threshold);
                    for event in events {
                        emit_event(&event, &output_format, &tx_clone);
                    }

                    if !watcher.completed {
                        // Session died without result event — run post-completion
                        emit_event(
                            &WatchEvent::SessionDied { id: id.clone() },
                            &output_format,
                            &tx_clone,
                        );

                        // Trigger post-completion
                        let input = atc_core::post_completion::PostCompleteInput {
                            dispatch_id: id.clone(),
                            exit_code: None,
                            log_file: Some(watcher.log_file.clone()),
                        };
                        if let Err(e) =
                            post_completion::run_post_completion(&input, registry.as_ref(), config)
                                .await
                        {
                            warn!(id = %id, error = %e, "post-completion failed for dead session");
                        }
                    }

                    watcher.completed = true;
                }
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
    let json = serde_json::to_string(event).unwrap_or_default();

    // Broadcast to socket consumers
    let _ = tx.send(json.clone());

    match format {
        OutputFormat::Ndjson => {
            println!("{json}");
        }
        OutputFormat::Human => match event {
            WatchEvent::Started { id, task, mode, .. } => {
                let label = task.as_deref().unwrap_or(id);
                eprintln!("▶ watching {label} ({mode})");
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
                            println!(">>> {line}");
                        }
                    }
                }
                "tool_use" => {
                    let tool_name = tool.as_deref().unwrap_or("?");
                    let input = text.as_deref().unwrap_or("");
                    println!("  [{tool_name}] {input}");
                }
                _ => {}
            },
            WatchEvent::CostThreshold {
                id,
                cost_usd,
                threshold,
            } => {
                eprintln!("⚠ {id}: cost ${cost_usd:.2} exceeds ${threshold:.2} threshold");
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
                eprintln!("✅ {id}: done ({cost}) {pr}");
            }
            WatchEvent::Failed { id, subtype, .. } => {
                eprintln!("❌ {id}: failed ({subtype})");
            }
            WatchEvent::SessionDied { id } => {
                eprintln!("💀 {id}: session died without result event");
            }
        },
    }
}
