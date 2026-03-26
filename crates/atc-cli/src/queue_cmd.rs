//! `atc queue` command — view, drain, and manage dispatch queues.

use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::executor::AgentExecutor;
use atc_core::queue::{DispatchQueue, Priority, QueueInputType, QueueRow};
use atc_core::registry::Registry;
use atc_core::types::RunOpts;
use tracing::{info, warn};

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
        let input_display = if item.input_value.len() > 40 {
            format!("{}...", &item.input_value[..37])
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
            if !queue.queue_claim(&item.id).await? {
                continue; // someone else claimed it
            }

            let label = crate::daemon::log_label(&item);

            // Convert to RunOpts and dispatch
            match dispatch_queue_item(&item, queue, registry, executor, config).await {
                Ok(dispatch_id) => {
                    queue.queue_mark_dispatched(&item.id, &dispatch_id).await?;
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
                    queue.queue_mark_failed(&item.id, &err_msg).await?;
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
    _queue: &dyn DispatchQueue,
    registry: &dyn Registry,
    executor: &dyn AgentExecutor,
    config: &AtcConfig,
) -> Result<String> {
    // Build input string for the resolver chain
    let input = match row.input_type {
        QueueInputType::Task => format!("task {}", row.input_value),
        QueueInputType::Template => row.input_value.clone(),
        QueueInputType::Prompt => row.input_value.clone(),
    };

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

    // Determine if first word is "task" to force task resolver
    let (raw_input, force_task) = if let Some(rest) = input.strip_prefix("task ") {
        (rest.to_string(), true)
    } else {
        (input.clone(), false)
    };

    let opts = RunOpts {
        input: raw_input.clone(),
        directive: row.mode.as_ref().and_then(|m| m.parse().ok()),
        params,
        pr_url: None,
        inline: false,
        force: false,
        dry_run: false,
        directives: None,
        no_worktree: false,
        max_budget_usd: None,
        max_turns: None,
        retries: 0,
        list: false,
    };

    // Build resolver chain
    let all_resolvers = crate::resolvers::build_resolvers(config);
    let resolvers_to_use = if force_task {
        all_resolvers
            .into_iter()
            .filter(|r| r.name() == "task")
            .collect()
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
