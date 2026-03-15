use anyhow::Result;
use atc_core::registry::Registry;
use atc_core::types::Status;
use tracing::warn;

/// Execute the `atc redirect` command.
pub async fn run_redirect(registry: &dyn Registry, slug: &str, message: &str) -> Result<()> {
    // 1. Get the record
    let record = registry
        .get(slug)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no dispatch record found for slug: {slug}"))?;

    // 2. Warn if not Running (but don't hard-error)
    if record.status != Status::Running {
        warn!(
            slug,
            status = %record.status,
            "record status is not running; redirect may not reach the agent"
        );
        eprintln!(
            "warning: [{slug}] status is '{}', not 'running'",
            record.status
        );
    }

    let session_name = &record.session;

    // 3. Check if tmux session exists (with timeout)
    let has_session = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new("tmux")
            .args(["has-session", "-t", session_name])
            .status(),
    )
    .await;

    match has_session {
        Ok(Ok(s)) if !s.success() => {
            anyhow::bail!("No active tmux session: {session_name}");
        }
        Ok(Err(e)) => {
            anyhow::bail!("tmux has-session failed: {e}");
        }
        Err(_) => {
            anyhow::bail!("tmux has-session timed out after 10s");
        }
        _ => {}
    }

    // 4. Send the message (with timeout)
    let send = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new("tmux")
            .args(["send-keys", "-t", session_name, message, "Enter"])
            .status(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("tmux send-keys timed out after 10s"))??;

    if !send.success() {
        anyhow::bail!("tmux send-keys failed (exit {:?})", send.code());
    }

    // 5. Print result
    println!("[{slug}] redirected | session={session_name}");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
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
        async fn update_status(&self, _slug: &str, _status: Status) -> Result<()> {
            Ok(())
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
            _slug: &str,
            _new_session: &str,
            _new_log_file: &std::path::Path,
            _new_dispatched_at: chrono::DateTime<Utc>,
        ) -> Result<()> {
            Ok(())
        }
    }

    fn sample_record(slug: &str, status: Status) -> DispatchRecord {
        DispatchRecord {
            slug: slug.to_string(),
            branch: "test-branch".to_string(),
            worktree_path: PathBuf::from("/tmp/test"),
            session: "test-session".to_string(),
            log_file: PathBuf::from("/tmp/test.jsonl"),
            status,
            mode: Mode::Implement,
            retries: 0,
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
    async fn test_redirect_unknown_slug_errors() {
        let registry = MockRegistry::new(vec![]);
        let result = run_redirect(&registry, "tasks/nonexistent", "hello").await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no dispatch record found"));
    }

    // Note: testing tmux interaction requires a real tmux server,
    // so we test the non-running status warning path only.
    // The tmux send-keys would fail in CI, but the warning logic is testable.
    #[tokio::test]
    async fn test_redirect_non_running_proceeds_to_tmux_check() {
        let record = sample_record("tasks/test-1", Status::Failed);
        let registry = MockRegistry::new(vec![record]);

        // This will fail at the tmux has-session step (no real tmux),
        // but it should NOT fail at the status check (warn only).
        let result = run_redirect(&registry, "tasks/test-1", "hello").await;
        assert!(result.is_err());
        // Error should be about tmux, not about status
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("tmux") || err.contains("session"),
            "expected tmux error, got: {err}"
        );
    }
}
