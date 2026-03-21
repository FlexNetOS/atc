use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::executor::AgentExecutor;
use atc_core::registry::Registry;
use atc_core::stream_json;
use atc_core::types::{DispatchOpts, Status};
use tracing::{info, warn};

use crate::subprocess::run_cmd_with_timeout;

/// Timeout for non-fatal subprocess calls (tmux, git-kb).
const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Resolve a dispatch record by ID or task slug.
async fn resolve_record(
    registry: &dyn Registry,
    arg: &str,
) -> Result<atc_core::types::DispatchRecord> {
    if let Some(record) = registry.get(arg).await? {
        return Ok(record);
    }
    if let Some(record) = registry.find_latest_for_task(arg).await? {
        return Ok(record);
    }
    anyhow::bail!("no dispatch record found for: {arg}")
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

    // 4. Classify failure: read last result event
    match stream_json::read_last_result(&record.log_file) {
        Ok(Some(event)) => {
            info!(
                id,
                subtype = %event.subtype,
                "last result event: subtype={}",
                event.subtype
            );
        }
        Ok(None) => {
            warn!(id, log_file = %record.log_file.display(), "no result event found in log");
        }
        Err(e) => {
            warn!(id, error = %e, "failed to read log file for failure classification");
        }
    }

    let slug = record.task_slug.as_deref().ok_or_else(|| {
        anyhow::anyhow!("cannot retry dispatch {id}: this dispatch has no task slug")
    })?;

    // 5. Kill old tmux session (non-fatal, with timeout)
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

    // 6. Clear git-kb claim (non-fatal)
    if let Some(ref task_slug) = record.task_slug {
        kb_unassign(task_slug, config).await;
        kb_set_status_draft(task_slug, config).await;
    }

    // 7. Re-dispatch
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
        max_budget_override: None,
        max_turns_override: None,
        retries: record.retries + 1,
    };

    let outcome = match crate::dispatch::dispatch(config, registry, executor, &opts).await {
        Ok(o) => o,
        Err(e) => {
            warn!(id, error = %e, "dispatch failed during retry; rolling back to Failed");
            if let Err(rollback_err) = registry.update_status(id, Status::Failed).await {
                warn!(
                    id,
                    error = %rollback_err,
                    "failed to rollback status to Failed after dispatch error"
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
}
