//! `atc enqueue` command — add work to a dispatch queue.
//!
//! Supports explicit input (task, prompt, template) and delegated selection
//! (--ready, --board, --view, --stdin).

use anyhow::Result;
use atc_core::queue::{DispatchQueue, EnqueueItem, EnqueueResult, Priority, QueueInputType};
use std::io::BufRead;

/// Timeout for git subprocess calls.
const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// CLI options for `atc enqueue`.
#[derive(Debug)]
pub struct EnqueueOpts {
    /// Positional input: "task <slug>", "<template> --param k=v", or raw prompt.
    pub input: Vec<String>,
    /// Target queue name.
    pub queue: String,
    /// Priority override.
    pub priority: Priority,
    /// Directive/mode override.
    pub mode: Option<String>,
    /// Key=value pairs for template params.
    pub params: std::collections::HashMap<String, String>,
    /// Delegated selection: use kb_ready scoring.
    pub ready: bool,
    /// Limit for --ready.
    pub limit: u32,
    /// Delegated selection: board query.
    pub board: bool,
    /// Board filter: --status.
    pub status_filter: Option<String>,
    /// Board filter: --unblocked.
    pub unblocked: bool,
    /// Board filter: --unassigned.
    pub unassigned: bool,
    /// Delegated selection: saved view.
    pub view: Option<String>,
    /// Read slugs from stdin.
    pub stdin: bool,
    /// Source label (who enqueued).
    pub enqueued_by: String,
}

/// Run the `atc enqueue` command.
pub async fn run_enqueue(
    queue: &(dyn DispatchQueue + Send + Sync),
    opts: &EnqueueOpts,
) -> Result<()> {
    // Reject ambiguous mode combinations
    let delegated = u8::from(opts.ready)
        + u8::from(opts.board)
        + u8::from(opts.view.is_some())
        + u8::from(opts.stdin);
    let has_explicit_input = !opts.input.is_empty();
    if delegated > 1 || (delegated > 0 && has_explicit_input) {
        anyhow::bail!(
            "choose exactly one of positional input, --ready, --board, --view, or --stdin"
        );
    }

    let mut total_enqueued = 0u32;
    let mut total_skipped = 0u32;

    if opts.ready {
        // Delegated selection: kb_ready
        let slugs = kb_ready_slugs(opts.limit).await?;
        if slugs.is_empty() {
            println!("No ready tasks found.");
            return Ok(());
        }
        for slug in &slugs {
            let result = enqueue_one(queue, opts, QueueInputType::Task, slug).await?;
            count_result(&result, &mut total_enqueued, &mut total_skipped);
            print_result(slug, &result);
        }
    } else if opts.board {
        // Delegated selection: board query
        let slugs = board_query_slugs(opts).await?;
        if slugs.is_empty() {
            println!("No matching tasks found.");
            return Ok(());
        }
        for slug in &slugs {
            let result = enqueue_one(queue, opts, QueueInputType::Task, slug).await?;
            count_result(&result, &mut total_enqueued, &mut total_skipped);
            print_result(slug, &result);
        }
    } else if let Some(ref view_slug) = opts.view {
        // Delegated selection: saved view
        let slugs = view_query_slugs(view_slug).await?;
        if slugs.is_empty() {
            println!("No results from view '{}'.", view_slug);
            return Ok(());
        }
        for slug in &slugs {
            let result = enqueue_one(queue, opts, QueueInputType::Task, slug).await?;
            count_result(&result, &mut total_enqueued, &mut total_skipped);
            print_result(slug, &result);
        }
    } else if opts.stdin {
        // Read slugs from stdin
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let (input_type, input_value) = parse_input_line(trimmed);
            let result = enqueue_one(queue, opts, input_type, &input_value).await?;
            count_result(&result, &mut total_enqueued, &mut total_skipped);
            print_result(&input_value, &result);
        }
    } else {
        // Explicit input
        if opts.input.is_empty() {
            anyhow::bail!("input required: provide a task slug, template name, prompt, or use --ready/--board/--view/--stdin");
        }
        let raw = opts.input.join(" ");
        let (input_type, input_value) = parse_input_line(&raw);
        let result = enqueue_one(queue, opts, input_type, &input_value).await?;
        count_result(&result, &mut total_enqueued, &mut total_skipped);
        print_result(&input_value, &result);
    }

    if total_enqueued + total_skipped > 1 {
        println!(
            "\nTotal: {} enqueued, {} skipped (dedup)",
            total_enqueued, total_skipped
        );
    }

    Ok(())
}

/// Parse a single input line into (type, value).
fn parse_input_line(line: &str) -> (QueueInputType, String) {
    let parts: Vec<&str> = line.splitn(2, ' ').collect();
    match parts.first().copied() {
        Some("task") if parts.len() > 1 => (QueueInputType::Task, parts[1].to_string()),
        Some("prompt") if parts.len() > 1 => (QueueInputType::Prompt, parts[1].to_string()),
        // Bare "task" or "prompt" without a value is an error — treat as prompt text
        Some("task") | Some("prompt") if parts.len() == 1 => {
            (QueueInputType::Prompt, line.to_string())
        }
        _ => {
            // If it looks like a slug (contains /), treat as task
            if line.contains('/') && !line.contains(' ') {
                (QueueInputType::Task, line.to_string())
            } else if !line.contains(' ') && !line.contains('/') {
                // Could be a template name
                (QueueInputType::Template, line.to_string())
            } else {
                (QueueInputType::Prompt, line.to_string())
            }
        }
    }
}

/// Enqueue a single item.
async fn enqueue_one(
    queue: &(dyn DispatchQueue + Send + Sync),
    opts: &EnqueueOpts,
    input_type: QueueInputType,
    input_value: &str,
) -> Result<EnqueueResult> {
    let params_json = if opts.params.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&opts.params)?)
    };

    let item = EnqueueItem {
        queue_name: opts.queue.clone(),
        input_type,
        input_value: input_value.to_string(),
        mode: opts.mode.clone(),
        priority: opts.priority,
        params: params_json,
        enqueued_by: Some(opts.enqueued_by.clone()),
    };

    queue.enqueue(item).await
}

fn count_result(result: &EnqueueResult, enqueued: &mut u32, skipped: &mut u32) {
    match result {
        EnqueueResult::Enqueued { .. } => *enqueued += 1,
        EnqueueResult::Skipped(_) => *skipped += 1,
    }
}

fn print_result(input: &str, result: &EnqueueResult) {
    match result {
        EnqueueResult::Enqueued { id } => {
            println!("Enqueued: {} (id: {})", input, id);
        }
        EnqueueResult::Skipped(reason) => {
            println!("Skipped:  {} ({})", input, reason);
        }
    }
}

/// Run a git subprocess with timeout and kill_on_drop.
async fn run_git_cmd(args: &[&str]) -> Result<std::process::Output> {
    let output = tokio::time::timeout(
        CMD_TIMEOUT,
        tokio::process::Command::new("git")
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "git {} timed out after {}s",
            args.join(" "),
            CMD_TIMEOUT.as_secs()
        )
    })??;
    Ok(output)
}

/// Query `git kb ready` for top-scored task slugs.
async fn kb_ready_slugs(limit: u32) -> Result<Vec<String>> {
    let limit_str = limit.to_string();
    let output = run_git_cmd(&["kb", "ready", "--limit", &limit_str, "--format", "slugs"]).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git kb ready failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// Query `git kb list` for tasks matching board filters.
async fn board_query_slugs(opts: &EnqueueOpts) -> Result<Vec<String>> {
    let mut args = vec!["kb", "list", "--type", "task", "--format", "slugs"];

    let status_filter;
    if let Some(ref status) = opts.status_filter {
        args.push("--status");
        status_filter = status.clone();
        args.push(&status_filter);
    }
    if opts.unblocked {
        args.push("--unblocked");
    }
    if opts.unassigned {
        args.push("--unassigned");
    }

    let output = run_git_cmd(&args).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git kb list failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// Query a saved view via `git kb list --view` for task slugs.
async fn view_query_slugs(view_slug: &str) -> Result<Vec<String>> {
    let output = run_git_cmd(&["kb", "list", "--view", view_slug, "--format", "slugs"]).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git kb list --view failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}
