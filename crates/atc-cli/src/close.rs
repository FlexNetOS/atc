use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::registry::{Registry, StatusFilter};
use atc_core::types::Status;
use tracing::{info, warn};

/// Timeout for non-fatal subprocess calls (git-kb, git worktree).
const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Execute the `atc close` command.
pub async fn run_close(
    config: &AtcConfig,
    registry: &dyn Registry,
    slug: &str,
    pr_url: Option<&str>,
) -> Result<()> {
    // 1. Get the record
    let record = registry
        .get(slug)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no dispatch record found for slug: {slug}"))?;

    // 2. Idempotent: already Done
    if record.status == Status::Done {
        println!("[{slug}] already closed");
        return Ok(());
    }

    // 3. Set PR URL
    let effective_pr_url = if let Some(url) = pr_url {
        registry.set_pr_url(slug, url).await?;
        Some(url.to_string())
    } else {
        record.pr_url.clone()
    };

    // 4. Update status to Done
    registry.update_status(slug, Status::Done).await?;

    // 5. git-kb set status=completed (non-fatal)
    let kb_root = config
        .dispatch
        .resolved_meta_workspace_root(config.config_dir.as_deref())
        .ok();

    if let Some(ref kb_root) = kb_root {
        let status = run_cmd_with_timeout(
            tokio::process::Command::new("git-kb")
                .args(["set", slug, "status=completed"])
                .env("GITKB_ROOT", kb_root),
        )
        .await;

        match status {
            Ok(Some(s)) if !s.success() => {
                warn!(slug, exit_code = ?s.code(), "git-kb set status=completed failed (non-fatal)");
            }
            Ok(None) => {
                warn!(slug, "git-kb set status=completed timed out (non-fatal)");
            }
            Err(e) => {
                warn!(slug, error = %e, "git-kb set status=completed failed (non-fatal)");
            }
            _ => {
                info!(slug, "git-kb status set to completed");
            }
        }
    } else {
        warn!(
            slug,
            "could not resolve meta_workspace_root; skipping git-kb set"
        );
    }

    // 6. Remove worktree
    let worktree_path = &record.worktree_path;

    if worktree_path.exists() {
        // Check if another Running record shares the same worktree_path.
        // NOTE: This check is inherently racy (TOCTOU) — another dispatch could start
        // between the check and the removal. This is acceptable because the window is
        // very small and the consequence (removing a shared worktree) is recoverable
        // via re-dispatch. A lock-based approach would add complexity for minimal benefit.
        let all_records = registry.list(StatusFilter::all()).await?;
        let shared = all_records.iter().any(|r| {
            r.slug != slug && r.status == Status::Running && r.worktree_path == *worktree_path
        });

        if shared {
            warn!(
                slug,
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
                    )
                    .await;

                    match result {
                        Ok(Some(s)) if !s.success() => {
                            warn!(
                                slug,
                                exit_code = ?s.code(),
                                "git worktree remove failed (non-fatal)"
                            );
                        }
                        Ok(None) => {
                            warn!(slug, "git worktree remove timed out (non-fatal)");
                        }
                        Err(e) => {
                            warn!(slug, error = %e, "git worktree remove failed (non-fatal)");
                        }
                        _ => {
                            info!(slug, worktree = %worktree_path.display(), "worktree removed");

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
                    warn!(
                        slug,
                        "could not derive repo_root; skipping worktree removal"
                    );
                }
            }
        }
    } else {
        warn!(
            slug,
            worktree = %worktree_path.display(),
            "worktree path does not exist; skipping removal"
        );
    }

    // 7. Print result
    let pr_display = effective_pr_url.as_deref().unwrap_or("none");
    println!("[{slug}] closed | pr={pr_display}");

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

/// Derive the repo root path from config: meta_workspace_root + repo.
fn derive_repo_root(config: &AtcConfig) -> Option<std::path::PathBuf> {
    let meta_root = config
        .dispatch
        .resolved_meta_workspace_root(config.config_dir.as_deref())
        .ok()?;
    let repo = config.dispatch.resolved_repo().ok()?;
    Some(meta_root.join(repo))
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

        async fn set_pr_url(&self, slug: &str, url: &str) -> Result<()> {
            let mut records = self.records.lock().unwrap();
            for r in records.iter_mut() {
                if r.slug == slug {
                    r.pr_url = Some(url.to_string());
                    return Ok(());
                }
            }
            anyhow::bail!("no dispatch record found for slug: {slug}")
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

    fn sample_record(slug: &str, status: Status) -> DispatchRecord {
        DispatchRecord {
            slug: slug.to_string(),
            branch: "test-branch".to_string(),
            worktree_path: PathBuf::from("/tmp/nonexistent-atc-test-worktree"),
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
    async fn test_close_idempotent_on_done() {
        let record = sample_record("tasks/test-1", Status::Done);
        let registry = MockRegistry::new(vec![record]);
        let config = AtcConfig::default();

        // Should succeed without error
        let result = run_close(&config, &registry, "tasks/test-1", None).await;
        assert!(result.is_ok());

        // Status should still be Done
        let r = registry.get("tasks/test-1").await.unwrap().unwrap();
        assert_eq!(r.status, Status::Done);
    }

    #[tokio::test]
    async fn test_close_unknown_slug_errors() {
        let registry = MockRegistry::new(vec![]);
        let config = AtcConfig::default();

        let result = run_close(&config, &registry, "tasks/nonexistent", None).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no dispatch record found"));
    }

    #[tokio::test]
    async fn test_close_updates_status_to_done() {
        let record = sample_record("tasks/test-1", Status::Running);
        let registry = MockRegistry::new(vec![record]);
        let config = AtcConfig::default();

        let result = run_close(&config, &registry, "tasks/test-1", None).await;
        assert!(result.is_ok());

        let r = registry.get("tasks/test-1").await.unwrap().unwrap();
        assert_eq!(r.status, Status::Done);
    }

    #[tokio::test]
    async fn test_close_with_pr_url_sets_it() {
        let record = sample_record("tasks/test-1", Status::Running);
        let registry = MockRegistry::new(vec![record]);
        let config = AtcConfig::default();

        let result = run_close(
            &config,
            &registry,
            "tasks/test-1",
            Some("https://github.com/org/repo/pull/1"),
        )
        .await;
        assert!(result.is_ok());

        let r = registry.get("tasks/test-1").await.unwrap().unwrap();
        assert_eq!(
            r.pr_url.as_deref(),
            Some("https://github.com/org/repo/pull/1")
        );
    }

    #[tokio::test]
    async fn test_close_missing_worktree_does_not_error() {
        let mut record = sample_record("tasks/test-1", Status::Running);
        record.worktree_path = PathBuf::from("/tmp/this-path-definitely-does-not-exist-atc-test");
        let registry = MockRegistry::new(vec![record]);
        let config = AtcConfig::default();

        // Should succeed even though worktree path doesn't exist
        let result = run_close(&config, &registry, "tasks/test-1", None).await;
        assert!(result.is_ok());
    }
}
