//! `atc daemon` — long-running process that drains queues and runs sources.
//!
//! Architecture:
//! - QueueDrainer: polls queue, dispatches through pipeline, health-checks running dispatches
//! - Sources: pluggable selection strategies that feed queues on a timer
//! - SignalHandler: SIGTERM/SIGINT for graceful shutdown, SIGHUP for config reload

use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::executor::AgentExecutor;
use atc_core::queue::DispatchQueue;
use atc_core::registry::{Registry, StatusFilter};
use atc_core::source::SourceConfig;
use atc_core::types::Status;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;
use tracing::{error, info, warn};

/// Options for `atc daemon`.
#[derive(Debug)]
pub struct DaemonOpts {
    pub queues: Vec<String>,
    pub max_concurrent: usize,
    pub sources: Vec<String>,
    pub detach: bool,
}

/// Daemon state.
struct DaemonState {
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<Notify>,
}

/// Run the daemon.
pub async fn run_daemon(
    registry: Arc<atc_core::registry::SqliteRegistry>,
    executor: Arc<dyn AgentExecutor>,
    config: &AtcConfig,
    opts: &DaemonOpts,
) -> Result<()> {
    let queue = registry.clone();
    let state = DaemonState {
        shutdown: Arc::new(AtomicBool::new(false)),
        shutdown_notify: Arc::new(Notify::new()),
    };

    // PID file
    let pid_file = config.daemon.resolved_pid_file();
    write_pid_file(&pid_file)?;
    info!(pid_file = %pid_file.display(), "daemon starting");

    // Recover stale queue items
    let (recovered, completed) = queue.queue_recover().await?;
    if recovered > 0 || completed > 0 {
        info!(recovered, completed, "recovered stale queue items");
    }

    // Setup signal handler
    let shutdown = state.shutdown.clone();
    let notify = state.shutdown_notify.clone();
    tokio::spawn(async move {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        let mut sigint =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap();

        tokio::select! {
            _ = sigterm.recv() => {
                info!("received SIGTERM, initiating graceful shutdown");
            }
            _ = sigint.recv() => {
                info!("received SIGINT, initiating graceful shutdown");
            }
        }

        shutdown.store(true, Ordering::SeqCst);
        notify.notify_waiters();
    });

    // Start sources
    let source_handles =
        start_sources(config, &opts.sources, queue.clone(), state.shutdown.clone()).await;

    let drain_interval = std::time::Duration::from_secs(config.daemon.drain_interval_secs);
    let queues = if opts.queues.is_empty() {
        vec!["default".to_string()]
    } else {
        opts.queues.clone()
    };

    println!(
        "Daemon running (queues: {}, max_concurrent: {}, sources: {})",
        queues.join(", "),
        opts.max_concurrent,
        if opts.sources.is_empty() {
            "none".to_string()
        } else {
            opts.sources.join(", ")
        }
    );

    // Main drain loop
    while !state.shutdown.load(Ordering::SeqCst) {
        // Health check running dispatches
        let running = registry
            .list(StatusFilter::any(vec![Status::Running, Status::Retrying]))
            .await
            .unwrap_or_default();
        let active_count = running.len();

        // Calculate available slots
        let available_slots = opts.max_concurrent.saturating_sub(active_count);

        if available_slots > 0 {
            // Drain from each queue
            for queue_name in &queues {
                if state.shutdown.load(Ordering::SeqCst) {
                    break;
                }

                let items = match queue.queue_peek(queue_name, available_slots as u32).await {
                    Ok(items) => items,
                    Err(e) => {
                        warn!(queue = %queue_name, error = %e, "failed to peek queue");
                        continue;
                    }
                };

                for item in items {
                    if state.shutdown.load(Ordering::SeqCst) {
                        break;
                    }

                    if !queue.queue_claim(&item.id).await.unwrap_or(false) {
                        continue;
                    }

                    match crate::queue_cmd::dispatch_queue_item(
                        &item,
                        queue.as_ref(),
                        queue.as_ref(), // Registry
                        executor.as_ref(),
                        config,
                    )
                    .await
                    {
                        Ok(dispatch_id) => {
                            if let Err(e) =
                                queue.queue_mark_dispatched(&item.id, &dispatch_id).await
                            {
                                error!(
                                    queue_id = %item.id,
                                    error = %e,
                                    "failed to mark dispatched"
                                );
                            }
                            info!(
                                queue_id = %item.id,
                                dispatch_id = %dispatch_id,
                                input = %item.input_value,
                                "dispatched from daemon"
                            );
                        }
                        Err(e) => {
                            let err_msg = format!("{:#}", e);
                            let _ = queue.queue_mark_failed(&item.id, &err_msg).await;
                            warn!(
                                queue_id = %item.id,
                                input = %item.input_value,
                                error = %err_msg,
                                "daemon dispatch failed"
                            );
                        }
                    }
                }
            }
        }

        // Wait for drain interval or shutdown signal
        tokio::select! {
            _ = tokio::time::sleep(drain_interval) => {}
            _ = state.shutdown_notify.notified() => {}
        }
    }

    // Graceful shutdown
    info!("shutting down daemon...");

    // Wait for source tasks to finish
    for handle in source_handles {
        let _ = handle.await;
    }

    // Wait for running agents (with timeout)
    let timeout = std::time::Duration::from_secs(config.daemon.graceful_shutdown_timeout_secs);
    let start = std::time::Instant::now();
    loop {
        let running = registry
            .list(StatusFilter::by_status(Status::Running))
            .await
            .unwrap_or_default();
        if running.is_empty() || start.elapsed() >= timeout {
            if !running.is_empty() {
                warn!(
                    count = running.len(),
                    "shutdown timeout reached with running agents"
                );
            }
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    // Clean up PID file
    let _ = std::fs::remove_file(&pid_file);
    info!("daemon stopped");
    println!("Daemon stopped.");
    Ok(())
}

/// Start source tasks.
async fn start_sources(
    config: &AtcConfig,
    source_names: &[String],
    queue: Arc<atc_core::registry::SqliteRegistry>,
    shutdown: Arc<AtomicBool>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();

    for name in source_names {
        let source_config = match config.sources.get(name) {
            Some(c) => c.clone(),
            None => {
                // Built-in source names
                match name.as_str() {
                    "ready" => SourceConfig::Ready(atc_core::source::ReadySourceConfig {
                        poll_interval_secs: 10,
                        limit: 3,
                        queue: "default".to_string(),
                    }),
                    "board" => SourceConfig::Board(atc_core::source::BoardSourceConfig {
                        poll_interval_secs: 10,
                        queue: "default".to_string(),
                        filter_status: vec!["ready".to_string()],
                        exclude_tags: vec![],
                        require_unassigned: true,
                        require_unblocked: true,
                        view: None,
                        filter_type: None,
                        filter_priority: None,
                        filter_container: None,
                    }),
                    "events" => SourceConfig::Events(atc_core::source::EventsSourceConfig {
                        poll_interval_secs: 5,
                        queue: "default".to_string(),
                        filter: Some("document:updated".to_string()),
                        path: Some("tasks/".to_string()),
                        trigger_on_status: vec!["ready".to_string()],
                    }),
                    _ => {
                        warn!(source = %name, "unknown source, skipping");
                        continue;
                    }
                }
            }
        };

        let queue = queue.clone();
        let shutdown = shutdown.clone();
        let name = name.clone();
        let poll_interval = std::time::Duration::from_secs(source_config.poll_interval_secs());
        let source_queue = source_config.queue().to_string();

        let handle = tokio::spawn(async move {
            info!(source = %name, "source started");
            while !shutdown.load(Ordering::SeqCst) {
                if let Err(e) =
                    run_source_iteration(&name, &source_config, &source_queue, queue.as_ref()).await
                {
                    warn!(source = %name, error = %e, "source iteration failed");
                }
                tokio::time::sleep(poll_interval).await;
            }
            info!(source = %name, "source stopped");
        });
        handles.push(handle);
    }

    handles
}

/// Run a single iteration of a source.
async fn run_source_iteration(
    name: &str,
    config: &SourceConfig,
    queue_name: &str,
    queue: &dyn DispatchQueue,
) -> Result<()> {
    match config {
        SourceConfig::Ready(cfg) => {
            let output = tokio::process::Command::new("git")
                .args([
                    "kb",
                    "ready",
                    "--limit",
                    &cfg.limit.to_string(),
                    "--format",
                    "slugs",
                ])
                .output()
                .await?;

            if !output.status.success() {
                anyhow::bail!(
                    "git kb ready failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            for slug in stdout.lines().filter(|l| !l.trim().is_empty()) {
                let slug = slug.trim();
                let item = atc_core::queue::EnqueueItem {
                    queue_name: queue_name.to_string(),
                    input_type: atc_core::queue::QueueInputType::Task,
                    input_value: slug.to_string(),
                    mode: None,
                    priority: atc_core::queue::Priority::Medium,
                    params: None,
                    enqueued_by: Some(format!("source:{}", name)),
                };
                let _ = queue.enqueue(item).await;
            }
        }
        SourceConfig::Board(cfg) => {
            let mut args = vec!["kb", "list", "--type", "task", "--format", "slugs"];
            let status_args: Vec<String>;

            if !cfg.filter_status.is_empty() {
                status_args = cfg.filter_status.clone();
                for s in &status_args {
                    args.push("--status");
                    args.push(s);
                }
            }
            if cfg.require_unblocked {
                args.push("--unblocked");
            }
            if cfg.require_unassigned {
                args.push("--unassigned");
            }

            let cmd = if let Some(ref view) = cfg.view {
                let mut v_args = vec!["kb", "view"];
                v_args.push(view);
                v_args.push("--format");
                v_args.push("slugs");
                v_args
            } else {
                args
            };

            let output = tokio::process::Command::new("git")
                .args(&cmd)
                .output()
                .await?;

            if !output.status.success() {
                anyhow::bail!(
                    "board source failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            for slug in stdout.lines().filter(|l| !l.trim().is_empty()) {
                let slug = slug.trim();
                let item = atc_core::queue::EnqueueItem {
                    queue_name: queue_name.to_string(),
                    input_type: atc_core::queue::QueueInputType::Task,
                    input_value: slug.to_string(),
                    mode: None,
                    priority: atc_core::queue::Priority::Medium,
                    params: None,
                    enqueued_by: Some(format!("source:{}", name)),
                };
                let _ = queue.enqueue(item).await;
            }
        }
        SourceConfig::Events(cfg) => {
            // Subscribe to git kb events — one poll reads recent events
            let mut args = vec!["kb", "events", "--format", "json", "--since", "10s"];
            if let Some(ref path) = cfg.path {
                args.push("--path");
                args.push(path);
            }

            let output = tokio::process::Command::new("git")
                .args(&args)
                .output()
                .await?;

            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    if let Ok(event) = serde_json::from_str::<serde_json::Value>(line) {
                        if let Some(slug) = event.get("slug").and_then(|s| s.as_str()) {
                            let status = event.get("status").and_then(|s| s.as_str()).unwrap_or("");
                            if cfg.trigger_on_status.is_empty()
                                || cfg.trigger_on_status.iter().any(|s| s == status)
                            {
                                let item = atc_core::queue::EnqueueItem {
                                    queue_name: queue_name.to_string(),
                                    input_type: atc_core::queue::QueueInputType::Task,
                                    input_value: slug.to_string(),
                                    mode: None,
                                    priority: atc_core::queue::Priority::Medium,
                                    params: None,
                                    enqueued_by: Some(format!("source:{}", name)),
                                };
                                let _ = queue.enqueue(item).await;
                            }
                        }
                    }
                }
            }
        }
        SourceConfig::Script(cfg) => {
            let output = tokio::process::Command::new("sh")
                .args(["-c", &cfg.command])
                .output()
                .await?;

            if !output.status.success() {
                anyhow::bail!(
                    "script source failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }

                let (input_type, input_value) = if let Some(rest) = trimmed.strip_prefix("task ") {
                    (atc_core::queue::QueueInputType::Task, rest.to_string())
                } else if let Some(rest) = trimmed.strip_prefix("prompt ") {
                    (atc_core::queue::QueueInputType::Prompt, rest.to_string())
                } else {
                    (atc_core::queue::QueueInputType::Task, trimmed.to_string())
                };

                let item = atc_core::queue::EnqueueItem {
                    queue_name: queue_name.to_string(),
                    input_type,
                    input_value,
                    mode: None,
                    priority: atc_core::queue::Priority::Medium,
                    params: None,
                    enqueued_by: Some(format!("source:{}", name)),
                };
                let _ = queue.enqueue(item).await;
            }
        }
    }

    Ok(())
}

/// Write PID file (checking for duplicates).
fn write_pid_file(path: &PathBuf) -> Result<()> {
    // Check for existing daemon
    if path.exists() {
        if let Ok(contents) = std::fs::read_to_string(path) {
            if let Ok(pid) = contents.trim().parse::<u32>() {
                // Check if process is alive
                let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
                if alive {
                    anyhow::bail!(
                        "daemon already running (PID {}). Use 'atc daemon stop' first.",
                        pid
                    );
                }
            }
        }
        // Stale PID file — remove it
        let _ = std::fs::remove_file(path);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}", std::process::id()))?;
    Ok(())
}

/// Stop a running daemon.
pub fn stop_daemon(config: &AtcConfig) -> Result<()> {
    let pid_file = config.daemon.resolved_pid_file();
    if !pid_file.exists() {
        println!("No daemon running (PID file not found).");
        return Ok(());
    }

    let contents = std::fs::read_to_string(&pid_file)?;
    let pid: i32 = contents
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid PID in {}", pid_file.display()))?;

    // Send SIGTERM
    let result = unsafe { libc::kill(pid, libc::SIGTERM) };
    if result == 0 {
        println!("Sent SIGTERM to daemon (PID {}).", pid);
        // Wait a moment and clean up
        std::thread::sleep(std::time::Duration::from_millis(500));
        let _ = std::fs::remove_file(&pid_file);
    } else {
        // Process doesn't exist — clean up stale PID file
        let _ = std::fs::remove_file(&pid_file);
        println!("Daemon not running (stale PID file cleaned up).");
    }
    Ok(())
}

/// Show daemon status.
pub async fn daemon_status(
    config: &AtcConfig,
    queue: &atc_core::registry::SqliteRegistry,
    registry: &dyn Registry,
    queues: &[String],
) -> Result<()> {
    let pid_file = config.daemon.resolved_pid_file();
    let daemon_running = if pid_file.exists() {
        if let Ok(contents) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = contents.trim().parse::<u32>() {
                unsafe { libc::kill(pid as i32, 0) == 0 }
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };

    println!(
        "Daemon: {}",
        if daemon_running { "running" } else { "stopped" }
    );

    if daemon_running {
        if let Ok(contents) = std::fs::read_to_string(&pid_file) {
            println!("PID:    {}", contents.trim());
        }
    }

    // Queue depths
    let queue_names = if queues.is_empty() {
        vec!["default".to_string()]
    } else {
        queues.to_vec()
    };

    for qn in &queue_names {
        let count = queue.queue_pending_count(qn).await.unwrap_or(0);
        println!("Queue '{}': {} pending", qn, count);
    }

    // Active dispatches
    let running = registry
        .list(StatusFilter::any(vec![Status::Running, Status::Retrying]))
        .await
        .unwrap_or_default();
    println!("Active dispatches: {}", running.len());
    println!("Max concurrent:    {}", config.daemon.max_concurrent);
    println!(
        "Available slots:   {}",
        config.daemon.max_concurrent.saturating_sub(running.len())
    );

    Ok(())
}
