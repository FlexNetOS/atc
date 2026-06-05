use anyhow::{Context, Result};
use atc_core::types::{TerminalStatus, TerminalStatusState};
use std::io;
use std::process::Stdio;

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
    matches!(
        inspect_session(session).await,
        TmuxInspect::Attached | TmuxInspect::Detached
    )
}

pub async fn inspect_session(session: &str) -> TmuxInspect {
    if session.trim().is_empty() {
        return TmuxInspect::Stale;
    }

    let status = tokio::process::Command::new("tmux")
        .args(["has-session", "-t", session])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    match status {
        Ok(status) if status.success() => {}
        Ok(_) => return TmuxInspect::Stale,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return TmuxInspect::Unavailable("tmux executable not found".to_string());
        }
        Err(e) => return TmuxInspect::Unavailable(format!("tmux has-session failed: {e}")),
    }

    let clients = tokio::process::Command::new("tmux")
        .args(["list-clients", "-t", session, "-F", "#{client_tty}"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await;

    match clients {
        Ok(output) if output.status.success() && !output.stdout.is_empty() => TmuxInspect::Attached,
        Ok(_) => TmuxInspect::Detached,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            TmuxInspect::Unavailable("tmux executable not found".to_string())
        }
        Err(_) => TmuxInspect::Detached,
    }
}

pub async fn attach(session: &str) -> Result<()> {
    let status = tokio::process::Command::new("tmux")
        .args(["attach", "-t", session])
        .status()
        .await
        .context("failed to execute tmux attach")?;
    if !status.success() {
        anyhow::bail!("tmux attach exited with status {status}");
    }
    Ok(())
}
