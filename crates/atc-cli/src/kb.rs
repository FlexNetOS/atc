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
