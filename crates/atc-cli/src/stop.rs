use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::registry::Registry;
use atc_core::types::Status;
use tracing::warn;

use crate::kb::kill_tmux_session;
use crate::pipeline::resolver_by_name;
use crate::resolve::resolve_record;
use crate::terminal_text::display_text;

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
            display_text(&record.session)
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
    println!(
        "Stopped {} (session: {})",
        display_text(id),
        display_text(&record.session)
    );

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
