use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::registry::Registry;
use atc_core::types::Status;
use tracing::{info, warn};

use crate::kb::kill_tmux_session;
use crate::pipeline::resolver_by_name;
use crate::resolve::resolve_record;
use crate::subprocess::run_cmd_with_timeout;

/// Timeout for non-fatal subprocess calls (git-kb, git worktree).
const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Execute the `atc close` command.
pub async fn run_close(
    config: &AtcConfig,
    registry: &dyn Registry,
    arg: &str,
    pr_url: Option<&str>,
) -> Result<()> {
    // 1. Get the record
    let record = resolve_record(registry, arg).await?;
    let id = &record.id;

    // 2. Idempotent: already Done
    if record.status == Status::Done {
        println!("[{id}] already closed");
        return Ok(());
    }

    // 3. Set PR URL
    let effective_pr_url = if let Some(url) = pr_url {
        registry.set_pr_url(id, url).await?;
        Some(url.to_string())
    } else {
        record.pr_url.clone()
    };

    // 4. Kill any lingering tmux session (non-fatal)
    // Sessions can survive beyond Running/Stopped — e.g. a Failed record may
    // still have a live tmux session if the agent process crashed but tmux
    // survived, or a NeedsHuman record had its status set externally.
    // Note: Status::Done is already handled above (early return), so this always runs.
    let session_killed = kill_tmux_session(&record.session).await;

    // For non-terminal records, bail if the kill was inconclusive to avoid
    // releasing task state while the agent may still be alive.
    if !session_killed && !record.status.is_terminal() {
        anyhow::bail!(
            "failed to confirm tmux session '{}' was stopped; leaving dispatch state unchanged",
            record.session
        );
    }

    // 5. Update status to Done
    registry.update_status(id, Status::Done).await?;

    // 6. Resolver cleanup (replaces hardcoded git-kb unassign) + close-specific git-kb set
    match resolver_by_name(&record.resolver) {
        Some(resolver) => resolver.on_cleanup(&record, config, Some(registry)).await,
        None => warn!(
            id,
            resolver = %record.resolver,
            "unknown resolver name; skipping on_cleanup — task state may be orphaned"
        ),
    }

    // Close-specific: set task status to completed in git-kb (non-fatal)
    // Guard: only mark completed if no other non-terminal dispatch exists for this slug
    if record.resolver == "task" {
        let kb_root = config
            .dispatch
            .resolved_meta_workspace_root(config.config_dir.as_deref())
            .ok();

        if let Some(ref kb_root) = kb_root {
            if let Some(ref slug) = record.task_slug {
                // Check for sibling dispatches before marking completed
                let should_complete = match registry.find_by_task_slug(slug).await {
                    Ok(records) => {
                        let has_other_live = records
                            .iter()
                            .any(|r| r.id != *id && !r.status.is_terminal());
                        if has_other_live {
                            info!(id, slug, "skipping status=completed: another live dispatch exists for this slug");
                        }
                        !has_other_live
                    }
                    Err(e) => {
                        warn!(id, error = %e, "failed to check sibling dispatches; skipping status=completed for safety");
                        false
                    }
                };

                if should_complete {
                    let status = run_cmd_with_timeout(
                        tokio::process::Command::new("git-kb")
                            .args(["set", slug, "status=completed"])
                            .env("GITKB_ROOT", kb_root),
                        CMD_TIMEOUT,
                    )
                    .await;

                    match status {
                        Ok(Some(s)) if !s.success() => {
                            warn!(id, exit_code = ?s.code(), "git-kb set status=completed failed (non-fatal)");
                        }
                        Ok(None) => {
                            warn!(id, "git-kb set status=completed timed out (non-fatal)");
                        }
                        Err(e) => {
                            warn!(id, error = %e, "git-kb set status=completed failed (non-fatal)");
                        }
                        _ => {
                            info!(id, "git-kb status set to completed");
                        }
                    }
                }
            }
        } else {
            warn!(
                id,
                "could not resolve meta_workspace_root; skipping git-kb set"
            );
        }
    }

    // 7. Remove worktree
    let worktree_path = &record.worktree_path;

    if worktree_path.exists() {
        // Check if another Running record shares the same worktree_path.
        let running = registry.find_running_on_worktree(worktree_path).await?;
        let shared = running.iter().any(|r| r.id != *id);

        if shared {
            warn!(
                id,
                worktree = %worktree_path.display(),
                "skipping worktree removal: another running record shares this worktree"
            );
        } else {
            // Derive repo_root from config
            let repo_root = derive_repo_root(config);

            match repo_root {
                Some(root) => {
                    let result = run_cmd_with_timeout(
                        tokio::process::Command::new("git")
                            .arg("-C")
                            .arg(&root)
                            .arg("worktree")
                            .arg("remove")
                            .arg("--force")
                            .arg(worktree_path),
                        CMD_TIMEOUT,
                    )
                    .await;

                    match result {
                        Ok(Some(s)) if !s.success() => {
                            warn!(
                                id,
                                exit_code = ?s.code(),
                                "git worktree remove failed (non-fatal)"
                            );
                        }
                        Ok(None) => {
                            warn!(id, "git worktree remove timed out (non-fatal)");
                        }
                        Err(e) => {
                            warn!(id, error = %e, "git worktree remove failed (non-fatal)");
                        }
                        _ => {
                            info!(id, worktree = %worktree_path.display(), "worktree removed");

                            // Attempt to rmdir parent if empty and inside worktree_base
                            if let Some(parent) = worktree_path.parent() {
                                let worktree_base = config.dispatch.resolved_worktree_base();
                                if parent.starts_with(&worktree_base) && parent != worktree_base {
                                    let _ = std::fs::remove_dir(parent); // ignore error (not empty)
                                }
                            }
                        }
                    }
                }
                None => {
                    warn!(id, "could not derive repo_root; skipping worktree removal");
                }
            }
        }
    } else {
        warn!(
            id,
            worktree = %worktree_path.display(),
            "worktree path does not exist; skipping removal"
        );
    }

    // 8. Print result
    let pr_display = effective_pr_url.as_deref().unwrap_or("none");
    let slug_display = record.task_slug.as_deref().unwrap_or(id);
    println!("[{slug_display}] closed | pr={pr_display}");

    Ok(())
}

/// Derive the repo root path from config: meta_workspace_root + repo.
fn derive_repo_root(config: &AtcConfig) -> Option<std::path::PathBuf> {
    let meta_root = config
        .dispatch
        .resolved_meta_workspace_root(config.config_dir.as_deref())
        .ok()?;
    let repo = config.dispatch.resolved_repo()?;
    Some(meta_root.join(repo))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use atc_core::registry::StatusFilter;
    use atc_core::types::{Directive, DispatchRecord, HealthChecks};
    use chrono::Utc;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// A simple in-memory registry for testing close logic without SQLite.
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

        async fn update_cost(
            &self,
            _id: &str,
            _cost: f64,
            _turns: u32,
            _duration_ms: u64,
        ) -> Result<()> {
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
            _id: &str,
            _checks: &HealthChecks,
            _status: Status,
            _updated_at: chrono::DateTime<Utc>,
        ) -> Result<()> {
            Ok(())
        }

        async fn set_pr_url(&self, id: &str, url: &str) -> Result<()> {
            let mut records = self.records.lock().unwrap();
            for r in records.iter_mut() {
                if r.id == id {
                    r.pr_url = Some(url.to_string());
                    return Ok(());
                }
            }
            anyhow::bail!("no dispatch record found for id: {id}")
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

        async fn find_by_branch(&self, _branch: &str) -> Result<Vec<DispatchRecord>> {
            Ok(vec![])
        }
        async fn find_by_task_slug(&self, _task_slug: &str) -> Result<Vec<DispatchRecord>> {
            Ok(vec![])
        }
        async fn find_by_pr_url(&self, _pr_url: &str) -> Result<Vec<DispatchRecord>> {
            Ok(vec![])
        }
        async fn find_by_worktree(&self, _worktree_path: &Path) -> Result<Vec<DispatchRecord>> {
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
            directive: Directive::Implement,
            retries: 0,
            resolver: "task".to_string(),
            pr_url: None,
            no_worktree: false,
            original_input: None,
            checks: HealthChecks::default(),
            kb_root: None,
            cost_usd: None,
            num_turns: None,
            duration_ms: None,
            artifacts: None,
            dispatched_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_close_idempotent_on_done() {
        let record = sample_record("test-id-1", Status::Done);
        let registry = MockRegistry::new(vec![record]);
        let config = AtcConfig::default();

        let result = run_close(&config, &registry, "test-id-1", None).await;
        assert!(result.is_ok());

        let r = registry.get("test-id-1").await.unwrap().unwrap();
        assert_eq!(r.status, Status::Done);
    }

    #[tokio::test]
    async fn test_close_unknown_id_errors() {
        let registry = MockRegistry::new(vec![]);
        let config = AtcConfig::default();

        let result = run_close(&config, &registry, "nonexistent", None).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no dispatch record found"));
    }

    #[tokio::test]
    async fn test_close_updates_status_to_done() {
        let record = sample_record("test-id-1", Status::Running);
        let registry = MockRegistry::new(vec![record]);
        let config = AtcConfig::default();

        let result = run_close(&config, &registry, "test-id-1", None).await;
        assert!(result.is_ok());

        let r = registry.get("test-id-1").await.unwrap().unwrap();
        assert_eq!(r.status, Status::Done);
    }

    #[tokio::test]
    async fn test_close_with_pr_url_sets_it() {
        let record = sample_record("test-id-1", Status::Running);
        let registry = MockRegistry::new(vec![record]);
        let config = AtcConfig::default();

        let result = run_close(
            &config,
            &registry,
            "test-id-1",
            Some("https://github.com/org/repo/pull/1"),
        )
        .await;
        assert!(result.is_ok());

        let r = registry.get("test-id-1").await.unwrap().unwrap();
        assert_eq!(
            r.pr_url.as_deref(),
            Some("https://github.com/org/repo/pull/1")
        );
    }

    #[tokio::test]
    async fn test_close_missing_worktree_does_not_error() {
        let mut record = sample_record("test-id-1", Status::Running);
        record.worktree_path = PathBuf::from("/tmp/this-path-definitely-does-not-exist-atc-test");
        let registry = MockRegistry::new(vec![record]);
        let config = AtcConfig::default();

        let result = run_close(&config, &registry, "test-id-1", None).await;
        assert!(result.is_ok());
    }
}
