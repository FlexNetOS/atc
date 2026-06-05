use anyhow::{Context, Result};
use atc_core::types::{TerminalStatus, TerminalStatusState};
use std::io;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

const TMUX_BIN: &str = "tmux";
const TMUX_INSPECT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxInspect {
    Attached,
    Detached,
    Stale,
    Unavailable(String),
}

impl TmuxInspect {
    pub fn terminal_status(&self) -> TerminalStatus {
        match self {
            Self::Attached => TerminalStatus::new(TerminalStatusState::Attached, Some("tmux")),
            Self::Detached => TerminalStatus::new(TerminalStatusState::Detached, Some("tmux")),
            Self::Stale => TerminalStatus::new(TerminalStatusState::Stale, Some("tmux"))
                .with_reason("tmux session is not running"),
            Self::Unavailable(reason) => TerminalStatus {
                state: TerminalStatusState::Unavailable,
                backend: Some("tmux".to_string()),
                reason: Some(reason.clone()),
            },
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(self, Self::Attached | Self::Detached)
    }
}

pub fn attach_command_preview(session: &str) -> Vec<String> {
    vec![
        "tmux".to_string(),
        "attach".to_string(),
        "-t".to_string(),
        session.to_string(),
    ]
}

pub async fn session_alive(session: &str) -> bool {
    inspect_session(session).await.is_live()
}

pub async fn inspect_session(session: &str) -> TmuxInspect {
    inspect_session_with_binary(TMUX_BIN, session, TMUX_INSPECT_TIMEOUT).await
}

async fn inspect_session_with_binary(
    tmux_bin: &str,
    session: &str,
    timeout_duration: Duration,
) -> TmuxInspect {
    if session.trim().is_empty() {
        return TmuxInspect::Stale;
    }

    let status = run_tmux_status(
        tmux_bin,
        ["has-session", "-t", session],
        "has-session",
        timeout_duration,
    )
    .await;

    match status {
        Ok(status) if status.success() => {}
        Ok(_) => return TmuxInspect::Stale,
        Err(inspect) => return inspect,
    }

    let clients = run_tmux_output(
        tmux_bin,
        ["list-clients", "-t", session, "-F", "#{client_tty}"],
        "list-clients",
        timeout_duration,
    )
    .await;

    match clients {
        Ok(output) if output.status.success() && !output.stdout.is_empty() => TmuxInspect::Attached,
        Ok(_) => TmuxInspect::Detached,
        Err(inspect) => inspect,
    }
}

async fn run_tmux_status<const N: usize>(
    tmux_bin: &str,
    args: [&str; N],
    action: &str,
    timeout_duration: Duration,
) -> Result<std::process::ExitStatus, TmuxInspect> {
    let mut command = Command::new(tmux_bin);
    command
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    match timeout(timeout_duration, command.status()).await {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(e)) => Err(tmux_io_error(action, e)),
        Err(_) => Err(tmux_timeout(action, timeout_duration)),
    }
}

async fn run_tmux_output<const N: usize>(
    tmux_bin: &str,
    args: [&str; N],
    action: &str,
    timeout_duration: Duration,
) -> Result<std::process::Output, TmuxInspect> {
    let mut command = Command::new(tmux_bin);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    match timeout(timeout_duration, command.output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(tmux_io_error(action, e)),
        Err(_) => Err(tmux_timeout(action, timeout_duration)),
    }
}

fn tmux_io_error(action: &str, error: io::Error) -> TmuxInspect {
    if error.kind() == io::ErrorKind::NotFound {
        TmuxInspect::Unavailable("tmux executable not found".to_string())
    } else {
        TmuxInspect::Unavailable(format!("tmux {action} failed: {error}"))
    }
}

fn tmux_timeout(action: &str, timeout_duration: Duration) -> TmuxInspect {
    TmuxInspect::Unavailable(format!(
        "tmux {action} timed out after {}ms",
        timeout_duration.as_millis()
    ))
}

pub async fn attach(session: &str) -> Result<()> {
    let status = Command::new(TMUX_BIN)
        .args(["attach", "-t", session])
        .status()
        .await
        .context("failed to execute tmux attach")?;
    if !status.success() {
        anyhow::bail!("tmux attach exited with status {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::fs;

    #[test]
    fn attach_command_preview_preserves_session_as_argv_data() {
        assert_eq!(
            attach_command_preview("session; touch /tmp/pwned"),
            vec![
                "tmux".to_string(),
                "attach".to_string(),
                "-t".to_string(),
                "session; touch /tmp/pwned".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn inspect_session_reports_missing_tmux_as_unavailable() {
        let tempdir = tempfile::tempdir().unwrap();
        let missing_tmux = tempdir.path().join("missing-tmux");
        let inspect = inspect_session_with_binary(
            missing_tmux.to_str().unwrap(),
            "session",
            Duration::from_millis(50),
        )
        .await;

        assert!(matches!(
            inspect,
            TmuxInspect::Unavailable(reason) if reason == "tmux executable not found"
        ));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn inspect_session_times_out_when_tmux_probe_hangs() {
        use std::os::unix::fs::PermissionsExt;

        let tempdir = tempfile::tempdir().unwrap();
        let tmux = tempdir.path().join("tmux");
        fs::write(&tmux, "#!/bin/sh\nexec sleep 5\n").unwrap();
        fs::set_permissions(&tmux, fs::Permissions::from_mode(0o755)).unwrap();

        let inspect = inspect_session_with_binary(
            tmux.to_str().unwrap(),
            "session",
            Duration::from_millis(250),
        )
        .await;

        assert!(matches!(
            inspect,
            TmuxInspect::Unavailable(reason) if reason.contains("has-session timed out")
        ));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn inspect_session_reports_list_clients_timeout_as_unavailable() {
        use std::os::unix::fs::PermissionsExt;

        let tempdir = tempfile::tempdir().unwrap();
        let tmux = tempdir.path().join("tmux");
        fs::write(
            &tmux,
            "#!/bin/sh\nif [ \"$1\" = \"has-session\" ]; then exit 0; fi\nexec sleep 5\n",
        )
        .unwrap();
        fs::set_permissions(&tmux, fs::Permissions::from_mode(0o755)).unwrap();

        let inspect = inspect_session_with_binary(
            tmux.to_str().unwrap(),
            "session",
            Duration::from_millis(250),
        )
        .await;

        assert!(matches!(
            inspect,
            TmuxInspect::Unavailable(reason) if reason.contains("list-clients timed out")
        ));
    }
}
