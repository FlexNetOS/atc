use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::executor::AgentExecutor;
use atc_core::registry::Registry;
use atc_core::stream_json;
use atc_core::types::{DispatchOpts, Status};
use tracing::{info, warn};

/// Timeout for non-fatal subprocess calls (tmux, git-kb).
const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Execute the `atc retry` command.
pub async fn run_retry(
    config: &AtcConfig,
    registry: &dyn Registry,
    executor: &dyn AgentExecutor,
    slug: &str,
) -> Result<()> {
    // 1. Get the record
    let record = registry
        .get(slug)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no dispatch record found for slug: {slug}"))?;

    // 2. Only allow retrying failed or needs-human tasks
    match record.status {
        Status::Failed | Status::NeedsHuman => {}
        other => {
            anyhow::bail!(
                "cannot retry task {slug}: status is '{other}', expected 'failed' or 'needs-human'"
            );
        }
    }

    let max_retries = config.dispatch.max_retries;

    // 3. Check retry limit
    // NOTE: This check-then-act is inherently racy (TOCTOU) — two concurrent
    // `atc retry` calls could both pass the guard. This is acceptable because
    // (a) the CLI is single-user, (b) the window is tiny, and (c) the
    // consequence (one extra retry) is recoverable. A CAS-based
    // `increment_retries_if_below_max` would eliminate the race but adds
    // complexity to the Registry trait for minimal practical benefit.
    if record.retries >= max_retries {
        registry.update_status(slug, Status::NeedsHuman).await?;

        // Unassign in git-kb (non-fatal)
        kb_unassign(slug, config).await;

        anyhow::bail!("Task {slug} has reached max retries ({max_retries}). Marking needs-human.");
    }

    // 4. Classify failure: read last result event
    match stream_json::read_last_result(&record.log_file) {
        Ok(Some(event)) => {
            info!(
                slug,
                subtype = %event.subtype,
                "last result event: subtype={}",
                event.subtype
            );
        }
        Ok(None) => {
            warn!(slug, log_file = %record.log_file.display(), "no result event found in log");
        }
        Err(e) => {
            warn!(slug, error = %e, "failed to read log file for failure classification");
        }
    }

    // 5. Build new session and log file for retry
    let new_session = crate::dispatch::build_session_name(slug, &record.mode);
    let log_dir = config.dispatch.resolved_log_dir();
    let new_log_file = log_dir.join(format!("{}.jsonl", new_session));

    // 6. Increment retries in registry (atomic update)
    let now = chrono::Utc::now();
    registry
        .increment_retries(slug, &new_session, &new_log_file, now)
        .await?;

    // 7. Kill old tmux session (non-fatal, with timeout)
    match run_cmd_with_timeout(
        tokio::process::Command::new("tmux")
            .args(["kill-session", "-t", &record.session])
            .stderr(std::process::Stdio::null()),
    )
    .await
    {
        Ok(Some(s)) if !s.success() => {
            tracing::debug!(slug, session = %record.session, "tmux kill-session exited non-zero (session may not exist)");
        }
        Ok(None) => {
            tracing::debug!(slug, session = %record.session, "tmux kill-session timed out (non-fatal)");
        }
        Err(e) => {
            tracing::debug!(slug, error = %e, "tmux kill-session failed (non-fatal)");
        }
        _ => {}
    }

    // 8. Clear git-kb claim (non-fatal)
    kb_unassign(slug, config).await;
    kb_set_status_draft(slug, config).await;

    // 9. Re-dispatch
    let retry_num = record.retries + 1;
    println!("Re-dispatching {slug} (retry {retry_num}/{max_retries})...");

    let opts = DispatchOpts {
        slug: slug.to_string(),
        cli_mode: Some(record.mode.clone()),
        directive: None,
        inline: false,
    };

    let outcome = match crate::dispatch::dispatch(config, registry, executor, &opts).await {
        Ok(o) => o,
        Err(e) => {
            // Rollback: dispatch failed before an agent was spawned, so the task
            // would be stranded in Running with no active agent. Reset to Failed
            // so it can be retried again.
            warn!(slug, error = %e, "dispatch failed during retry; rolling back to Failed");
            let _ = registry.update_status(slug, Status::Failed).await;
            return Err(e);
        }
    };

    if let Some(code) = outcome.inline_exit_code {
        if code != 0 {
            anyhow::bail!("inline dispatch failed with exit code {code}");
        }
    }

    Ok(())
}

/// Run a command with a timeout, killing the child on timeout.
/// Returns `Ok(Some(status))` on normal exit, `Ok(None)` on timeout, `Err` on spawn failure.
async fn run_cmd_with_timeout(
    cmd: &mut tokio::process::Command,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    let mut child = cmd.kill_on_drop(true).spawn()?;
    match tokio::time::timeout(CMD_TIMEOUT, child.wait()).await {
        Ok(status) => status.map(Some),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Ok(None)
        }
    }
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
    use std::path::PathBuf;
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
        async fn update_status(&self, slug: &str, status: Status) -> Result<()> {
            let mut records = self.records.lock().unwrap();
            for r in records.iter_mut() {
                if r.slug == slug {
                    r.status = status;
                    return Ok(());
                }
            }
            anyhow::bail!("no dispatch record found for slug: {slug}")
        }
        async fn update_cost(
            &self,
            _slug: &str,
            _cost: f64,
            _turns: u32,
            _duration_ms: u64,
        ) -> Result<()> {
            Ok(())
        }
        async fn get(&self, slug: &str) -> Result<Option<DispatchRecord>> {
            Ok(self
                .records
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.slug == slug)
                .cloned())
        }
        async fn list(&self, _filter: StatusFilter) -> Result<Vec<DispatchRecord>> {
            Ok(self.records.lock().unwrap().clone())
        }
        async fn update_health(
            &self,
            _slug: &str,
            _checks: &HealthChecks,
            _status: Status,
            _updated_at: chrono::DateTime<Utc>,
        ) -> Result<()> {
            Ok(())
        }
        async fn set_pr_url(&self, _slug: &str, _url: &str) -> Result<()> {
            Ok(())
        }
        async fn increment_retries(
            &self,
            slug: &str,
            new_session: &str,
            new_log_file: &std::path::Path,
            new_dispatched_at: chrono::DateTime<Utc>,
        ) -> Result<()> {
            let mut records = self.records.lock().unwrap();
            for r in records.iter_mut() {
                if r.slug == slug {
                    r.retries += 1;
                    r.session = new_session.to_string();
                    r.log_file = new_log_file.to_path_buf();
                    r.dispatched_at = new_dispatched_at;
                    r.status = Status::Running;
                    return Ok(());
                }
            }
            anyhow::bail!("no dispatch record found for slug: {slug}")
        }
    }

    struct MockExecutor;

    #[async_trait]
    impl AgentExecutor for MockExecutor {
        async fn spawn(&self, _opts: &AgentOpts) -> Result<AgentHandle> {
            // This won't be reached in the max-retries test
            anyhow::bail!("mock executor: not implemented")
        }
    }

    fn sample_record(slug: &str, retries: u32) -> DispatchRecord {
        DispatchRecord {
            slug: slug.to_string(),
            branch: "test-branch".to_string(),
            worktree_path: PathBuf::from("/tmp/test"),
            session: "test-session".to_string(),
            log_file: PathBuf::from("/tmp/nonexistent-test.jsonl"),
            status: Status::Failed,
            mode: Mode::Implement,
            retries,
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
        let record = sample_record("tasks/test-1", 3); // already at max
        let registry = MockRegistry::new(vec![record]);
        let executor = MockExecutor;
        let config = AtcConfig::default(); // max_retries = 3

        let result = run_retry(&config, &registry, &executor, "tasks/test-1").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("max retries"),
            "expected max retries error, got: {err}"
        );

        // Status should be NeedsHuman
        let r = registry.get("tasks/test-1").await.unwrap().unwrap();
        assert_eq!(r.status, Status::NeedsHuman);
    }

    #[tokio::test]
    async fn test_retry_unknown_slug_errors() {
        let registry = MockRegistry::new(vec![]);
        let executor = MockExecutor;
        let config = AtcConfig::default();

        let result = run_retry(&config, &registry, &executor, "tasks/nonexistent").await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no dispatch record found"));
    }

    #[tokio::test]
    async fn test_retry_below_max_retries_increments() {
        let record = sample_record("tasks/test-1", 1); // below max of 3
        let registry = MockRegistry::new(vec![record]);
        let executor = MockExecutor;
        let config = AtcConfig::default();

        // This will fail at the dispatch step (mock executor),
        // but increment_retries should have been called first.
        // Actually, dispatch will fail because git-kb/meta aren't available.
        // But we can verify that the retry limit check passed.
        let result = run_retry(&config, &registry, &executor, "tasks/test-1").await;
        // It will error somewhere in the dispatch flow, but NOT at the max retries check
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            !err.contains("max retries"),
            "should not hit max retries with retries=1, got: {err}"
        );

        // Retries should have been incremented
        let r = registry.get("tasks/test-1").await.unwrap().unwrap();
        assert_eq!(r.retries, 2);
    }

    #[tokio::test]
    async fn test_retry_rejects_running_task() {
        let mut record = sample_record("tasks/test-1", 0);
        record.status = Status::Running;
        let registry = MockRegistry::new(vec![record]);
        let executor = MockExecutor;
        let config = AtcConfig::default();

        let result = run_retry(&config, &registry, &executor, "tasks/test-1").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("cannot retry"),
            "expected status guard error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_retry_rejects_done_task() {
        let mut record = sample_record("tasks/test-1", 0);
        record.status = Status::Done;
        let registry = MockRegistry::new(vec![record]);
        let executor = MockExecutor;
        let config = AtcConfig::default();

        let result = run_retry(&config, &registry, &executor, "tasks/test-1").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("cannot retry"),
            "expected status guard error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_retry_accepts_needs_human_task() {
        let mut record = sample_record("tasks/test-1", 0);
        record.status = Status::NeedsHuman;
        let registry = MockRegistry::new(vec![record]);
        let executor = MockExecutor;
        let config = AtcConfig::default();

        // Should pass the status guard (NeedsHuman is retryable).
        // Will fail later in the dispatch flow, but NOT at the status check.
        let result = run_retry(&config, &registry, &executor, "tasks/test-1").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            !err.contains("cannot retry"),
            "NeedsHuman should be retryable, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_retry_configurable_max_retries() {
        let record = sample_record("tasks/test-1", 1);
        let registry = MockRegistry::new(vec![record]);
        let executor = MockExecutor;
        let mut config = AtcConfig::default();
        config.dispatch.max_retries = 1; // set max to 1

        let result = run_retry(&config, &registry, &executor, "tasks/test-1").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("max retries"),
            "expected max retries error with max=1 and retries=1, got: {err}"
        );
    }
}
