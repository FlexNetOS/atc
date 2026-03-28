use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::executor::AgentExecutor;
use atc_core::health::{HealthChecker, HealthResult};
use atc_core::post_completion::{self, PostCompleteInput};
use atc_core::registry::{Registry, StatusFilter};
use atc_core::types::{Directive, DispatchRecord, RunOpts, Status, WorkUnitStatus};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::warn;

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

/// Determine which NeedsReview records should have review-fix dispatched.
pub fn collect_auto_review_candidates(results: &[HealthResult]) -> Vec<&DispatchRecord> {
    results
        .iter()
        .filter(|r| {
            r.changed
                && r.record.status == Status::NeedsReview
                && !r.record.pr_urls.is_empty()
                && r.record.task_slug.is_some()
                // Don't dispatch a review-fix for a record that is itself a
                // review-fix — that would loop.  Failed review-fixes should go
                // through the retry path instead.
                && r.record.directive != Directive::ReviewFix
        })
        .map(|r| &r.record)
        .collect()
}

/// Check if a cost exceeds the configured threshold and return a warning message if so.
pub fn cost_warning(record: &DispatchRecord, threshold: f64) -> Option<String> {
    if let Some(cost) = record.cost_usd {
        if cost > threshold {
            return Some(format!(
                "\u{26a0} {} cost ${:.2} (exceeds ${:.2} threshold)",
                record.id, cost, threshold
            ));
        }
    }
    None
}

/// Print a message to stdout, or stderr when in JSON mode (to keep stdout parsable).
fn emit(json: bool, msg: &str) {
    if json {
        eprintln!("{msg}");
    } else {
        println!("{msg}");
    }
}

/// Run the health command: evaluate signals, apply transitions, display results,
/// and optionally auto-dispatch review-fix for NeedsReview records.
pub async fn run_health(
    config: &AtcConfig,
    registry: Arc<dyn Registry>,
    executor: Arc<dyn AgentExecutor>,
    json: bool,
    all: bool,
    auto_flag: bool,
) -> Result<()> {
    let checker = HealthChecker {
        registry: registry.clone(),
        config: Arc::new(config.clone()),
        git_bin: PathBuf::from("git"),
        gh_bin: PathBuf::from("gh"),
        tmux_bin: PathBuf::from("tmux"),
    };

    // Evaluate active records (running + needs-review)
    let mut results: Vec<HealthResult> = checker.run().await?;

    // --- 7B: Stale record cleanup ---
    // For records in a terminal state whose post-completion was never triggered by
    // the watcher, run artifact extraction now as a fallback.  We use
    // `artifacts.is_none()` as the idempotency guard (not `r.changed`) so that
    // records missed on a previous health run are still picked up.
    let mut refreshed_ids: Vec<String> = Vec::new();
    for r in &results {
        if r.record.checks.agent_exited_clean
            && matches!(
                r.record.status,
                Status::Done | Status::Failed | Status::NeedsReview
            )
            // Only run if post-completion hasn't already stored artifacts
            // (artifacts are always written by post-completion, even when no
            // result event exists, making this a reliable once-only sentinel)
            && r.record.artifacts.is_none()
        {
            // Check if log file has artifacts we can extract
            if r.record.log_file.exists() {
                let input = PostCompleteInput {
                    dispatch_id: r.record.id.clone(),
                    exit_code: None,
                    log_file: Some(r.record.log_file.clone()),
                    skip_cleanup: true,
                };
                if let Err(e) =
                    post_completion::run_post_completion(&input, registry.as_ref(), config).await
                {
                    warn!(
                        id = %r.record.id,
                        error = %e,
                        "stale record post-completion extraction failed"
                    );
                } else {
                    refreshed_ids.push(r.record.id.clone());
                }
            } else {
                warn!(
                    id = %r.record.id,
                    log_file = %r.record.log_file.display(),
                    "stale record missing log file — skipping post-completion fallback"
                );
            }
        }
    }

    // Re-read records that were updated by 7B so downstream logic (7C, 7D,
    // display) uses fresh PR URLs, cost data, and status.
    for id in &refreshed_ids {
        if let Ok(Some(fresh)) = registry.get(id).await {
            if let Some(entry) = results.iter_mut().find(|r| &r.record.id == id) {
                entry.record = fresh;
            }
        }
    }

    // --- 7A: Cost threshold warnings ---
    // Emitted after 7B so that records refreshed by stale-record extraction
    // have their cost_usd populated before the warning pass.
    // We warn for records that just transitioned (r.changed) OR that 7B just
    // ran post-completion for (refreshed_ids) — the latter are stale records
    // whose watcher died before post-completion, making them the most likely
    // to have unusual cost values.
    let cost_threshold = config.health.cost_warning_threshold;
    let refreshed_id_set: std::collections::HashSet<&str> =
        refreshed_ids.iter().map(|s| s.as_str()).collect();
    for r in &results {
        if !r.changed && !refreshed_id_set.contains(r.record.id.as_str()) {
            continue;
        }
        if let Some(msg) = cost_warning(&r.record, cost_threshold) {
            emit(json, &msg);
        }
    }

    // --- 7C: Auto-cleanup worktrees for Done records with merged/closed PRs ---
    let auto_enabled = auto_flag || config.health.auto_review;
    if auto_enabled {
        let worktree_base = config.dispatch.resolved_worktree_base();

        // Collect Done records from the health-check snapshot
        let snapshot_ids: std::collections::HashSet<String> =
            results.iter().map(|r| r.record.id.clone()).collect();

        // Also load Done records from the registry that weren't in the snapshot
        // (records that reached Done before this health run are not included in
        // the health-checker results, so their worktrees would never be cleaned).
        let mut done_records: Vec<&DispatchRecord> = results
            .iter()
            .filter(|r| r.record.status == Status::Done)
            .map(|r| &r.record)
            .collect();

        let registry_done = registry.list(StatusFilter::by_status(Status::Done)).await?;
        // Limit to records updated in the last 30 days to bound the number of
        // sequential `gh pr view` API calls on first --auto run (T19).
        let cutoff = chrono::Utc::now() - chrono::Duration::days(30);
        let extra_done: Vec<&DispatchRecord> = registry_done
            .iter()
            .filter(|r| !snapshot_ids.contains(&r.id) && r.updated_at > cutoff)
            .collect();
        done_records.extend(extra_done);

        for record in &done_records {
            if record.pr_urls.is_empty() || !record.worktree_path.exists() {
                continue;
            }
            // Only clean up worktree when ALL tracked PRs are terminal
            let all_done = futures::future::join_all(
                record
                    .pr_urls
                    .iter()
                    .map(|u| post_completion::is_pr_done(u)),
            )
            .await
            .into_iter()
            .all(|done| done);
            if all_done {
                post_completion::cleanup_worktree(&record.worktree_path, &worktree_base).await;
            }
        }
    }

    // --- 7C2: Work unit merge detection ---
    // Check active work units and transition to merged/closed when all PRs are done.
    {
        let active_units = registry.list_active_work_units().await?;
        for wu in &active_units {
            if wu.pr_urls.is_empty() {
                continue;
            }

            // Check each PR's state individually to distinguish merged vs closed
            let pr_states = futures::future::join_all(
                wu.pr_urls
                    .iter()
                    .map(|u| post_completion::get_pr_state_public(u)),
            )
            .await;
            // If any PR state lookup failed (None), log and skip — don't leave
            // the unit stuck in active forever, but also don't wrongly transition.
            if pr_states.iter().any(|s| s.is_none()) {
                warn!(
                    work_unit = %wu.id,
                    pr_urls = ?wu.pr_urls,
                    "PR state lookup returned None for one or more PRs — skipping work unit transition (transient failure?)"
                );
                continue;
            }
            // Single pass: check all terminal + track if any merged
            let (all_terminal, any_merged) = pr_states.iter().fold((true, false), |(at, am), s| {
                let is_terminal = matches!(s.as_deref(), Some("MERGED") | Some("CLOSED"));
                let is_merged = matches!(s.as_deref(), Some("MERGED"));
                (at && is_terminal, am || is_merged)
            });
            if !all_terminal {
                continue;
            }
            let new_status = if any_merged {
                WorkUnitStatus::Merged
            } else {
                WorkUnitStatus::Closed
            };
            match registry
                .update_work_unit_status_if_idle(&wu.id, new_status)
                .await
            {
                Ok(true) => {
                    emit(
                        json,
                        &format!(
                            "Work unit {} → {} (all PRs done)",
                            wu.task_slug.as_deref().unwrap_or(&wu.id),
                            new_status.as_str()
                        ),
                    );
                }
                Ok(false) => {
                    // A non-terminal dispatch is still attached — skip transition
                }
                Err(e) => {
                    warn!(work_unit = %wu.id, error = %e, "failed to transition work unit to {}", new_status.as_str());
                }
            }
        }
    }

    // --- 7D: Auto-remediation ---
    if auto_enabled {
        let candidates = collect_auto_review_candidates(&results);
        for record in &candidates {
            let Some(task_slug) = record.task_slug.clone() else {
                warn!(id = %record.id, "skipping auto review-fix: missing task_slug");
                continue;
            };
            let pr_url = record.pr_urls.first().cloned();
            emit(
                json,
                &format!("Auto-triggering review-fix for {task_slug}..."),
            );
            let opts = RunOpts {
                input: format!("task {task_slug}"),
                directive: Some(Directive::ReviewFix),
                pr_url,
                repos: vec![],
                params: std::collections::HashMap::new(),
                inline: false,
                force: false,
                dry_run: false,
                directives: None,
                no_worktree: false,
                max_budget_usd: None,
                max_turns: None,
                retries: 0,
                list: false,
                ephemeral: false,
                timeout: None,
            };
            let resolvers = crate::resolvers::build_resolvers(config);
            let pipeline = crate::pipeline::DispatchPipeline {
                resolvers,
                config,
                registry: registry.as_ref(),
                executor: executor.as_ref(),
            };
            match pipeline.execute(&format!("task {task_slug}"), &opts).await {
                Ok(outcome) => {
                    emit(
                        json,
                        &format!(
                            "  Dispatched review-fix for {}: session={}",
                            task_slug, outcome.session
                        ),
                    );
                }
                Err(e) => {
                    warn!(task = %task_slug, error = %e, "auto review-fix dispatch failed");
                    emit(
                        json,
                        &format!("  Warning: review-fix dispatch failed for {task_slug}: {e}"),
                    );
                }
            }
        }
    }

    // Collect evaluated records for display
    let mut display_records: Vec<DispatchRecord> = results.into_iter().map(|r| r.record).collect();

    // Add needs-human records (shown but not evaluated)
    let needs_human = registry
        .list(StatusFilter::by_status(Status::NeedsHuman))
        .await?;
    display_records.extend(needs_human);

    // If --all, also include terminal and non-active records (excluding
    // already-collected IDs to avoid duplicates when a record transitioned this run)
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
    use atc_core::types::{Directive, HealthChecks};
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
            directive: Directive::Implement,
            retries: 0,
            resolver: "task".to_string(),
            pr_urls: vec![],
            no_worktree: false,
            original_input: None,
            checks,
            kb_root: None,
            cost_usd: None,
            num_turns: None,
            duration_ms: None,
            artifacts: None,
            work_unit_id: None,
            dispatched_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn make_record_with_pr(
        status: Status,
        checks: HealthChecks,
        pr_url: Option<String>,
        cost_usd: Option<f64>,
    ) -> DispatchRecord {
        DispatchRecord {
            pr_urls: pr_url.into_iter().collect(),
            cost_usd,
            ..make_record(status, checks)
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

    // --- 7D: Auto-review candidate tests ---

    #[test]
    fn test_auto_review_collects_needs_review_with_pr() {
        let checks = HealthChecks {
            agent_exited_clean: true,
            branch_pushed: true,
            pr_created: true,
            ci_passed: false,
            ..Default::default()
        };
        let record = make_record_with_pr(
            Status::NeedsReview,
            checks,
            Some("https://github.com/org/repo/pull/1".to_string()),
            None,
        );
        let results = vec![HealthResult {
            record,
            changed: true,
        }];
        let candidates = collect_auto_review_candidates(&results);
        assert_eq!(candidates.len(), 1);
    }

    #[test]
    fn test_auto_review_skips_needs_review_without_pr() {
        let checks = HealthChecks {
            agent_exited_clean: true,
            branch_pushed: true,
            pr_created: true,
            ci_passed: false,
            ..Default::default()
        };
        let record = make_record_with_pr(Status::NeedsReview, checks, None, None);
        let results = vec![HealthResult {
            record,
            changed: true,
        }];
        let candidates = collect_auto_review_candidates(&results);
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_auto_review_skips_unchanged_needs_review() {
        let checks = HealthChecks {
            agent_exited_clean: true,
            branch_pushed: true,
            pr_created: true,
            ci_passed: false,
            ..Default::default()
        };
        let record = make_record_with_pr(
            Status::NeedsReview,
            checks,
            Some("https://github.com/org/repo/pull/1".to_string()),
            None,
        );
        let results = vec![HealthResult {
            record,
            changed: false,
        }];
        let candidates = collect_auto_review_candidates(&results);
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_auto_review_skips_non_needs_review() {
        let checks = HealthChecks::default();
        let record = make_record_with_pr(
            Status::Running,
            checks,
            Some("https://github.com/org/repo/pull/1".to_string()),
            None,
        );
        let results = vec![HealthResult {
            record,
            changed: false,
        }];
        let candidates = collect_auto_review_candidates(&results);
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_auto_review_skips_missing_task_slug() {
        let checks = HealthChecks {
            agent_exited_clean: true,
            branch_pushed: true,
            pr_created: true,
            ci_passed: false,
            ..Default::default()
        };
        let mut record = make_record_with_pr(
            Status::NeedsReview,
            checks,
            Some("https://github.com/org/repo/pull/1".to_string()),
            None,
        );
        record.task_slug = None;
        let results = vec![HealthResult {
            record,
            changed: true,
        }];
        let candidates = collect_auto_review_candidates(&results);
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_auto_review_skips_review_fix_directive() {
        let checks = HealthChecks {
            agent_exited_clean: true,
            branch_pushed: true,
            pr_created: true,
            ci_passed: false,
            ..Default::default()
        };
        let mut record = make_record_with_pr(
            Status::NeedsReview,
            checks,
            Some("https://github.com/org/repo/pull/1".to_string()),
            None,
        );
        record.directive = Directive::ReviewFix;
        let results = vec![HealthResult {
            record,
            changed: true,
        }];
        let candidates = collect_auto_review_candidates(&results);
        assert!(candidates.is_empty());
    }

    // --- 7A: Cost warning tests ---

    #[test]
    fn test_cost_warning_over_threshold() {
        let record = make_record_with_pr(Status::Done, HealthChecks::default(), None, Some(15.0));
        let msg = cost_warning(&record, 10.0);
        assert!(msg.is_some());
        let msg = msg.unwrap();
        assert!(msg.contains("15.00"));
        assert!(msg.contains("10.00"));
    }

    #[test]
    fn test_cost_warning_under_threshold() {
        let record = make_record_with_pr(Status::Done, HealthChecks::default(), None, Some(5.0));
        assert!(cost_warning(&record, 10.0).is_none());
    }

    #[test]
    fn test_cost_warning_no_cost() {
        let record = make_record_with_pr(Status::Done, HealthChecks::default(), None, None);
        assert!(cost_warning(&record, 10.0).is_none());
    }

    #[test]
    fn test_cost_warning_exact_threshold() {
        let record = make_record_with_pr(Status::Done, HealthChecks::default(), None, Some(10.0));
        // Exactly at threshold should NOT warn (> not >=)
        assert!(cost_warning(&record, 10.0).is_none());
    }

    #[test]
    fn test_cost_warning_custom_threshold() {
        let record = make_record_with_pr(Status::Done, HealthChecks::default(), None, Some(6.0));
        assert!(cost_warning(&record, 5.0).is_some());
        assert!(cost_warning(&record, 10.0).is_none());
    }
}
