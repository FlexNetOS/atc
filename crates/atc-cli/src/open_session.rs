use anyhow::{bail, Result};
use atc_core::registry::Registry;
use atc_core::terminal_text::{display_text, terminal_safe_json_pretty};
use atc_core::types::{
    atc_session_uri, parse_atc_session_uri, DispatchRecord, OpenSessionPreview, Status,
    TerminalLocator, TerminalStatus, TerminalStatusState,
};
use serde::Serialize;
use std::io::IsTerminal;

use crate::output_schema::SCHEMA_VERSION;

const ACTION_OPEN_SESSION: &str = "open-session";

#[derive(Debug, Clone, Serialize)]
pub struct OpenSessionOutputV1 {
    pub schema_version: u32,
    pub kind: &'static str,
    pub data: OpenSessionResult,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenSessionResult {
    pub target: String,
    pub dispatch_id: String,
    pub uri: String,
    pub task_slug: Option<String>,
    pub session: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_locator: Option<TerminalLocator>,
    pub terminal_status: TerminalStatus,
    pub open_shell: OpenSessionPreview,
}

pub async fn run_open_session(registry: &dyn Registry, target: &str, json: bool) -> Result<()> {
    let result = resolve_open_session(registry, target).await?;
    if json {
        let output = OpenSessionOutputV1 {
            schema_version: SCHEMA_VERSION,
            kind: ACTION_OPEN_SESSION,
            data: result,
        };
        println!("{}", terminal_safe_json_pretty(&output)?);
        return Ok(());
    }

    attach_result(&result).await?;
    println!("attached to {}", display_text(&result.session));
    Ok(())
}

pub async fn run_open_session_action(registry: &dyn Registry, target: &str) -> Result<String> {
    let result = resolve_open_session(registry, target).await?;
    attach_result(&result).await?;
    Ok(format!("attached to {}", display_text(&result.session)))
}

pub async fn resolve_open_session(
    registry: &dyn Registry,
    target: &str,
) -> Result<OpenSessionResult> {
    let record = resolve_record(registry, target).await?;
    open_session_result(target, record).await
}

pub fn effective_terminal_locator(record: &DispatchRecord) -> Option<TerminalLocator> {
    record.terminal_locator.clone().or_else(|| {
        let caps = record.agent_capabilities.unwrap_or_default();
        (caps.supports_tmux_attach && !record.session.trim().is_empty()).then(|| {
            TerminalLocator::inferred_tmux(
                record.session.clone(),
                Some(record.worktree_path.clone()),
                record.updated_at,
            )
        })
    })
}

pub async fn terminal_status_for_locator(locator: Option<&TerminalLocator>) -> TerminalStatus {
    let Some(locator) = locator else {
        return TerminalStatus::unavailable("no terminal locator");
    };

    match locator {
        TerminalLocator::Tmux(tmux) => crate::tmux::inspect_session(&tmux.session)
            .await
            .terminal_status(),
    }
}

pub fn open_shell_preview(
    locator: Option<&TerminalLocator>,
    status: &TerminalStatus,
) -> OpenSessionPreview {
    let Some(locator) = locator else {
        return OpenSessionPreview::disabled(ACTION_OPEN_SESSION, "no terminal locator");
    };

    match locator {
        TerminalLocator::Tmux(tmux) if tmux.session.trim().is_empty() => {
            OpenSessionPreview::disabled(ACTION_OPEN_SESSION, "tmux locator is missing session")
        }
        TerminalLocator::Tmux(tmux) if status.is_openable() => OpenSessionPreview::enabled(
            ACTION_OPEN_SESSION,
            "tmux",
            crate::tmux::attach_command_preview(&tmux.session),
        ),
        TerminalLocator::Tmux(_) => OpenSessionPreview {
            enabled: false,
            reason: status
                .reason
                .clone()
                .or_else(|| Some(format!("terminal is {}", status_state_label(status.state)))),
            action: ACTION_OPEN_SESSION.to_string(),
            backend: Some("tmux".to_string()),
            attach_command: None,
        },
    }
}

async fn open_session_result(target: &str, record: DispatchRecord) -> Result<OpenSessionResult> {
    let locator = effective_terminal_locator(&record);
    let terminal_status = terminal_status_for_locator(locator.as_ref()).await;
    let open_shell = open_shell_preview(locator.as_ref(), &terminal_status);
    let session = locator
        .as_ref()
        .and_then(TerminalLocator::tmux_session)
        .unwrap_or(&record.session)
        .to_string();
    Ok(OpenSessionResult {
        target: target.to_string(),
        uri: atc_session_uri(&record.id),
        task_slug: record.task_slug.clone(),
        session,
        dispatch_id: record.id,
        terminal_locator: locator,
        terminal_status,
        open_shell,
    })
}

async fn attach_result(result: &OpenSessionResult) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("refusing to attach from a non-interactive terminal; use --json for a preview");
    }

    if !result.open_shell.enabled {
        bail!(
            "{}",
            open_session_unavailable_message(result.open_shell.reason.as_deref())
        );
    }

    match result.terminal_locator.as_ref() {
        Some(TerminalLocator::Tmux(tmux)) => crate::tmux::attach(&tmux.session).await,
        None => bail!("no terminal locator"),
    }
}

async fn resolve_record(registry: &dyn Registry, target: &str) -> Result<DispatchRecord> {
    let target = target.trim();
    if target.is_empty() {
        bail!("open-session target is required");
    }

    if target.starts_with("atc://") {
        let dispatch_id = parse_atc_session_uri(target)?;
        return registry.get(&dispatch_id).await?.ok_or_else(|| {
            anyhow::anyhow!(
                "no dispatch record found for id: {}",
                display_text(&dispatch_id)
            )
        });
    }

    if let Some(record) = registry.get(target).await? {
        return Ok(record);
    }

    let candidates: Vec<DispatchRecord> = registry
        .find_by_task_slug(target)
        .await?
        .into_iter()
        .filter(|record| is_active_for_open_session(record.status))
        .collect();

    match candidates.as_slice() {
        [record] => Ok(record.clone()),
        [] => bail!(
            "no active dispatch found for task slug: {}",
            display_text(target)
        ),
        many => {
            let ids = many
                .iter()
                .map(|record| display_text(&record.id))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "task slug {} has multiple active dispatches: {ids}",
                display_text(target)
            );
        }
    }
}

fn is_active_for_open_session(status: Status) -> bool {
    !status.is_terminal()
}

fn status_state_label(state: TerminalStatusState) -> &'static str {
    match state {
        TerminalStatusState::Focusable => "focusable",
        TerminalStatusState::Attached => "attached",
        TerminalStatusState::Detached => "detached",
        TerminalStatusState::Running => "running",
        TerminalStatusState::Stale => "stale",
        TerminalStatusState::Unavailable => "unavailable",
        TerminalStatusState::Unknown => "unknown",
    }
}

fn open_session_unavailable_message(reason: Option<&str>) -> String {
    display_text(reason.unwrap_or("open-session is unavailable"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use atc_core::types::{AgentCapabilities, TerminalStatus, TerminalStatusState};
    use chrono::Utc;

    use crate::test_support::MockRegistry;

    fn assert_no_raw_terminal_controls(value: &str) {
        assert!(!value.contains('\x1b'), "raw escape leaked: {value:?}");
        assert!(
            !value.contains('\u{202e}'),
            "raw bidi control leaked: {value:?}"
        );
    }

    fn record_with_session(caps: AgentCapabilities) -> DispatchRecord {
        let mut record = crate::test_support::dispatch_record_fixture();
        record.id = "dispatch-1".to_string();
        record.session = "tmux-session".to_string();
        record.agent_capabilities = Some(caps);
        record
    }

    #[test]
    fn open_shell_preview_never_executes_stored_shell_text() {
        let locator = TerminalLocator::inferred_tmux(
            "session; touch /tmp/pwned",
            None,
            chrono::DateTime::parse_from_rfc3339("2026-06-05T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        let status = TerminalStatus::new(TerminalStatusState::Detached, Some("tmux"));
        let preview = open_shell_preview(Some(&locator), &status);
        assert!(preview.enabled);
        assert_eq!(
            preview.attach_command.unwrap(),
            vec![
                "tmux".to_string(),
                "attach".to_string(),
                "-t".to_string(),
                "session; touch /tmp/pwned".to_string()
            ]
        );
    }

    #[test]
    fn open_shell_preview_rejects_stale_tmux() {
        let locator = TerminalLocator::inferred_tmux("dead", None, Utc::now());
        let status =
            TerminalStatus::new(TerminalStatusState::Stale, Some("tmux")).with_reason("dead");
        let preview = open_shell_preview(Some(&locator), &status);
        assert!(!preview.enabled);
        assert_eq!(preview.reason.as_deref(), Some("dead"));
        assert!(preview.attach_command.is_none());
    }

    #[test]
    fn open_shell_preview_rejects_unknown_tmux_status() {
        let locator = TerminalLocator::inferred_tmux("not-yet-probed", None, Utc::now());
        let status = TerminalStatus::new(TerminalStatusState::Unknown, Some("tmux"));
        let preview = open_shell_preview(Some(&locator), &status);

        assert!(!preview.enabled);
        assert_eq!(preview.reason.as_deref(), Some("terminal is unknown"));
        assert!(preview.attach_command.is_none());
    }

    #[test]
    fn open_session_unavailable_message_escapes_terminal_controls() {
        let message = open_session_unavailable_message(Some("bad\x1b[2J\u{202e}reason"));

        assert_no_raw_terminal_controls(&message);
        assert!(message.contains("\\x1b"));
        assert!(message.contains("\\u{202e}"));
    }

    #[test]
    fn effective_terminal_locator_requires_tmux_attach_capability_for_legacy_session() {
        let unsupported = record_with_session(AgentCapabilities::default());
        assert!(effective_terminal_locator(&unsupported).is_none());

        let mut supported = record_with_session(AgentCapabilities {
            supports_tmux_attach: true,
            ..AgentCapabilities::default()
        });
        supported.updated_at = chrono::DateTime::parse_from_rfc3339("2026-06-05T01:02:03Z")
            .unwrap()
            .with_timezone(&Utc);
        let Some(TerminalLocator::Tmux(locator)) = effective_terminal_locator(&supported) else {
            panic!("legacy tmux-capable records should expose an inferred locator");
        };
        assert_eq!(locator.detected_at, supported.updated_at);
    }

    #[tokio::test]
    async fn resolve_open_session_errors_escape_hostile_target_text() {
        let registry = MockRegistry::new(Vec::new());

        let error = resolve_open_session(&registry, "tasks/bad\x1b[2J\u{202e}gpj")
            .await
            .unwrap_err()
            .to_string();

        assert_no_raw_terminal_controls(&error);
        assert!(error.contains("\\x1b"));
        assert!(error.contains("\\u{202e}"));
    }

    #[tokio::test]
    async fn resolve_open_session_ambiguous_task_errors_escape_candidate_ids() {
        let task_slug = "tasks/bad\x1b[2J\u{202e}gpj";
        let mut first = record_with_session(AgentCapabilities::default());
        first.id = "first\x1b[31m".to_string();
        first.task_slug = Some(task_slug.to_string());
        let mut second = record_with_session(AgentCapabilities::default());
        second.id = "second\u{202e}gpj".to_string();
        second.task_slug = Some(task_slug.to_string());
        let registry = MockRegistry::new(vec![first, second]);

        let error = resolve_open_session(&registry, task_slug)
            .await
            .unwrap_err()
            .to_string();

        assert_no_raw_terminal_controls(&error);
        assert!(error.contains("\\x1b"));
        assert!(error.contains("\\u{202e}"));
    }
}
