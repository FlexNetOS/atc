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

pub struct HealthChecker {
    pub registry: Arc<dyn Registry>,
    pub config: Arc<AtcConfig>,
    pub git_bin: PathBuf,
    pub gh_bin: PathBuf,
    pub tmux_bin: PathBuf,
}

impl HealthChecker {
    /// Evaluate all active records and apply transitions.
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
            let registry = self.registry.clone();
            let config = self.config.clone();
            let git_bin = self.git_bin.clone();
            let gh_bin = self.gh_bin.clone();
            let tmux_bin = self.tmux_bin.clone();

            handles.push(tokio::spawn(async move {
                let checker = HealthChecker {
                    registry,
                    config,
                    git_bin,
                    gh_bin,
                    tmux_bin,
                };
                checker.check_record(record).await
            }));
        }

        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await??);
        }
        Ok(results)
    }

    async fn check_record(&self, mut record: DispatchRecord) -> Result<HealthResult> {
        let old_checks = record.checks.clone();
        let old_status = record.status.clone();

        // Signal 1: agent_exited_clean
        let agent_exited_clean = self.eval_signal_1(&record).await;
        let mut checks = HealthChecks {
            agent_exited_clean,
            ..Default::default()
        };

        if !agent_exited_clean {
            // short-circuit: all downstream stays false
        } else {
            // Signal 2: branch_pushed
            checks.branch_pushed = self.eval_signal_2(&record).await;
            if !checks.branch_pushed {
                // short-circuit: downstream stays false
            } else {
                // Signal 3: pr_created (may update pr_url)
                checks.pr_created = self.eval_signal_3(&mut record).await;
                if !checks.pr_created {
                    // short-circuit
                } else {
                    // Signal 4: ci_passed
                    checks.ci_passed = self.eval_signal_4(&record).await;
                    if !checks.ci_passed {
                        // short-circuit
                    } else {
                        // Signal 5: reviews_approved
                        checks.reviews_approved = self.eval_signal_5(&record).await;
                        if !checks.reviews_approved {
                            // short-circuit
                        } else {
                            // Signal 6: threads_resolved
                            checks.threads_resolved = self.eval_signal_6(&record).await;
                        }
                    }
                }
            }
        }

        let new_status = Self::apply_transition(&checks);

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
                    record.status.clone(),
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
    async fn eval_signal_1(&self, record: &DispatchRecord) -> bool {
        let timeout = Duration::from_secs(self.config.health.signal_timeout_secs);
        let result = tokio::time::timeout(
            timeout,
            tokio::process::Command::new(&self.tmux_bin)
                .args(["has-session", "-t", &record.session])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status(),
        )
        .await;

        match result {
            Ok(Ok(status)) => {
                if status.success() {
                    // Session exists — agent still running
                    debug!(slug = %record.slug, "signal 1: tmux session exists, agent running");
                    false
                } else {
                    // Session gone — agent finished
                    debug!(slug = %record.slug, "signal 1: tmux session gone, agent exited");
                    true
                }
            }
            Ok(Err(e)) => {
                warn!(slug = %record.slug, error = %e, "signal 1: tmux command failed");
                false
            }
            Err(_) => {
                warn!(slug = %record.slug, "signal 1: tmux command timed out");
                false
            }
        }
    }

    /// Signal 2: Check if branch exists on remote.
    async fn eval_signal_2(&self, record: &DispatchRecord) -> bool {
        let timeout = Duration::from_secs(self.config.health.signal_timeout_secs);
        let result = tokio::time::timeout(
            timeout,
            tokio::process::Command::new(&self.git_bin)
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
                .stderr(std::process::Stdio::null())
                .status(),
        )
        .await;

        match result {
            Ok(Ok(status)) => {
                let code = status.code().unwrap_or(-1);
                match code {
                    0 => {
                        debug!(slug = %record.slug, "signal 2: branch exists on remote");
                        true
                    }
                    2 => {
                        debug!(slug = %record.slug, "signal 2: branch not found on remote");
                        false
                    }
                    other => {
                        warn!(slug = %record.slug, exit_code = other, "signal 2: git ls-remote unexpected exit code");
                        false
                    }
                }
            }
            Ok(Err(e)) => {
                warn!(slug = %record.slug, error = %e, "signal 2: git ls-remote failed");
                false
            }
            Err(_) => {
                warn!(slug = %record.slug, "signal 2: git ls-remote timed out");
                false
            }
        }
    }

    /// Signal 3: Check if PR exists for the branch. Updates pr_url in registry if discovered.
    async fn eval_signal_3(&self, record: &mut DispatchRecord) -> bool {
        // If pr_url already known, skip the gh call
        if record.pr_url.is_some() {
            debug!(slug = %record.slug, "signal 3: pr_url already known, skipping gh call");
            return true;
        }

        let timeout = Duration::from_secs(self.config.health.signal_timeout_secs);
        let result = tokio::time::timeout(
            timeout,
            tokio::process::Command::new(&self.gh_bin)
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
                .stderr(std::process::Stdio::null())
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                if !output.status.success() {
                    warn!(slug = %record.slug, "signal 3: gh pr list failed");
                    return false;
                }
                let stdout = String::from_utf8_lossy(&output.stdout);
                let trimmed = stdout.trim();
                if trimmed.is_empty() || trimmed == "null" {
                    debug!(slug = %record.slug, "signal 3: no PR found");
                    return false;
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
                        return true;
                    }
                }
                warn!(slug = %record.slug, output = %trimmed, "signal 3: could not parse PR URL from gh output");
                false
            }
            Ok(Err(e)) => {
                warn!(slug = %record.slug, error = %e, "signal 3: gh pr list command failed");
                false
            }
            Err(_) => {
                warn!(slug = %record.slug, "signal 3: gh pr list timed out");
                false
            }
        }
    }

    /// Signal 4: Check if all CI checks passed on the PR.
    async fn eval_signal_4(&self, record: &DispatchRecord) -> bool {
        let pr_url = match &record.pr_url {
            Some(url) => url,
            None => return false,
        };

        let timeout = Duration::from_secs(self.config.health.signal_timeout_secs);
        let result = tokio::time::timeout(
            timeout,
            tokio::process::Command::new(&self.gh_bin)
                .args([
                    "pr",
                    "checks",
                    pr_url,
                    "--json",
                    "name,state",
                    "--jq",
                    "[.[] | select(.state != \"SUCCESS\")] | length",
                ])
                .current_dir(&record.worktree_path)
                .stderr(std::process::Stdio::null())
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                if !output.status.success() {
                    warn!(slug = %record.slug, "signal 4: gh pr checks failed");
                    return false;
                }
                let stdout = String::from_utf8_lossy(&output.stdout);
                let trimmed = stdout.trim();
                match trimmed.parse::<u64>() {
                    Ok(0) => {
                        debug!(slug = %record.slug, "signal 4: all CI checks passed");
                        true
                    }
                    Ok(n) => {
                        debug!(slug = %record.slug, failing = n, "signal 4: CI checks not all passing");
                        false
                    }
                    Err(_) => {
                        warn!(slug = %record.slug, output = %trimmed, "signal 4: could not parse gh pr checks output");
                        false
                    }
                }
            }
            Ok(Err(e)) => {
                warn!(slug = %record.slug, error = %e, "signal 4: gh pr checks command failed");
                false
            }
            Err(_) => {
                warn!(slug = %record.slug, "signal 4: gh pr checks timed out");
                false
            }
        }
    }

    /// Signal 5: Check if PR reviews are approved.
    async fn eval_signal_5(&self, record: &DispatchRecord) -> bool {
        let pr_url = match &record.pr_url {
            Some(url) => url,
            None => return false,
        };

        let timeout = Duration::from_secs(self.config.health.signal_timeout_secs);
        let result = tokio::time::timeout(
            timeout,
            tokio::process::Command::new(&self.gh_bin)
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
                .stderr(std::process::Stdio::null())
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                if !output.status.success() {
                    warn!(slug = %record.slug, "signal 5: gh pr view failed");
                    return false;
                }
                let stdout = String::from_utf8_lossy(&output.stdout);
                let trimmed = stdout.trim();
                match trimmed {
                    "APPROVED" => {
                        debug!(slug = %record.slug, "signal 5: reviews approved");
                        true
                    }
                    "" | "null" => {
                        // No review policy — treat as approved
                        debug!(slug = %record.slug, "signal 5: no review policy, treating as approved");
                        true
                    }
                    other => {
                        debug!(slug = %record.slug, decision = %other, "signal 5: reviews not approved");
                        false
                    }
                }
            }
            Ok(Err(e)) => {
                warn!(slug = %record.slug, error = %e, "signal 5: gh pr view command failed");
                false
            }
            Err(_) => {
                warn!(slug = %record.slug, "signal 5: gh pr view timed out");
                false
            }
        }
    }

    /// Signal 6: Check if all review threads are resolved.
    async fn eval_signal_6(&self, record: &DispatchRecord) -> bool {
        let pr_url = match &record.pr_url {
            Some(url) => url,
            None => return false,
        };

        let timeout = Duration::from_secs(self.config.health.signal_timeout_secs);
        let result = tokio::time::timeout(
            timeout,
            tokio::process::Command::new(&self.gh_bin)
                .args([
                    "pr",
                    "view",
                    pr_url,
                    "--json",
                    "reviewThreads",
                    "--jq",
                    "[.reviewThreads[] | select(.isResolved == false)] | length",
                ])
                .current_dir(&record.worktree_path)
                .stderr(std::process::Stdio::null())
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                if !output.status.success() {
                    warn!(slug = %record.slug, "signal 6: gh pr view reviewThreads failed");
                    return false;
                }
                let stdout = String::from_utf8_lossy(&output.stdout);
                let trimmed = stdout.trim();
                match trimmed.parse::<u64>() {
                    Ok(0) => {
                        debug!(slug = %record.slug, "signal 6: all threads resolved");
                        true
                    }
                    Ok(n) => {
                        debug!(slug = %record.slug, unresolved = n, "signal 6: unresolved threads remain");
                        false
                    }
                    Err(_) => {
                        warn!(slug = %record.slug, output = %trimmed, "signal 6: could not parse gh output");
                        false
                    }
                }
            }
            Ok(Err(e)) => {
                warn!(slug = %record.slug, error = %e, "signal 6: gh pr view command failed");
                false
            }
            Err(_) => {
                warn!(slug = %record.slug, "signal 6: gh pr view timed out");
                false
            }
        }
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
}
