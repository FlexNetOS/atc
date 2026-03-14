use anyhow::Result;
use atc_core::health::{HealthChecker, HealthResult};
use atc_core::registry::{Registry, StatusFilter};
use atc_core::types::{DispatchRecord, Status};
use std::path::PathBuf;
use std::sync::Arc;

/// Format a three-state signal value for display.
/// `Some(true)` = "✓", `Some(false)` = "✗", `None` = "-" (not evaluated / skipped).
fn signal_display(evaluated: Option<bool>) -> &'static str {
    match evaluated {
        Some(true) => "✓",
        Some(false) => "✗",
        None => "-",
    }
}

/// Convert a DispatchRecord's checks into an array of Option<bool> values
/// representing the display state of each signal. For needs-human records,
/// all signals display as None (not evaluated).
fn signal_values(record: &DispatchRecord) -> [Option<bool>; 6] {
    if record.status == Status::NeedsHuman {
        return [None; 6];
    }

    let c = &record.checks;

    // Short-circuit display: if a signal is false, downstream signals show as None
    let agent = Some(c.agent_exited_clean);
    if !c.agent_exited_clean {
        return [agent, None, None, None, None, None];
    }

    let branch = Some(c.branch_pushed);
    if !c.branch_pushed {
        return [agent, branch, None, None, None, None];
    }

    let pr = Some(c.pr_created);
    if !c.pr_created {
        return [agent, branch, pr, None, None, None];
    }

    let ci = Some(c.ci_passed);
    if !c.ci_passed {
        return [agent, branch, pr, ci, None, None];
    }

    let reviews = Some(c.reviews_approved);
    if !c.reviews_approved {
        return [agent, branch, pr, ci, reviews, None];
    }

    let threads = Some(c.threads_resolved);
    [agent, branch, pr, ci, reviews, threads]
}

fn print_table(records: &[DispatchRecord]) {
    // Header
    println!(
        "{:<25} {:<14} {:<14} {:<15} {:<12} {:<11} {:<18} {:<16}",
        "slug",
        "status",
        "agent_exited",
        "branch_pushed",
        "pr_created",
        "ci_passed",
        "reviews_approved",
        "threads_resolved"
    );

    for record in records {
        let signals = signal_values(record);
        println!(
            "{:<25} {:<14} {:<14} {:<15} {:<12} {:<11} {:<18} {:<16}",
            record.slug,
            record.status.as_str(),
            signal_display(signals[0]),
            signal_display(signals[1]),
            signal_display(signals[2]),
            signal_display(signals[3]),
            signal_display(signals[4]),
            signal_display(signals[5]),
        );
    }
}

/// Run the health command: evaluate signals, apply transitions, display results.
pub async fn run_health(
    config: &atc_core::config::AtcConfig,
    registry: Arc<dyn Registry>,
    json: bool,
    all: bool,
) -> Result<()> {
    let checker = HealthChecker {
        registry: registry.clone(),
        config: Arc::new(config.clone()),
        git_bin: PathBuf::from("git"),
        gh_bin: PathBuf::from("gh"),
        tmux_bin: PathBuf::from("tmux"),
    };

    // Evaluate active records (running + needs-review)
    let results: Vec<HealthResult> = checker.run().await?;

    // Collect evaluated records
    let mut display_records: Vec<DispatchRecord> =
        results.into_iter().map(|r| r.record).collect();

    // Add needs-human records (shown but not evaluated)
    let needs_human = registry
        .list(StatusFilter::by_status(Status::NeedsHuman))
        .await?;
    display_records.extend(needs_human);

    // If --all, also include done and failed
    if all {
        let done = registry
            .list(StatusFilter::by_status(Status::Done))
            .await?;
        let failed = registry
            .list(StatusFilter::by_status(Status::Failed))
            .await?;
        display_records.extend(done);
        display_records.extend(failed);
    }

    // Sort by dispatched_at desc for consistent display
    display_records.sort_by(|a, b| b.dispatched_at.cmp(&a.dispatched_at));

    if json {
        let json_out = serde_json::to_string_pretty(&display_records)?;
        println!("{json_out}");
    } else if display_records.is_empty() {
        println!("No dispatch records found.");
    } else {
        print_table(&display_records);
    }

    Ok(())
}
