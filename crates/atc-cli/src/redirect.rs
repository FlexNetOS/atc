use anyhow::Result;
use atc_core::registry::Registry;
use atc_core::types::Status;
use tracing::warn;

use crate::resolve::resolve_record;

/// Timeout for tmux subprocess calls.
const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Execute the `atc redirect` command.
pub async fn run_redirect(registry: &dyn Registry, arg: &str, message: &str) -> Result<()> {
    // 1. Get the record
    let record = resolve_record(registry, arg).await?;

    // 2. Warn if not Running (but don't hard-error)
    if record.status != Status::Running {
        warn!(
            id = %record.id,
            status = %record.status,
            "record status is not running; redirect may not reach the agent"
        );
        eprintln!(
            "warning: [{}] status is '{}', not 'running'",
            record.id, record.status
        );
    }

    let session_name = &record.session;

    // 3. Check if tmux session exists (with timeout, kill on drop)
    {
        let mut child = tokio::process::Command::new("tmux")
            .args(["has-session", "-t", session_name])
            .kill_on_drop(true)
            .spawn()?;
        match tokio::time::timeout(CMD_TIMEOUT, child.wait()).await {
            Ok(Ok(s)) if !s.success() => {
                anyhow::bail!("No active tmux session: {session_name}");
            }
            Ok(Err(e)) => {
                anyhow::bail!("tmux has-session failed: {e}");
            }
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                anyhow::bail!("tmux has-session timed out after 10s");
            }
            _ => {} // success
        }
    };

    // 4. Send the message (with timeout, kill on drop)
    let send = {
        let mut child = tokio::process::Command::new("tmux")
            .args(["send-keys", "-t", session_name, message, "Enter"])
            .kill_on_drop(true)
            .spawn()?;
        match tokio::time::timeout(CMD_TIMEOUT, child.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                anyhow::bail!("tmux send-keys timed out after 10s");
            }
        }
    };

    if !send.success() {
        anyhow::bail!("tmux send-keys failed (exit {:?})", send.code());
    }

    // 5. Print result
    println!("[{}] redirected | session={session_name}", record.id);

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
            worktree_path: PathBuf::from("/tmp/test"),
            session: "test-session".to_string(),
            log_file: PathBuf::from("/tmp/test.jsonl"),
            status,
            directive: Directive::Implement,
            resolver: "task".to_string(),
            ..crate::test_support::dispatch_record_fixture()
        }
    }

    #[tokio::test]
    async fn test_redirect_unknown_id_errors() {
        let registry = MockRegistry::new(vec![]);
        let result = run_redirect(&registry, "nonexistent", "hello").await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no dispatch record found"));
    }

    #[tokio::test]
    async fn test_redirect_non_running_proceeds_to_tmux_check() {
        let record = sample_record("test-id-1", Status::Failed);
        let registry = MockRegistry::new(vec![record]);

        let result = run_redirect(&registry, "test-id-1", "hello").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("tmux") || err.contains("session"),
            "expected tmux error, got: {err}"
        );
    }
}
