use atc_core::config::AtcConfig;
use atc_core::registry::Registry;
use tracing::warn;

use crate::subprocess::run_cmd_with_timeout;

/// Timeout for non-fatal subprocess calls (tmux, git-kb).
pub(crate) const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Best-effort tmux session kill.
///
/// Returns `true` if the session was confirmed killed or was already absent,
/// `false` if the kill outcome is inconclusive (timeout, exec failure).
pub(crate) async fn kill_tmux_session(session: &str) -> bool {
    match run_cmd_with_timeout(
        tokio::process::Command::new("tmux")
            .args(["kill-session", "-t", session])
            .stderr(std::process::Stdio::null()),
        CMD_TIMEOUT,
    )
    .await
    {
        Ok(Some(s)) if !s.success() => {
            // Non-zero exit typically means the session doesn't exist (already gone)
            tracing::debug!(
                session,
                "tmux kill-session exited non-zero (may already be gone)"
            );
            true
        }
        Ok(Some(_)) => true, // success
        Ok(None) => {
            tracing::debug!(session, "tmux kill-session timed out");
            false
        }
        Err(e) => {
            tracing::debug!(session, error = %e, "tmux kill-session failed");
            false
        }
    }
}

/// Unassign a task in git-kb if no other live (non-terminal) dispatch exists for the same slug.
pub(crate) async fn kb_unassign_if_sole(
    registry: &dyn Registry,
    id: &str,
    slug: &str,
    config: &AtcConfig,
) {
    let has_other_live = match registry.find_by_task_slug(slug).await {
        Ok(records) => records
            .into_iter()
            .any(|r| r.id != id && !r.status.is_terminal()),
        Err(_) => return, // can't determine — skip unassign to be safe
    };
    if !has_other_live {
        kb_unassign(slug, config).await;
    }
}

/// Non-fatal: unassign task in git-kb.
pub(crate) async fn kb_unassign(slug: &str, config: &AtcConfig) {
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
        CMD_TIMEOUT,
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
