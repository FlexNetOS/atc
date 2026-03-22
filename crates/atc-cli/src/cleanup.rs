use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::registry::{Registry, StatusFilter};
use atc_core::types::Status;
use atc_core::worktree::cleanup_worktree;
use tracing::warn;

use crate::kb::{kb_unassign_if_sole, kill_tmux_session};
use crate::resolve::resolve_record;

/// Execute the `atc cleanup` command.
pub async fn run_cleanup(
    config: &AtcConfig,
    registry: &dyn Registry,
    id: Option<&str>,
    done: bool,
) -> Result<()> {
    if let Some(arg) = id {
        let removed = cleanup_single(config, registry, arg).await?;
        println!("Cleaned {arg} (worktree removed: {removed})");
        Ok(())
    } else if done {
        cleanup_done(config, registry).await
    } else {
        anyhow::bail!("either <id> or --done is required")
    }
}

/// Clean up a single dispatch by ID or task slug.
/// Returns whether the worktree was removed.
async fn cleanup_single(config: &AtcConfig, registry: &dyn Registry, arg: &str) -> Result<bool> {
    let record = resolve_record(registry, arg).await?;
    let id = &record.id;

    // 1. Kill tmux session (best-effort)
    let session_killed = kill_tmux_session(&record.session).await;

    // 2. Check if other Running dispatches share this worktree
    let worktree_path = &record.worktree_path;
    let running = registry.find_running_on_worktree(worktree_path).await?;
    let shared = running.iter().any(|r| r.id != *id);

    let mut removed = false;

    if !session_killed {
        warn!(
            id,
            session = %record.session,
            "skipping worktree removal: tmux session kill was inconclusive"
        );
    } else if shared {
        warn!(
            id,
            worktree = %worktree_path.display(),
            "skipping worktree removal: another running dispatch shares this worktree"
        );
    } else {
        let worktree_base = config.dispatch.resolved_worktree_base();
        removed = cleanup_worktree(worktree_path, &worktree_base).await?;
    }

    // 3. Unassign task in git-kb (best-effort, only if no other live dispatch for same slug)
    if let Some(ref slug) = record.task_slug {
        kb_unassign_if_sole(registry, id, slug, config).await;
    }

    // 4. Update status to Stopped if not already terminal
    if !record.status.is_terminal() {
        registry.update_status(id, Status::Stopped).await?;
    }

    Ok(removed)
}

/// Batch-clean all Done dispatches.
async fn cleanup_done(config: &AtcConfig, registry: &dyn Registry) -> Result<()> {
    let records = registry.list(StatusFilter::by_status(Status::Done)).await?;

    if records.is_empty() {
        println!("No done dispatches to clean up");
        return Ok(());
    }

    let mut cleaned = 0u32;
    let mut failed = 0u32;
    for record in &records {
        match cleanup_single(config, registry, &record.id).await {
            Ok(_) => {
                cleaned += 1;
            }
            Err(e) => {
                failed += 1;
                warn!(
                    id = %record.id,
                    error = %e,
                    "failed to clean dispatch (skipping)"
                );
            }
        }
    }

    if failed > 0 {
        println!(
            "Cleaned {cleaned} dispatches ({failed} failed — run with RUST_LOG=warn for details)"
        );
    } else {
        println!("Cleaned {cleaned} dispatches");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
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
        async fn find_running_on_worktree(
            &self,
            worktree_path: &Path,
        ) -> Result<Vec<DispatchRecord>> {
            Ok(self
                .records
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.worktree_path == worktree_path && r.status == Status::Running)
                .cloned()
                .collect())
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
            checks: HealthChecks::default(),
            cost_usd: None,
            num_turns: None,
            duration_ms: None,
            dispatched_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_cleanup_unknown_id_errors() {
        let registry = MockRegistry::new(vec![]);
        let config = AtcConfig::default();

        let result = run_cleanup(&config, &registry, Some("nonexistent"), false).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no dispatch record found"));
    }

    #[tokio::test]
    async fn test_cleanup_missing_worktree_succeeds() {
        let record = sample_record("test-id-1", Status::Done);
        let registry = MockRegistry::new(vec![record]);
        let config = AtcConfig::default();

        // Worktree path doesn't exist — cleanup should succeed with removed=false
        let result = run_cleanup(&config, &registry, Some("test-id-1"), false).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_cleanup_skips_shared_worktree() {
        let mut record1 = sample_record("test-id-1", Status::Done);
        record1.worktree_path = PathBuf::from("/tmp/shared-worktree");

        let mut record2 = sample_record("test-id-2", Status::Running);
        record2.worktree_path = PathBuf::from("/tmp/shared-worktree");

        let registry = MockRegistry::new(vec![record1, record2]);
        let config = AtcConfig::default();

        // cleanup_single should not error even when another dispatch shares the worktree
        let result = run_cleanup(&config, &registry, Some("test-id-1"), false).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_cleanup_done_empty() {
        let registry = MockRegistry::new(vec![]);
        let config = AtcConfig::default();

        let result = run_cleanup(&config, &registry, None, true).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_cleanup_done_batch() {
        let r1 = sample_record("test-id-1", Status::Done);
        let mut r2 = sample_record("test-id-2", Status::Done);
        r2.task_slug = Some("tasks/test-2".to_string());
        let r3 = sample_record("test-id-3", Status::Running); // should not be cleaned
        let registry = MockRegistry::new(vec![r1, r2, r3]);
        let config = AtcConfig::default();

        let result = run_cleanup(&config, &registry, None, true).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_cleanup_requires_id_or_done() {
        let registry = MockRegistry::new(vec![]);
        let config = AtcConfig::default();

        let result = run_cleanup(&config, &registry, None, false).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("either <id> or --done is required"));
    }
}
