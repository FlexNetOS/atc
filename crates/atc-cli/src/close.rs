use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::registry::Registry;
use atc_core::terminal_text::display_text;
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
        println!("[{}] already closed", display_text(id));
        return Ok(());
    }

    // 3. Set PR URL
    let effective_pr_url = if let Some(url) = pr_url {
        registry.add_pr_url(id, url).await?;
        Some(url.to_string())
    } else {
        record.pr_urls.first().cloned()
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
            display_text(&record.session)
        );
    }

    // 5. Update status to Done
    registry.update_status(id, Status::Done).await?;

    // 6. Resolver cleanup (replaces hardcoded git-kb unassign) + close-specific git-kb set
    match resolver_by_name(&record.resolver) {
        Some(resolver) => resolver.on_cleanup(&record, config, Some(registry)).await,
        None => warn!(
            id = %display_text(id),
            resolver = %display_text(&record.resolver),
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
                            info!(
                                id = %display_text(id),
                                slug = %display_text(slug),
                                "skipping status=completed: another live dispatch exists for this slug"
                            );
                        }
                        !has_other_live
                    }
                    Err(e) => {
                        warn!(
                            id = %display_text(id),
                            error = %display_text(&e.to_string()),
                            "failed to check sibling dispatches; skipping status=completed for safety"
                        );
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
                            warn!(
                                id = %display_text(id),
                                exit_code = ?s.code(),
                                "git-kb set status=completed failed (non-fatal)"
                            );
                        }
                        Ok(None) => {
                            warn!(
                                id = %display_text(id),
                                "git-kb set status=completed timed out (non-fatal)"
                            );
                        }
                        Err(e) => {
                            warn!(
                                id = %display_text(id),
                                error = %display_text(&e.to_string()),
                                "git-kb set status=completed failed (non-fatal)"
                            );
                        }
                        _ => {
                            info!(id = %display_text(id), "git-kb status set to completed");
                        }
                    }
                }
            }
        } else {
            warn!(
                id = %display_text(id),
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
                id = %display_text(id),
                worktree = %display_text(&worktree_path.display().to_string()),
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
                                id = %display_text(id),
                                exit_code = ?s.code(),
                                "git worktree remove failed (non-fatal)"
                            );
                        }
                        Ok(None) => {
                            warn!(
                                id = %display_text(id),
                                "git worktree remove timed out (non-fatal)"
                            );
                        }
                        Err(e) => {
                            warn!(
                                id = %display_text(id),
                                error = %display_text(&e.to_string()),
                                "git worktree remove failed (non-fatal)"
                            );
                        }
                        _ => {
                            info!(
                                id = %display_text(id),
                                worktree = %display_text(&worktree_path.display().to_string()),
                                "worktree removed"
                            );

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
                        id = %display_text(id),
                        "could not derive repo_root; skipping worktree removal"
                    );
                }
            }
        }
    } else {
        warn!(
            id = %display_text(id),
            worktree = %display_text(&worktree_path.display().to_string()),
            "worktree path does not exist; skipping removal"
        );
    }

    // 8. Print result
    let pr_display = effective_pr_url.as_deref().unwrap_or("none");
    let slug_display = record.task_slug.as_deref().unwrap_or(id);
    println!(
        "[{}] closed | pr={}",
        display_text(slug_display),
        display_text(pr_display)
    );

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
    use atc_core::types::{Directive, DispatchRecord};
    use std::path::PathBuf;

    use crate::test_support::MockRegistry;

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
            resolver: "task".to_string(),
            ..crate::test_support::dispatch_record_fixture()
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
            r.pr_urls.first().map(|s| s.as_str()),
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
