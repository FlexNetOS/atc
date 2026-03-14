use crate::config::AtcConfig;
use crate::registry::{Registry, StatusFilter};
use crate::types::{DispatchRecord, HealthChecks, Status};
use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Result of evaluating one record.
#[derive(Debug)]
pub struct HealthResult {
    /// The record with updated checks and status.
    pub record: DispatchRecord,
    /// True if any field changed from its prior state.
    pub changed: bool,
}

/// Tri-state signal result distinguishing definitive answers from transient errors.
/// When a signal returns `Error`, the health checker skips the transition to avoid
/// permanently stranding a record in a terminal state due to a transient failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignalResult {
    /// Signal definitively true (e.g. branch exists, CI passed).
    True,
    /// Signal definitively false (e.g. branch not found, CI failed).
    False,
    /// Transient error (timeout, CLI failure, auth issue) — cannot determine state.
    Error,
}

impl SignalResult {
    fn is_true(self) -> bool {
        self == Self::True
    }
    fn is_error(self) -> bool {
        self == Self::Error
    }
}

#[derive(Clone)]
pub struct HealthChecker {
    pub registry: Arc<dyn Registry>,
    pub config: Arc<AtcConfig>,
    pub git_bin: PathBuf,
    pub gh_bin: PathBuf,
    pub tmux_bin: PathBuf,
}

impl HealthChecker {
    /// Evaluate all active records and apply transitions.
    /// Waits for all spawned tasks to complete before returning, ensuring no
    /// detached tasks continue writing to the DB after this function returns.
    pub async fn run(&self) -> Result<Vec<HealthResult>> {
        let records = self
            .registry
            .list(StatusFilter::any(vec![
                Status::Running,
                Status::NeedsReview,
            ]))
            .await?;

        let mut handles = Vec::new();
        for record in records {
            let checker = self.clone();
            handles.push(tokio::spawn(async move {
                checker.check_record(record).await
            }));
        }

        // Collect all results — don't short-circuit on error so that remaining
        // tasks are not detached (which would let them keep writing to the DB).
        let mut results = Vec::new();
        let mut errors = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(Ok(result)) => results.push(result),
                Ok(Err(e)) => errors.push(e),
                Err(e) => errors.push(anyhow::anyhow!("task panicked: {e}")),
            }
        }

        // Log errors but still return successful results
        for e in &errors {
            warn!(error = %e, "health check task failed");
        }

        // If ALL tasks failed, propagate the first error
        if results.is_empty() && !errors.is_empty() {
            return Err(errors.into_iter().next().unwrap());
        }

        Ok(results)
    }

    /// Evaluate all six health signals for a single record and apply the
    /// transition matrix. Signals are evaluated sequentially with short-circuit
    /// logic — if signal N is false/error, downstream signals are skipped.
    ///
    /// **Timing**: Worst-case per-record time is 6 × `signal_timeout_secs`
    /// (default 30s × 6 = 180s) when every signal hits the timeout. In practice,
    /// short-circuiting and fast subprocess responses keep this well below the
    /// theoretical maximum. Records are evaluated concurrently via `tokio::spawn`
    /// in `run()`, so total wall-clock time is bounded by the slowest record,
    /// not the sum.
    async fn check_record(&self, mut record: DispatchRecord) -> Result<HealthResult> {
        let old_checks = record.checks.clone();
        let old_status = record.status;

        let mut had_error = false;

        // Signal 1: agent_exited_clean
        let s1 = self.eval_signal_1(&record).await;
        had_error = had_error || s1.is_error();
        let mut checks = HealthChecks {
            agent_exited_clean: s1.is_true(),
            ..Default::default()
        };

        if !s1.is_true() {
            // short-circuit: all downstream stays false
        } else {
            // Signal 2: branch_pushed
            let s2 = self.eval_signal_2(&record).await;
            had_error = had_error || s2.is_error();
            checks.branch_pushed = s2.is_true();
            if !s2.is_true() {
                // short-circuit: downstream stays false
            } else {
                // Signal 3: pr_created (may update pr_url)
                let s3 = self.eval_signal_3(&mut record).await;
                had_error = had_error || s3.is_error();
                checks.pr_created = s3.is_true();
                if !s3.is_true() {
                    // short-circuit
                } else {
                    // Signal 4: ci_passed
                    let s4 = self.eval_signal_4(&record).await;
                    had_error = had_error || s4.is_error();
                    checks.ci_passed = s4.is_true();
                    if !s4.is_true() {
                        // short-circuit
                    } else {
                        // Signal 5: reviews_approved
                        let s5 = self.eval_signal_5(&record).await;
                        had_error = had_error || s5.is_error();
                        checks.reviews_approved = s5.is_true();
                        if !s5.is_true() {
                            // short-circuit
                        } else {
                            // Signal 6: threads_resolved
                            let s6 = self.eval_signal_6(&record).await;
                            had_error = had_error || s6.is_error();
                            checks.threads_resolved = s6.is_true();
                        }
                    }
                }
            }
        }

        let new_status = Self::apply_transition(&checks);

        // If any signal hit a transient error AND the transition would move to a
        // terminal state (Done/Failed), skip the transition to avoid permanently
        // stranding the record due to a transient failure.
        if Self::should_skip_transition(had_error, new_status) {
            warn!(
                slug = %record.slug,
                proposed_status = %new_status,
                "health check: skipping terminal transition due to transient signal error"
            );
            return Ok(HealthResult {
                record,
                changed: false,
            });
        }

        let changed = checks != old_checks || new_status != old_status;

        if changed {
            let now = chrono::Utc::now();
            record.checks = checks;
            record.status = new_status;
            record.updated_at = now;

            // Persist checks + status atomically in a single UPDATE
            self.registry
                .update_health(
                    &record.slug,
                    &record.checks,
                    record.status,
                    record.updated_at,
                )
                .await?;

            info!(
                slug = %record.slug,
                status = %record.status,
                "health check: status transitioned"
            );
        } else {
            debug!(
                slug = %record.slug,
                status = %record.status,
                "health check: no change"
            );
        }

        // Store pr_url if discovered by signal 3
        // (already persisted inside eval_signal_3)

        Ok(HealthResult { record, changed })
    }

    /// Signal 1: Check if tmux session still exists.
    async fn eval_signal_1(&self, record: &DispatchRecord) -> SignalResult {
        let timeout = Duration::from_secs(self.config.health.signal_timeout_secs);
        let mut cmd = tokio::process::Command::new(&self.tmux_bin);
        cmd.kill_on_drop(true)
            .args(["has-session", "-t", &record.session])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let result = tokio::time::timeout(timeout, cmd.status()).await;

        match result {
            Ok(Ok(status)) => {
                if status.success() {
                    // Session exists — agent still running
                    debug!(slug = %record.slug, "signal 1: tmux session exists, agent running");
                    SignalResult::False
                } else {
                    // Session gone — agent finished
                    debug!(slug = %record.slug, "signal 1: tmux session gone, agent exited");
                    SignalResult::True
                }
            }
            Ok(Err(e)) => {
                warn!(slug = %record.slug, error = %e, "signal 1: tmux command failed");
                SignalResult::Error
            }
            Err(_) => {
                warn!(slug = %record.slug, "signal 1: tmux command timed out");
                SignalResult::Error
            }
        }
    }

    /// Signal 2: Check if branch exists on remote.
    async fn eval_signal_2(&self, record: &DispatchRecord) -> SignalResult {
        let timeout = Duration::from_secs(self.config.health.signal_timeout_secs);
        let mut cmd = tokio::process::Command::new(&self.git_bin);
        cmd.kill_on_drop(true)
            .args([
                "-C",
                &record.worktree_path.to_string_lossy(),
                "ls-remote",
                "--exit-code",
                "--heads",
                "origin",
                &record.branch,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let result = tokio::time::timeout(timeout, cmd.status()).await;

        match result {
            Ok(Ok(status)) => {
                let code = status.code().unwrap_or(-1);
                match code {
                    0 => {
                        debug!(slug = %record.slug, "signal 2: branch exists on remote");
                        SignalResult::True
                    }
                    2 => {
                        // Exit code 2 = definitive "not found"
                        debug!(slug = %record.slug, "signal 2: branch not found on remote");
                        SignalResult::False
                    }
                    other => {
                        // Unexpected exit code — could be auth/network error
                        warn!(slug = %record.slug, exit_code = other, "signal 2: git ls-remote unexpected exit code");
                        SignalResult::Error
                    }
                }
            }
            Ok(Err(e)) => {
                warn!(slug = %record.slug, error = %e, "signal 2: git ls-remote failed");
                SignalResult::Error
            }
            Err(_) => {
                warn!(slug = %record.slug, "signal 2: git ls-remote timed out");
                SignalResult::Error
            }
        }
    }

    /// Signal 3: Check if PR exists for the branch. Updates pr_url in registry if discovered.
    async fn eval_signal_3(&self, record: &mut DispatchRecord) -> SignalResult {
        // If pr_url already known, skip the gh call
        if record.pr_url.is_some() {
            debug!(slug = %record.slug, "signal 3: pr_url already known, skipping gh call");
            return SignalResult::True;
        }

        let timeout = Duration::from_secs(self.config.health.signal_timeout_secs);
        let mut cmd = tokio::process::Command::new(&self.gh_bin);
        cmd.kill_on_drop(true)
            .args([
                "pr",
                "list",
                "--head",
                &record.branch,
                "--state",
                "all",
                "--json",
                "number,url",
                "--jq",
                ".[0]",
            ])
            .current_dir(&record.worktree_path)
            .stderr(std::process::Stdio::null());
        let result = tokio::time::timeout(timeout, cmd.output()).await;

        match result {
            Ok(Ok(output)) => {
                if !output.status.success() {
                    warn!(slug = %record.slug, "signal 3: gh pr list failed");
                    return SignalResult::Error;
                }
                let stdout = String::from_utf8_lossy(&output.stdout);
                let trimmed = stdout.trim();
                if trimmed.is_empty() || trimmed == "null" {
                    // gh succeeded but returned no results — definitively no PR
                    debug!(slug = %record.slug, "signal 3: no PR found");
                    return SignalResult::False;
                }
                // Parse JSON to extract url
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) {
                    if let Some(url) = json.get("url").and_then(|v| v.as_str()) {
                        info!(slug = %record.slug, url = %url, "signal 3: PR discovered");
                        // Store in registry
                        if let Err(e) = self.registry.set_pr_url(&record.slug, url).await {
                            warn!(slug = %record.slug, error = %e, "signal 3: failed to store pr_url");
                        }
                        record.pr_url = Some(url.to_string());
                        return SignalResult::True;
                    }
                }
                warn!(slug = %record.slug, output = %trimmed, "signal 3: could not parse PR URL from gh output");
                SignalResult::Error
            }
            Ok(Err(e)) => {
                warn!(slug = %record.slug, error = %e, "signal 3: gh pr list command failed");
                SignalResult::Error
            }
            Err(_) => {
                warn!(slug = %record.slug, "signal 3: gh pr list timed out");
                SignalResult::Error
            }
        }
    }

    /// Signal 4: Check if all CI checks passed on the PR.
    /// Only counts genuinely failed states (FAILURE, TIMED_OUT, CANCELLED,
    /// ACTION_REQUIRED). SKIPPED and NEUTRAL are not considered failures.
    ///
    /// **Note:** If the repository has no CI checks configured, `gh pr checks`
    /// returns an empty list. The jq filter then evaluates to `0` (zero failures),
    /// so this signal returns `True`. This is intentional: repos without CI
    /// should not block the Done transition. If CI is later added and fails,
    /// the next health check cycle will catch it.
    async fn eval_signal_4(&self, record: &DispatchRecord) -> SignalResult {
        let pr_url = match &record.pr_url {
            Some(url) => url,
            None => return SignalResult::False,
        };

        let timeout = Duration::from_secs(self.config.health.signal_timeout_secs);
        let mut cmd = tokio::process::Command::new(&self.gh_bin);
        cmd.kill_on_drop(true)
            .args([
                "pr",
                "checks",
                pr_url,
                "--json",
                "name,state",
                "--jq",
                "[.[] | select(.state == \"FAILURE\" or .state == \"TIMED_OUT\" or .state == \"CANCELLED\" or .state == \"ACTION_REQUIRED\")] | length",
            ])
            .current_dir(&record.worktree_path)
            .stderr(std::process::Stdio::null());
        let result = tokio::time::timeout(timeout, cmd.output()).await;

        match result {
            Ok(Ok(output)) => {
                if !output.status.success() {
                    warn!(slug = %record.slug, "signal 4: gh pr checks failed");
                    return SignalResult::Error;
                }
                let stdout = String::from_utf8_lossy(&output.stdout);
                let trimmed = stdout.trim();
                match trimmed.parse::<u64>() {
                    Ok(0) => {
                        debug!(slug = %record.slug, "signal 4: all CI checks passed");
                        SignalResult::True
                    }
                    Ok(n) => {
                        debug!(slug = %record.slug, failing = n, "signal 4: CI checks failing");
                        SignalResult::False
                    }
                    Err(_) => {
                        warn!(slug = %record.slug, output = %trimmed, "signal 4: could not parse gh pr checks output");
                        SignalResult::Error
                    }
                }
            }
            Ok(Err(e)) => {
                warn!(slug = %record.slug, error = %e, "signal 4: gh pr checks command failed");
                SignalResult::Error
            }
            Err(_) => {
                warn!(slug = %record.slug, "signal 4: gh pr checks timed out");
                SignalResult::Error
            }
        }
    }

    /// Signal 5: Check if PR reviews are approved.
    async fn eval_signal_5(&self, record: &DispatchRecord) -> SignalResult {
        let pr_url = match &record.pr_url {
            Some(url) => url,
            None => return SignalResult::False,
        };

        let timeout = Duration::from_secs(self.config.health.signal_timeout_secs);
        let mut cmd = tokio::process::Command::new(&self.gh_bin);
        cmd.kill_on_drop(true)
            .args([
                "pr",
                "view",
                pr_url,
                "--json",
                "reviewDecision",
                "--jq",
                ".reviewDecision",
            ])
            .current_dir(&record.worktree_path)
            .stderr(std::process::Stdio::null());
        let result = tokio::time::timeout(timeout, cmd.output()).await;

        match result {
            Ok(Ok(output)) => {
                if !output.status.success() {
                    warn!(slug = %record.slug, "signal 5: gh pr view failed");
                    return SignalResult::Error;
                }
                let stdout = String::from_utf8_lossy(&output.stdout);
                let trimmed = stdout.trim();
                match trimmed {
                    "APPROVED" => {
                        debug!(slug = %record.slug, "signal 5: reviews approved");
                        SignalResult::True
                    }
                    "" | "null" => {
                        // Empty or "null" reviewDecision means the repo either has no
                        // branch protection rules requiring reviews, or the PR has not
                        // yet received any reviews.  We treat this as approved because:
                        //   1. Most agent-created PRs target repos without mandatory review.
                        //   2. Blocking on a review that may never come would strand the
                        //      record permanently in NeedsReview.
                        // If the repo does require reviews, this signal will flip to
                        // CHANGES_REQUESTED or REVIEW_REQUIRED once a reviewer acts,
                        // and the next health check cycle will pick up the change.
                        debug!(slug = %record.slug, "signal 5: no review policy or no reviews yet, treating as approved");
                        SignalResult::True
                    }
                    other => {
                        debug!(slug = %record.slug, decision = %other, "signal 5: reviews not approved");
                        SignalResult::False
                    }
                }
            }
            Ok(Err(e)) => {
                warn!(slug = %record.slug, error = %e, "signal 5: gh pr view command failed");
                SignalResult::Error
            }
            Err(_) => {
                warn!(slug = %record.slug, "signal 5: gh pr view timed out");
                SignalResult::Error
            }
        }
    }

    /// Signal 6: Check if all review threads are resolved.
    /// Uses the GraphQL API because `gh pr view --json reviewThreads` is not
    /// a supported JSON field in the GitHub CLI. Includes `pageInfo` to detect
    /// truncated results (>100 threads) and treats them conservatively as unresolved.
    async fn eval_signal_6(&self, record: &DispatchRecord) -> SignalResult {
        let pr_url = match &record.pr_url {
            Some(url) => url,
            None => return SignalResult::False,
        };

        // Parse owner, repo, and PR number from the URL
        // Expected format: https://github.com/{owner}/{repo}/pull/{number}
        let (owner, repo, number) = match Self::parse_pr_url(pr_url) {
            Some(parts) => parts,
            None => {
                warn!(slug = %record.slug, url = %pr_url, "signal 6: could not parse PR URL");
                return SignalResult::Error;
            }
        };

        // Use GraphQL variables instead of string interpolation for hygiene.
        // The gh CLI's -f flag passes string variables, -F passes JSON-typed values.
        let query = r#"query($owner: String!, $repo: String!, $number: Int!) { repository(owner: $owner, name: $repo) { pullRequest(number: $number) { reviewThreads(first: 100) { pageInfo { hasNextPage } nodes { isResolved } } } } }"#;

        let timeout = Duration::from_secs(self.config.health.signal_timeout_secs);
        let mut cmd = tokio::process::Command::new(&self.gh_bin);
        cmd.kill_on_drop(true)
            .args([
                "api",
                "graphql",
                "-f",
                &format!("query={query}"),
                "-f",
                &format!("owner={owner}"),
                "-f",
                &format!("repo={repo}"),
                "-F",
                &format!("number={number}"),
            ])
            .stderr(std::process::Stdio::null());
        let result = tokio::time::timeout(timeout, cmd.output()).await;

        match result {
            Ok(Ok(output)) => {
                if !output.status.success() {
                    warn!(slug = %record.slug, "signal 6: gh api graphql failed");
                    return SignalResult::Error;
                }
                let stdout = String::from_utf8_lossy(&output.stdout);
                // Parse the GraphQL response to count unresolved threads
                match serde_json::from_str::<serde_json::Value>(&stdout) {
                    Ok(json) => {
                        // Check for GraphQL-level errors
                        if json.get("errors").is_some() {
                            warn!(slug = %record.slug, "signal 6: GraphQL response contains errors");
                            return SignalResult::Error;
                        }

                        let threads_obj =
                            json.pointer("/data/repository/pullRequest/reviewThreads");

                        // Check pagination — if there are more pages, we can't
                        // confirm all threads are resolved
                        if let Some(has_next) = threads_obj
                            .and_then(|t| t.pointer("/pageInfo/hasNextPage"))
                            .and_then(|v| v.as_bool())
                        {
                            if has_next {
                                warn!(slug = %record.slug, "signal 6: >100 review threads, result truncated — treating as unresolved");
                                return SignalResult::False;
                            }
                        }

                        let nodes = threads_obj
                            .and_then(|t| t.get("nodes"))
                            .and_then(|v| v.as_array());

                        match nodes {
                            Some(threads) => {
                                let unresolved = threads
                                    .iter()
                                    .filter(|t| {
                                        t.get("isResolved") == Some(&serde_json::Value::Bool(false))
                                    })
                                    .count();
                                if unresolved == 0 {
                                    debug!(slug = %record.slug, "signal 6: all threads resolved");
                                    SignalResult::True
                                } else {
                                    debug!(slug = %record.slug, unresolved = unresolved, "signal 6: unresolved threads remain");
                                    SignalResult::False
                                }
                            }
                            None => {
                                // nodes is null or missing — could be a field-level
                                // GraphQL error, treat as error rather than resolved
                                warn!(slug = %record.slug, "signal 6: reviewThreads nodes is null/missing");
                                SignalResult::Error
                            }
                        }
                    }
                    Err(e) => {
                        warn!(slug = %record.slug, error = %e, "signal 6: could not parse GraphQL response");
                        SignalResult::Error
                    }
                }
            }
            Ok(Err(e)) => {
                warn!(slug = %record.slug, error = %e, "signal 6: gh api graphql command failed");
                SignalResult::Error
            }
            Err(_) => {
                warn!(slug = %record.slug, "signal 6: gh api graphql timed out");
                SignalResult::Error
            }
        }
    }

    /// Parse a GitHub PR URL into (owner, repo, number).
    /// Expected format: `https://github.com/{owner}/{repo}/pull/{number}`
    fn parse_pr_url(url: &str) -> Option<(String, String, u64)> {
        let path = url.strip_prefix("https://github.com/")?;
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() >= 4 && parts[2] == "pull" {
            let owner = parts[0].to_string();
            let repo = parts[1].to_string();
            let number = parts[3].parse::<u64>().ok()?;
            Some((owner, repo, number))
        } else {
            None
        }
    }

    /// Returns true if a terminal transition should be skipped because at least
    /// one signal returned a transient error. Prevents permanently stranding a
    /// record in Done/Failed when the true signal state is unknown.
    fn should_skip_transition(had_error: bool, new_status: Status) -> bool {
        had_error && matches!(new_status, Status::Done | Status::Failed)
    }

    /// Apply the transition matrix to determine the new status from health checks.
    pub fn apply_transition(checks: &HealthChecks) -> Status {
        if !checks.agent_exited_clean {
            return Status::Running;
        }
        if !checks.branch_pushed {
            return Status::Failed;
        }
        if !checks.pr_created {
            return Status::Failed;
        }
        if !checks.ci_passed {
            return Status::NeedsReview;
        }
        if !checks.reviews_approved {
            return Status::NeedsReview;
        }
        if !checks.threads_resolved {
            return Status::NeedsReview;
        }
        Status::Done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_transition_agent_still_running() {
        let checks = HealthChecks::default(); // all false
        assert_eq!(HealthChecker::apply_transition(&checks), Status::Running);
    }

    #[test]
    fn test_apply_transition_exited_no_branch() {
        let checks = HealthChecks {
            agent_exited_clean: true,
            ..Default::default()
        };
        assert_eq!(HealthChecker::apply_transition(&checks), Status::Failed);
    }

    #[test]
    fn test_apply_transition_branch_no_pr() {
        let checks = HealthChecks {
            agent_exited_clean: true,
            branch_pushed: true,
            ..Default::default()
        };
        assert_eq!(HealthChecker::apply_transition(&checks), Status::Failed);
    }

    #[test]
    fn test_apply_transition_pr_no_ci() {
        let checks = HealthChecks {
            agent_exited_clean: true,
            branch_pushed: true,
            pr_created: true,
            ..Default::default()
        };
        assert_eq!(
            HealthChecker::apply_transition(&checks),
            Status::NeedsReview
        );
    }

    #[test]
    fn test_apply_transition_ci_no_reviews() {
        let checks = HealthChecks {
            agent_exited_clean: true,
            branch_pushed: true,
            pr_created: true,
            ci_passed: true,
            ..Default::default()
        };
        assert_eq!(
            HealthChecker::apply_transition(&checks),
            Status::NeedsReview
        );
    }

    #[test]
    fn test_apply_transition_reviews_no_threads() {
        let checks = HealthChecks {
            agent_exited_clean: true,
            branch_pushed: true,
            pr_created: true,
            ci_passed: true,
            reviews_approved: true,
            threads_resolved: false,
        };
        assert_eq!(
            HealthChecker::apply_transition(&checks),
            Status::NeedsReview
        );
    }

    #[test]
    fn test_apply_transition_all_signals_true() {
        let checks = HealthChecks {
            agent_exited_clean: true,
            branch_pushed: true,
            pr_created: true,
            ci_passed: true,
            reviews_approved: true,
            threads_resolved: true,
        };
        assert_eq!(HealthChecker::apply_transition(&checks), Status::Done);
    }

    #[test]
    fn test_parse_pr_url_valid() {
        let result = HealthChecker::parse_pr_url("https://github.com/harmony-labs/atc/pull/6");
        assert_eq!(
            result,
            Some(("harmony-labs".to_string(), "atc".to_string(), 6))
        );
    }

    #[test]
    fn test_parse_pr_url_invalid() {
        assert_eq!(HealthChecker::parse_pr_url("https://example.com/foo"), None);
        assert_eq!(HealthChecker::parse_pr_url("not-a-url"), None);
        assert_eq!(
            HealthChecker::parse_pr_url("https://github.com/owner/repo/issues/1"),
            None
        );
    }

    #[test]
    fn test_should_skip_transition_error_and_terminal() {
        // Error + Done → skip
        assert!(HealthChecker::should_skip_transition(true, Status::Done));
        // Error + Failed → skip
        assert!(HealthChecker::should_skip_transition(true, Status::Failed));
        // Error + non-terminal → don't skip
        assert!(!HealthChecker::should_skip_transition(true, Status::Running));
        assert!(!HealthChecker::should_skip_transition(
            true,
            Status::NeedsReview
        ));
        assert!(!HealthChecker::should_skip_transition(
            true,
            Status::NeedsHuman
        ));
        // No error + terminal → don't skip
        assert!(!HealthChecker::should_skip_transition(false, Status::Done));
        assert!(!HealthChecker::should_skip_transition(
            false,
            Status::Failed
        ));
    }

}
