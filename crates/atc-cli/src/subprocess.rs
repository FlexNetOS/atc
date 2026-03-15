//! Shared subprocess utilities for running commands with timeouts.

/// Default timeout for non-fatal subprocess calls.
pub const DEFAULT_CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Run a command with a timeout, killing the child on timeout.
/// Returns `Ok(Some(status))` on normal exit, `Ok(None)` on timeout, `Err` on spawn failure.
pub async fn run_cmd_with_timeout(
    cmd: &mut tokio::process::Command,
    timeout: std::time::Duration,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    let mut child = cmd.kill_on_drop(true).spawn()?;
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => status.map(Some),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Ok(None)
        }
    }
}
