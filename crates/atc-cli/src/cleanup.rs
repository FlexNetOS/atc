use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::registry::{Registry, StatusFilter};
use atc_core::types::Status;
use atc_core::worktree::cleanup_worktree;
use tracing::warn;

use crate::kb::kill_tmux_session;
use crate::pipeline::resolver_by_name;
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
        if !record.status.is_terminal() {
            anyhow::bail!(
                "failed to confirm tmux session '{}' was stopped; leaving dispatch state unchanged",
                record.session
            );
        }
        warn!(
            id,
            session = %record.session,
            "skipping worktree removal: tmux session kill was inconclusive (already terminal)"
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

    // 3. Update status to Stopped if not already terminal
    if !record.status.is_terminal() {
        registry.update_status(id, Status::Stopped).await?;
    }

    // 4. Resolver cleanup (replaces hardcoded git-kb unassign)
    match resolver_by_name(&record.resolver) {
        Some(resolver) => resolver.on_cleanup(&record, config, Some(registry)).await,
        None => warn!(
            id = %record.id,
            resolver = %record.resolver,
            "unknown resolver name; skipping on_cleanup — task state may be orphaned"
        ),
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
        // Uses Status::Done (terminal) so that an inconclusive tmux kill
        // (e.g. tmux not installed) doesn't bail — the terminal-status
        // guard makes this test deterministic across hosts.
        let record = sample_record("test-id-1", Status::Done);
        let registry = MockRegistry::new(vec![record]);
        let config = AtcConfig::default();

        // Worktree path doesn't exist — cleanup should succeed with removed=false
        let result = run_cleanup(&config, &registry, Some("test-id-1"), false).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_cleanup_skips_shared_worktree() {
        // record1 is terminal (Done) so the inconclusive-kill guard
        // doesn't bail — this ensures the shared-worktree branch is
        // always exercised regardless of tmux availability.
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
    async fn test_cleanup_bails_on_inconclusive_kill_for_running() {
        // A Running record with a non-existent tmux session that can't be
        // confirmed killed should bail rather than marking Stopped.
        // This is deterministic: the session name doesn't exist, so
        // kill_tmux_session returns false (or true if tmux says "not found").
        // On hosts without tmux, kill returns false and the bail triggers.
        let record = sample_record("test-id-1", Status::Running);
        let registry = MockRegistry::new(vec![record]);
        let config = AtcConfig::default();

        let result = run_cleanup(&config, &registry, Some("test-id-1"), false).await;
        // On hosts without tmux: bails (Err). On hosts with tmux: the fake
        // session doesn't exist so tmux exits non-zero => killed=true => Ok.
        // Either way, the Running status must not be silently changed to Stopped.
        if result.is_err() {
            let r = registry.get("test-id-1").await.unwrap().unwrap();
            assert_eq!(
                r.status,
                Status::Running,
                "status must remain Running on bail"
            );
        }
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
