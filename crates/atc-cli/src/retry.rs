use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::executor::AgentExecutor;
use atc_core::registry::Registry;
use atc_core::stream_json;
use atc_core::types::{DispatchOpts, Status};
use tracing::{info, warn};

use crate::resolve::resolve_record;
use crate::subprocess::run_cmd_with_timeout;

/// Timeout for non-fatal subprocess calls (tmux, git-kb).
const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Compute budget/turns overrides based on the failure subtype from the last
/// result event in the log file.
///
/// Returns `(max_budget_override, max_turns_override)`.
///
/// Strategy: give the agent more runway on resource-limit failures by doubling
/// the relevant limit from the *static config* value. This is a one-step
/// increase (not exponential across retries) — `max_retries` bounds total spend.
fn classify_failure_overrides(
    config: &AtcConfig,
    mode: &str,
    log_file: &std::path::Path,
    id: &str,
) -> (Option<f64>, Option<u32>) {
    match stream_json::read_last_result(log_file) {
        Ok(Some(event)) => {
            info!(
                id,
                subtype = %event.subtype,
                "last result event: subtype={}",
                event.subtype
            );
            compute_overrides(config, mode, &event.subtype)
        }
        Ok(None) => {
            warn!(id, log_file = %log_file.display(), "no result event found in log");
            println!("  No result event found. Retrying with same configuration.");
            (None, None)
        }
        Err(e) => {
            warn!(id, error = %e, "failed to read log file for failure classification");
            println!("  Could not read log. Retrying with same configuration.");
            (None, None)
        }
    }
}

/// Pure computation of overrides from a failure subtype. Separated for testability.
fn compute_overrides(config: &AtcConfig, mode: &str, subtype: &str) -> (Option<f64>, Option<u32>) {
    match subtype {
        "error_max_turns" => {
            let current = config
                .modes
                .get(mode)
                .and_then(|m| m.max_turns)
                .unwrap_or(config.dispatch.max_turns);
            let doubled = current.saturating_mul(2);
            println!(
                "  Failure: max turns reached. Retrying with doubled max_turns ({current} → {doubled})."
            );
            (None, Some(doubled))
        }
        "error_max_budget_usd" => {
            let current = config
                .modes
                .get(mode)
                .and_then(|m| m.max_budget_usd)
                .unwrap_or(config.dispatch.max_budget_usd);
            let doubled = current * 2.0;
            println!(
                "  Failure: budget exceeded. Retrying with doubled budget (${current:.2} → ${doubled:.2})."
            );
            (Some(doubled), None)
        }
        other => {
            println!("  Failure: {other}. Retrying with same configuration.");
            (None, None)
        }
    }
}

/// Execute the `atc retry` command.
pub async fn run_retry(
    config: &AtcConfig,
    registry: &dyn Registry,
    executor: &dyn AgentExecutor,
    arg: &str,
) -> Result<()> {
    // 1. Get the record
    let record = resolve_record(registry, arg).await?;
    let id = &record.id;

    // 2. Only allow retrying failed or needs-human tasks
    match record.status {
        Status::Failed | Status::NeedsHuman => {}
        other => {
            anyhow::bail!(
                "cannot retry dispatch {id}: status is '{other}', expected 'failed' or 'needs-human'"
            );
        }
    }

    let max_retries = config.dispatch.max_retries;

    // 3. Check retry limit
    if record.retries >= max_retries {
        registry.update_status(id, Status::NeedsHuman).await?;

        // Unassign in git-kb (non-fatal)
        if let Some(ref slug) = record.task_slug {
            kb_unassign(slug, config).await;
        }

        anyhow::bail!(
            "Dispatch {id} has reached max retries ({max_retries}). Marking needs-human."
        );
    }

    // 4. Require a task slug before doing any I/O
    let slug = record.task_slug.as_deref().ok_or_else(|| {
        anyhow::anyhow!("cannot retry dispatch {id}: this dispatch has no task slug")
    })?;

    // 5. Classify failure and compute budget/turns adjustments
    let (max_budget_override, max_turns_override) =
        classify_failure_overrides(config, record.mode.as_str(), &record.log_file, id);

    // 6. Kill old tmux session (non-fatal, with timeout)
    match run_cmd_with_timeout(
        tokio::process::Command::new("tmux")
            .args(["kill-session", "-t", &record.session])
            .stderr(std::process::Stdio::null()),
        CMD_TIMEOUT,
    )
    .await
    {
        Ok(Some(s)) if !s.success() => {
            tracing::debug!(id, session = %record.session, "tmux kill-session exited non-zero (session may not exist)");
        }
        Ok(None) => {
            tracing::debug!(id, session = %record.session, "tmux kill-session timed out (non-fatal)");
        }
        Err(e) => {
            tracing::debug!(id, error = %e, "tmux kill-session failed (non-fatal)");
        }
        _ => {}
    }

    // 7. Clear git-kb claim (non-fatal)
    if let Some(ref task_slug) = record.task_slug {
        kb_unassign(task_slug, config).await;
        kb_set_status_draft(task_slug, config).await;
    }

    // 8. Re-dispatch
    let retry_num = record.retries + 1;
    println!("Re-dispatching {slug} (retry {retry_num}/{max_retries})...");

    let opts = DispatchOpts {
        slug: slug.to_string(),
        cli_mode: Some(record.mode.clone()),
        directive: None,
        pr_url: record.pr_url.clone(),
        inline: false,
        force: false,
        dry_run: false,
        max_budget_override,
        max_turns_override,
        retries: record.retries + 1,
    };

    let original_status = record.status;
    let outcome = match crate::dispatch::dispatch(config, registry, executor, &opts).await {
        Ok(o) => o,
        Err(e) => {
            warn!(id, error = %e, "dispatch failed during retry; rolling back to {original_status}");
            if let Err(rollback_err) = registry.update_status(id, original_status).await {
                warn!(
                    id,
                    error = %rollback_err,
                    "failed to rollback status to {original_status} after dispatch error"
                );
                anyhow::bail!(
                    "dispatch failed during retry ({e}); additionally failed to rollback status: {rollback_err}"
                );
            }
            return Err(e);
        }
    };

    // Mark the old record as Stopped so it can't be retried again by ID.
    // The new dispatch record carries the incremented retries counter.
    if let Err(e) = registry.update_status(id, Status::Stopped).await {
        warn!(id, error = %e, "failed to mark old record as Stopped after retry (non-fatal)");
    }

    if let Some(code) = outcome.inline_exit_code {
        if code != 0 {
            anyhow::bail!("inline dispatch failed with exit code {code}");
        }
    }

    Ok(())
}

/// Non-fatal: unassign task in git-kb.
async fn kb_unassign(slug: &str, config: &AtcConfig) {
    let kb_root = match config
        .dispatch
        .resolved_meta_workspace_root(config.config_dir.as_deref())
    {
        Ok(r) => r,
        Err(_) => return,
    };

    let status = run_cmd_with_timeout(
        tokio::process::Command::new("git-kb")
            .args(["unassign", slug])
            .env("GITKB_ROOT", &kb_root),
        CMD_TIMEOUT,
    )
    .await;

    match status {
        Ok(Some(s)) if !s.success() => {
            warn!(slug, "git-kb unassign failed (non-fatal)");
        }
        Ok(None) => {
            warn!(slug, "git-kb unassign timed out (non-fatal)");
        }
        Err(e) => {
            warn!(slug, error = %e, "git-kb unassign failed (non-fatal)");
        }
        _ => {}
    }
}

/// Non-fatal: set task status to draft in git-kb.
async fn kb_set_status_draft(slug: &str, config: &AtcConfig) {
    let kb_root = match config
        .dispatch
        .resolved_meta_workspace_root(config.config_dir.as_deref())
    {
        Ok(r) => r,
        Err(_) => return,
    };

    let status = run_cmd_with_timeout(
        tokio::process::Command::new("git-kb")
            .args(["set", slug, "status=draft"])
            .env("GITKB_ROOT", &kb_root),
        CMD_TIMEOUT,
    )
    .await;

    match status {
        Ok(Some(s)) if !s.success() => {
            warn!(slug, "git-kb set status=draft failed (non-fatal)");
        }
        Ok(None) => {
            warn!(slug, "git-kb set status=draft timed out (non-fatal)");
        }
        Err(e) => {
            warn!(slug, error = %e, "git-kb set status=draft failed (non-fatal)");
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use atc_core::executor::{AgentHandle, AgentOpts};
    use atc_core::registry::StatusFilter;
    use atc_core::types::{DispatchRecord, HealthChecks, Mode};
    use chrono::Utc;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    struct MockRegistry {
        records: Mutex<Vec<DispatchRecord>>,
    }

    impl MockRegistry {
        fn new(records: Vec<DispatchRecord>) -> Self {
            Self {
                records: Mutex::new(records),
            }
        }
    }

    #[async_trait]
    impl Registry for MockRegistry {
        async fn insert(&self, record: &DispatchRecord) -> Result<()> {
            self.records.lock().unwrap().push(record.clone());
            Ok(())
        }
        async fn update_status(&self, id: &str, status: Status) -> Result<()> {
            let mut records = self.records.lock().unwrap();
            for r in records.iter_mut() {
                if r.id == id {
                    r.status = status;
                    return Ok(());
                }
            }
            anyhow::bail!("no dispatch record found for id: {id}")
        }
        async fn update_cost(&self, _: &str, _: f64, _: u32, _: u64) -> Result<()> {
            Ok(())
        }
        async fn get(&self, id: &str) -> Result<Option<DispatchRecord>> {
            Ok(self
                .records
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.id == id)
                .cloned())
        }
        async fn list(&self, _filter: StatusFilter) -> Result<Vec<DispatchRecord>> {
            Ok(self.records.lock().unwrap().clone())
        }
        async fn update_health(
            &self,
            _: &str,
            _: &HealthChecks,
            _: Status,
            _: chrono::DateTime<Utc>,
        ) -> Result<()> {
            Ok(())
        }
        async fn set_pr_url(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn set_artifacts(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn increment_retries(
            &self,
            id: &str,
            new_session: &str,
            new_log_file: &Path,
            new_dispatched_at: chrono::DateTime<Utc>,
        ) -> Result<()> {
            let mut records = self.records.lock().unwrap();
            for r in records.iter_mut() {
                if r.id == id {
                    r.retries += 1;
                    r.session = new_session.to_string();
                    r.log_file = new_log_file.to_path_buf();
                    r.dispatched_at = new_dispatched_at;
                    r.status = Status::Running;
                    return Ok(());
                }
            }
            anyhow::bail!("no dispatch record found for id: {id}")
        }
        async fn find_by_branch(&self, _: &str) -> Result<Vec<DispatchRecord>> {
            Ok(vec![])
        }
        async fn find_by_task_slug(&self, _: &str) -> Result<Vec<DispatchRecord>> {
            Ok(vec![])
        }
        async fn find_by_pr_url(&self, _: &str) -> Result<Vec<DispatchRecord>> {
            Ok(vec![])
        }
        async fn find_by_worktree(&self, _: &Path) -> Result<Vec<DispatchRecord>> {
            Ok(vec![])
        }
        async fn find_latest_for_task(&self, task_slug: &str) -> Result<Option<DispatchRecord>> {
            Ok(self
                .records
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.task_slug.as_deref() == Some(task_slug))
                .max_by_key(|r| r.dispatched_at)
                .cloned())
        }
        async fn find_running_on_worktree(&self, _: &Path) -> Result<Vec<DispatchRecord>> {
            Ok(vec![])
        }
    }

    struct MockExecutor;

    #[async_trait]
    impl AgentExecutor for MockExecutor {
        async fn spawn(&self, _opts: &AgentOpts) -> Result<AgentHandle> {
            anyhow::bail!("mock executor: not implemented")
        }
    }

    fn sample_record(id: &str, retries: u32) -> DispatchRecord {
        DispatchRecord {
            id: id.to_string(),
            task_slug: Some("tasks/test-1".to_string()),
            branch: "test-branch".to_string(),
            worktree_path: PathBuf::from("/tmp/test"),
            session: "test-session".to_string(),
            log_file: PathBuf::from("/tmp/nonexistent-test.jsonl"),
            status: Status::Failed,
            mode: Mode::Implement,
            retries,
            resolver: "task".to_string(),
            pr_url: None,
            checks: HealthChecks::default(),
            cost_usd: None,
            num_turns: None,
            duration_ms: None,
            dispatched_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_retry_max_retries_marks_needs_human() {
        let record = sample_record("test-id-1", 3); // already at max
        let registry = MockRegistry::new(vec![record]);
        let executor = MockExecutor;
        let config = AtcConfig::default(); // max_retries = 3

        let result = run_retry(&config, &registry, &executor, "test-id-1").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("max retries"),
            "expected max retries error, got: {err}"
        );

        let r = registry.get("test-id-1").await.unwrap().unwrap();
        assert_eq!(r.status, Status::NeedsHuman);
    }

    #[tokio::test]
    async fn test_retry_unknown_id_errors() {
        let registry = MockRegistry::new(vec![]);
        let executor = MockExecutor;
        let config = AtcConfig::default();

        let result = run_retry(&config, &registry, &executor, "nonexistent").await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no dispatch record found"));
    }

    #[tokio::test]
    async fn test_retry_rejects_running_task() {
        let mut record = sample_record("test-id-1", 0);
        record.status = Status::Running;
        let registry = MockRegistry::new(vec![record]);
        let executor = MockExecutor;
        let config = AtcConfig::default();

        let result = run_retry(&config, &registry, &executor, "test-id-1").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("cannot retry"),
            "expected status guard error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_retry_rejects_done_task() {
        let mut record = sample_record("test-id-1", 0);
        record.status = Status::Done;
        let registry = MockRegistry::new(vec![record]);
        let executor = MockExecutor;
        let config = AtcConfig::default();

        let result = run_retry(&config, &registry, &executor, "test-id-1").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("cannot retry"),
            "expected status guard error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_retry_accepts_needs_human_status() {
        let mut record = sample_record("test-id-1", 0);
        record.status = Status::NeedsHuman;
        let registry = MockRegistry::new(vec![record]);
        let executor = MockExecutor;
        let config = AtcConfig::default();

        // NeedsHuman should be accepted as retryable (not rejected by status guard).
        // It will fail later at dispatch (mock executor), but crucially does NOT
        // fail with "cannot retry".
        let result = run_retry(&config, &registry, &executor, "test-id-1").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            !err.contains("cannot retry"),
            "NeedsHuman should be retryable, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_retry_configurable_max_retries() {
        // With max_retries=5 and retries=3, the retry should proceed (not hit the limit)
        let record = sample_record("test-id-1", 3);
        let registry = MockRegistry::new(vec![record]);
        let executor = MockExecutor;
        let mut config = AtcConfig::default();
        config.dispatch.max_retries = 5;

        let result = run_retry(&config, &registry, &executor, "test-id-1").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        // Should NOT hit max retries — should fail at dispatch (mock executor) instead
        assert!(
            !err.contains("max retries"),
            "expected dispatch error (not max retries), got: {err}"
        );
    }

    #[tokio::test]
    async fn test_retry_rejects_no_task_slug() {
        let mut record = sample_record("test-id-1", 0);
        record.task_slug = None;
        let registry = MockRegistry::new(vec![record]);
        let executor = MockExecutor;
        let config = AtcConfig::default();

        let result = run_retry(&config, &registry, &executor, "test-id-1").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("no task slug"),
            "expected no-task-slug error, got: {err}"
        );
    }

    // --- Failure classification tests ---

    #[test]
    fn test_compute_overrides_error_max_turns_doubles() {
        let config = AtcConfig::default(); // max_turns = 10_000
        let (budget, turns) = compute_overrides(&config, "implement", "error_max_turns");
        assert_eq!(budget, None);
        assert_eq!(turns, Some(20_000));
    }

    #[test]
    fn test_compute_overrides_error_max_budget_doubles() {
        let config = AtcConfig::default(); // max_budget_usd = 25.0
        let (budget, turns) = compute_overrides(&config, "implement", "error_max_budget_usd");
        assert_eq!(budget, Some(50.0));
        assert_eq!(turns, None);
    }

    #[test]
    fn test_compute_overrides_unknown_subtype_no_overrides() {
        let config = AtcConfig::default();
        let (budget, turns) = compute_overrides(&config, "implement", "error_something_else");
        assert_eq!(budget, None);
        assert_eq!(turns, None);
    }

    #[test]
    fn test_compute_overrides_success_no_overrides() {
        let config = AtcConfig::default();
        let (budget, turns) = compute_overrides(&config, "implement", "success");
        assert_eq!(budget, None);
        assert_eq!(turns, None);
    }

    #[test]
    fn test_compute_overrides_uses_mode_specific_config() {
        let mut config = AtcConfig::default();
        config.modes.insert(
            "research".to_string(),
            atc_core::config::ModeConfig {
                template_path: None,
                template_inline: None,
                max_turns: Some(500),
                max_budget_usd: Some(5.0),
                components: None,
            },
        );

        let (budget, turns) = compute_overrides(&config, "research", "error_max_turns");
        assert_eq!(turns, Some(1_000)); // 500 * 2
        assert_eq!(budget, None);

        let (budget, turns) = compute_overrides(&config, "research", "error_max_budget_usd");
        assert_eq!(budget, Some(10.0)); // 5.0 * 2
        assert_eq!(turns, None);
    }

    #[test]
    fn test_compute_overrides_falls_back_to_dispatch_defaults() {
        let config = AtcConfig::default();
        // "nonexistent" mode falls back to dispatch defaults
        let (_, turns) = compute_overrides(&config, "nonexistent", "error_max_turns");
        assert_eq!(turns, Some(config.dispatch.max_turns * 2));

        let (budget, _) = compute_overrides(&config, "nonexistent", "error_max_budget_usd");
        assert_eq!(budget, Some(config.dispatch.max_budget_usd * 2.0));
    }
}
