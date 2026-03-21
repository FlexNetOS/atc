use atc_core::config::AtcConfig;
use tracing::warn;

use crate::subprocess::run_cmd_with_timeout;

/// Timeout for non-fatal subprocess calls (tmux, git-kb).
pub(crate) const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
