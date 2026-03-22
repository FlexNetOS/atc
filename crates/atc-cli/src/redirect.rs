use anyhow::Result;
use atc_core::registry::Registry;
use atc_core::types::Status;
use tracing::warn;

use crate::resolve::resolve_record;

/// Timeout for tmux subprocess calls.
const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Execute the `atc redirect` command.
pub async fn run_redirect(registry: &dyn Registry, arg: &str, message: &str) -> Result<()> {
    // 1. Get the record
    let record = resolve_record(registry, arg).await?;

    // 2. Warn if not Running (but don't hard-error)
    if record.status != Status::Running {
        warn!(
            id = %record.id,
            status = %record.status,
            "record status is not running; redirect may not reach the agent"
        );
        eprintln!(
            "warning: [{}] status is '{}', not 'running'",
            record.id, record.status
        );
    }

    let session_name = &record.session;

    // 3. Check if tmux session exists (with timeout, kill on drop)
    {
        let mut child = tokio::process::Command::new("tmux")
            .args(["has-session", "-t", session_name])
            .kill_on_drop(true)
            .spawn()?;
        match tokio::time::timeout(CMD_TIMEOUT, child.wait()).await {
            Ok(Ok(s)) if !s.success() => {
                anyhow::bail!("No active tmux session: {session_name}");
            }
            Ok(Err(e)) => {
                anyhow::bail!("tmux has-session failed: {e}");
            }
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                anyhow::bail!("tmux has-session timed out after 10s");
            }
            _ => {} // success
        }
    };

    // 4. Send the message (with timeout, kill on drop)
    let send = {
        let mut child = tokio::process::Command::new("tmux")
            .args(["send-keys", "-t", session_name, message, "Enter"])
            .kill_on_drop(true)
            .spawn()?;
        match tokio::time::timeout(CMD_TIMEOUT, child.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                anyhow::bail!("tmux send-keys timed out after 10s");
            }
        }
    };

    if !send.success() {
        anyhow::bail!("tmux send-keys failed (exit {:?})", send.code());
    }

    // 5. Print result
    println!("[{}] redirected | session={session_name}", record.id);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
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
        async fn update_status(&self, _id: &str, _status: Status) -> Result<()> {
            Ok(())
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
            _: &str,
            _: &str,
            _: &Path,
            _: chrono::DateTime<Utc>,
        ) -> Result<()> {
            Ok(())
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

    fn sample_record(id: &str, status: Status) -> DispatchRecord {
        DispatchRecord {
            id: id.to_string(),
            task_slug: Some("tasks/test-1".to_string()),
            branch: "test-branch".to_string(),
            worktree_path: PathBuf::from("/tmp/test"),
            session: "test-session".to_string(),
            log_file: PathBuf::from("/tmp/test.jsonl"),
            status,
            mode: Mode::Implement,
            retries: 0,
            resolver: "task".to_string(),
            pr_url: None,
            no_worktree: false,
            checks: HealthChecks::default(),
            cost_usd: None,
            num_turns: None,
            duration_ms: None,
            dispatched_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_redirect_unknown_id_errors() {
        let registry = MockRegistry::new(vec![]);
        let result = run_redirect(&registry, "nonexistent", "hello").await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no dispatch record found"));
    }

    #[tokio::test]
    async fn test_redirect_non_running_proceeds_to_tmux_check() {
        let record = sample_record("test-id-1", Status::Failed);
        let registry = MockRegistry::new(vec![record]);

        let result = run_redirect(&registry, "test-id-1", "hello").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("tmux") || err.contains("session"),
            "expected tmux error, got: {err}"
        );
    }
}
