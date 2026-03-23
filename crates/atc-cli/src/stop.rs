use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::registry::Registry;
use atc_core::types::Status;
use tracing::warn;

use crate::kb::kill_tmux_session;
use crate::pipeline::resolver_by_name;
use crate::resolve::resolve_record;

/// Execute the `atc stop` command.
pub async fn run_stop(config: &AtcConfig, registry: &dyn Registry, arg: &str) -> Result<()> {
    // 1. Resolve record by ID or task slug
    let record = resolve_record(registry, arg).await?;
    let id = &record.id;

    // 2. Warn if already terminal, but proceed
    if record.status.is_terminal() {
        warn!(
            id,
            status = %record.status,
            "dispatch is already in terminal state"
        );
    }

    // 3. Kill tmux session (best-effort)
    let session_killed = kill_tmux_session(&record.session).await;
    if !session_killed && !record.status.is_terminal() {
        anyhow::bail!(
            "failed to confirm tmux session '{}' was stopped; leaving dispatch state unchanged",
            record.session
        );
    }

    // 4. Update status to Stopped (only if not already terminal)
    if !record.status.is_terminal() {
        registry.update_status(id, Status::Stopped).await?;
    }

    // 5. Resolver cleanup (replaces hardcoded git-kb unassign)
    match resolver_by_name(&record.resolver) {
        Some(resolver) => resolver.on_cleanup(&record, config, Some(registry)).await,
        None => warn!(
            id,
            resolver = %record.resolver,
            "unknown resolver name; skipping on_cleanup — task state may be orphaned"
        ),
    }

    // 6. Print result
    println!("Stopped {id} (session: {})", record.session);

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
        async fn list(&self, filter: StatusFilter) -> Result<Vec<DispatchRecord>> {
            let records = self.records.lock().unwrap();
            Ok(match filter {
                StatusFilter::All => records.clone(),
                StatusFilter::One(status) => records
                    .iter()
                    .filter(|r| r.status == status)
                    .cloned()
                    .collect(),
                StatusFilter::Any(ref statuses) => records
                    .iter()
                    .filter(|r| statuses.contains(&r.status))
                    .cloned()
                    .collect(),
            })
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
        async fn find_by_task_slug(&self, slug: &str) -> Result<Vec<DispatchRecord>> {
            Ok(self
                .records
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.task_slug.as_deref() == Some(slug))
                .cloned()
                .collect())
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
            worktree_path: PathBuf::from("/tmp/nonexistent-atc-test-worktree"),
            session: "test-session".to_string(),
            log_file: PathBuf::from("/tmp/test.jsonl"),
            status,
            mode: Mode::Implement,
            retries: 0,
            resolver: "task".to_string(),
            pr_url: None,
            no_worktree: false,
            original_input: None,
            checks: HealthChecks::default(),
            cost_usd: None,
            num_turns: None,
            duration_ms: None,
            dispatched_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_stop_sets_status_stopped() {
        let record = sample_record("test-id-1", Status::Running);
        let registry = MockRegistry::new(vec![record]);
        let config = AtcConfig::default();

        let result = run_stop(&config, &registry, "test-id-1").await;
        assert!(result.is_ok());

        let r = registry.get("test-id-1").await.unwrap().unwrap();
        assert_eq!(r.status, Status::Stopped);
    }

    #[tokio::test]
    async fn test_stop_unknown_id_errors() {
        let registry = MockRegistry::new(vec![]);
        let config = AtcConfig::default();

        let result = run_stop(&config, &registry, "nonexistent").await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no dispatch record found"));
    }

    #[tokio::test]
    async fn test_stop_terminal_state_warns_but_succeeds() {
        let record = sample_record("test-id-1", Status::Done);
        let registry = MockRegistry::new(vec![record]);
        let config = AtcConfig::default();

        // Should succeed (warns but does not error)
        let result = run_stop(&config, &registry, "test-id-1").await;
        assert!(result.is_ok());

        // Terminal status should be preserved, not overwritten to Stopped
        let r = registry.get("test-id-1").await.unwrap().unwrap();
        assert_eq!(r.status, Status::Done);
    }

    #[tokio::test]
    async fn test_stop_resolves_by_task_slug() {
        let record = sample_record("test-id-1", Status::Running);
        let registry = MockRegistry::new(vec![record]);
        let config = AtcConfig::default();

        let result = run_stop(&config, &registry, "tasks/test-1").await;
        assert!(result.is_ok());

        let r = registry.get("test-id-1").await.unwrap().unwrap();
        assert_eq!(r.status, Status::Stopped);
    }
}
