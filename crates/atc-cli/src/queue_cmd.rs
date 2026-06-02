//! `atc queue` command — view, drain, and manage dispatch queues.

use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::executor::AgentExecutor;
use atc_core::queue::{DispatchQueue, Priority, QueueInputType, QueueRow};
use atc_core::registry::Registry;
use atc_core::types::RunOpts;
use tracing::{error, info, warn};

/// Show queue contents.
pub async fn run_queue_list(queue: &dyn DispatchQueue, queue_name: &str) -> Result<()> {
    let items = queue.queue_list(queue_name).await?;
    if items.is_empty() {
        println!("Queue '{}' is empty.", queue_name);
        return Ok(());
    }

    println!(
        "Queue '{}' — {} pending item(s):\n",
        queue_name,
        items.len()
    );
    println!(
        "{:<24} {:<10} {:<10} {:<8} INPUT",
        "ID", "TYPE", "PRIORITY", "STATUS"
    );
    println!("{}", "-".repeat(80));
    for item in &items {
        let priority_str = Priority::from_i32(item.priority)
            .map(|p| p.as_str())
            .unwrap_or("unknown");
        // Truncate long input for display
        let input_display = if item.input_value.chars().count() > 40 {
            format!(
                "{}...",
                item.input_value.chars().take(37).collect::<String>()
            )
        } else {
            item.input_value.clone()
        };
        println!(
            "{:<24} {:<10} {:<10} {:<8} {}",
            &item.id[..item.id.len().min(24)],
            item.input_type.as_str(),
            priority_str,
            item.status.as_str(),
            input_display,
        );
    }
    Ok(())
}

/// Clear all pending items from a queue.
pub async fn run_queue_clear(queue: &dyn DispatchQueue, queue_name: &str) -> Result<()> {
    let count = queue.queue_clear(queue_name).await?;
    println!(
        "Cleared {} pending item(s) from queue '{}'.",
        count, queue_name
    );
    Ok(())
}

/// One-shot drain: dispatch all pending items in priority order, then exit.
pub async fn run_queue_drain(
    queue: &dyn DispatchQueue,
    registry: &dyn Registry,
    executor: &dyn AgentExecutor,
    config: &AtcConfig,
    queue_name: &str,
) -> Result<()> {
    // Recover any stale dispatching rows first
    let (recovered, completed) = queue.queue_recover(&[queue_name]).await?;
    if recovered > 0 || completed > 0 {
        info!(
            recovered,
            completed, "recovered stale queue items on drain start"
        );
    }

    let mut dispatched = 0u32;
    let mut failed = 0u32;

    loop {
        let items = queue.queue_peek(queue_name, 10).await?;
        if items.is_empty() {
            break;
        }

        for item in items {
            // Claim
            let claim_token = match queue.queue_claim(&item.id).await? {
                Some(token) => token,
                None => continue, // someone else claimed it
            };

            let label = crate::daemon::log_label(&item);

            // Convert to RunOpts and dispatch
            match dispatch_queue_item(&item, registry, executor, config).await {
                Ok(dispatch_id) => {
                    // Persist dispatch_id while still 'dispatching' for crash recovery
                    if let Err(e) = queue
                        .queue_set_dispatch_id(&item.id, &claim_token, &dispatch_id)
                        .await
                    {
                        warn!(
                            queue_id = %item.id,
                            error = %e,
                            "failed to persist dispatch_id for crash recovery"
                        );
                    }
                    if let Err(e) = queue
                        .queue_mark_dispatched(&item.id, &claim_token, &dispatch_id)
                        .await
                    {
                        error!(
                            queue_id = %item.id,
                            error = %e,
                            "failed to mark dispatched"
                        );
                    }
                    dispatched += 1;
                    info!(
                        queue_id = %item.id,
                        dispatch_id = %dispatch_id,
                        input = %label,
                        "dispatched from queue"
                    );
                }
                Err(e) => {
                    let err_msg = format!("{:#}", e);
                    if let Err(mark_err) = queue
                        .queue_mark_failed(&item.id, &claim_token, &err_msg)
                        .await
                    {
                        warn!(
                            queue_id = %item.id,
                            error = %mark_err,
                            "failed to mark item as failed in queue"
                        );
                    }
                    failed += 1;
                    warn!(
                        queue_id = %item.id,
                        input = %label,
                        error = %err_msg,
                        "queue dispatch failed"
                    );
                }
            }
        }
    }

    println!(
        "Drain complete: {} dispatched, {} failed.",
        dispatched, failed
    );
    Ok(())
}

/// Convert a queue row to RunOpts and dispatch through the pipeline.
pub async fn dispatch_queue_item(
    row: &QueueRow,
    registry: &dyn Registry,
    executor: &dyn AgentExecutor,
    config: &AtcConfig,
) -> Result<String> {
    let params: std::collections::HashMap<String, String> = row
        .params
        .as_ref()
        .map(|p| {
            serde_json::from_str(p).unwrap_or_else(|e| {
                warn!(queue_id = %row.id, error = %e, "failed to parse params JSON, using empty map");
                std::collections::HashMap::new()
            })
        })
        .unwrap_or_default();

    // Decide force_task from the row's input_type, not from the payload text
    let (raw_input, force_task) = match row.input_type {
        QueueInputType::Task => (row.input_value.clone(), true),
        QueueInputType::Template | QueueInputType::Prompt => (row.input_value.clone(), false),
    };

    let opts = RunOpts {
        input: raw_input.clone(),
        directive: match &row.mode {
            Some(m) => match m.parse() {
                Ok(d) => Some(d),
                Err(_) => {
                    warn!(queue_id = %row.id, mode = %m, "invalid directive/mode, ignoring");
                    None
                }
            },
            None => None,
        },
        params,
        pr_url: None,
        repos: vec![],
        inline: false,
        force: false,
        dry_run: false,
        directives: None,
        no_worktree: false,
        max_budget_usd: None,
        max_turns: None,
        resume: None,
        retries: 0,
        list: false,
        ephemeral: false,
        timeout: None,
        json: false,
    };

    // Build resolver chain
    let all_resolvers = crate::resolvers::build_resolvers(config);
    let resolvers_to_use = if force_task {
        let filtered: Vec<_> = all_resolvers
            .into_iter()
            .filter(|r| r.name() == "task")
            .collect();
        if filtered.is_empty() {
            anyhow::bail!("queue item requires task resolver but none is configured");
        }
        filtered
    } else {
        all_resolvers
    };

    let pipeline = crate::pipeline::DispatchPipeline {
        resolvers: resolvers_to_use,
        config,
        registry,
        executor,
    };

    let outcome = pipeline.execute(&raw_input, &opts).await?;
    Ok(outcome.id)
}
