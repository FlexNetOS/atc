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
        "id",
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
            record.id,
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
    let mut display_records: Vec<DispatchRecord> = results.into_iter().map(|r| r.record).collect();

    // Add needs-human records (shown but not evaluated)
    let needs_human = registry
        .list(StatusFilter::by_status(Status::NeedsHuman))
        .await?;
    display_records.extend(needs_human);

    // If --all, also include done and failed (excluding already-collected IDs
    // to avoid duplicates when a record transitioned to terminal status this run)
    if all {
        let existing_ids: std::collections::HashSet<String> =
            display_records.iter().map(|r| r.id.clone()).collect();
        let terminal = registry
            .list(StatusFilter::any(vec![
                Status::Done,
                Status::Failed,
                Status::Stopped,
                Status::Retrying,
            ]))
            .await?;
        display_records.extend(
            terminal
                .into_iter()
                .filter(|r| !existing_ids.contains(&r.id)),
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use atc_core::types::{HealthChecks, Mode};
    use chrono::Utc;

    fn make_record(status: Status, checks: HealthChecks) -> DispatchRecord {
        DispatchRecord {
            id: "test@implement@1234567890".to_string(),
            task_slug: Some("test".to_string()),
            branch: "test-branch".to_string(),
            worktree_path: PathBuf::from("/tmp/test"),
            session: "test-session".to_string(),
            log_file: PathBuf::from("/tmp/test.jsonl"),
            status,
            mode: Mode::Implement,
            retries: 0,
            resolver: "task".to_string(),
            pr_url: None,
            checks,
            cost_usd: None,
            num_turns: None,
            duration_ms: None,
            dispatched_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn test_signal_values_all_false_running() {
        let record = make_record(Status::Running, HealthChecks::default());
        let vals = signal_values(&record);
        assert_eq!(vals, [Some(false), None, None, None, None, None]);
    }

    #[test]
    fn test_signal_values_needs_human_all_none() {
        let record = make_record(Status::NeedsHuman, HealthChecks::default());
        let vals = signal_values(&record);
        assert_eq!(vals, [None; 6]);
    }

    #[test]
    fn test_signal_values_all_true() {
        let checks = HealthChecks {
            agent_exited_clean: true,
            branch_pushed: true,
            pr_created: true,
            ci_passed: true,
            reviews_approved: true,
            threads_resolved: true,
        };
        let record = make_record(Status::Done, checks);
        let vals = signal_values(&record);
        assert_eq!(vals, [Some(true); 6]);
    }

    #[test]
    fn test_signal_values_short_circuit_at_branch() {
        let checks = HealthChecks {
            agent_exited_clean: true,
            branch_pushed: false,
            ..Default::default()
        };
        let record = make_record(Status::Failed, checks);
        let vals = signal_values(&record);
        assert_eq!(vals, [Some(true), Some(false), None, None, None, None]);
    }

    #[test]
    fn test_signal_values_short_circuit_at_pr() {
        let checks = HealthChecks {
            agent_exited_clean: true,
            branch_pushed: true,
            pr_created: false,
            ..Default::default()
        };
        let record = make_record(Status::Failed, checks);
        let vals = signal_values(&record);
        assert_eq!(
            vals,
            [Some(true), Some(true), Some(false), None, None, None]
        );
    }

    #[test]
    fn test_signal_values_short_circuit_at_ci() {
        let checks = HealthChecks {
            agent_exited_clean: true,
            branch_pushed: true,
            pr_created: true,
            ci_passed: false,
            ..Default::default()
        };
        let record = make_record(Status::NeedsReview, checks);
        let vals = signal_values(&record);
        assert_eq!(
            vals,
            [Some(true), Some(true), Some(true), Some(false), None, None]
        );
    }

    #[test]
    fn test_signal_values_short_circuit_at_reviews() {
        let checks = HealthChecks {
            agent_exited_clean: true,
            branch_pushed: true,
            pr_created: true,
            ci_passed: true,
            reviews_approved: false,
            ..Default::default()
        };
        let record = make_record(Status::NeedsReview, checks);
        let vals = signal_values(&record);
        assert_eq!(
            vals,
            [
                Some(true),
                Some(true),
                Some(true),
                Some(true),
                Some(false),
                None
            ]
        );
    }

    #[test]
    fn test_signal_display_values() {
        assert_eq!(signal_display(Some(true)), "✓");
        assert_eq!(signal_display(Some(false)), "✗");
        assert_eq!(signal_display(None), "-");
    }
}
