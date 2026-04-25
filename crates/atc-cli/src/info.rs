//! `atc info <id>` — detailed view of a single dispatch record.

use anyhow::Result;
use atc_core::registry::Registry;
use atc_core::types::DispatchRecord;
use serde::Serialize;
use std::sync::Arc;

use crate::resolve::resolve_record;
use crate::status::{format_duration, STATUS_SCHEMA_VERSION};

/// JSON envelope for `atc info --json`. Mirrors the v1 schema versioning
/// shared by the rest of the human-facing commands.
#[derive(Debug, Serialize)]
pub struct InfoOutputV1<'a> {
    pub schema_version: u32,
    pub record: &'a DispatchRecord,
}

/// Format a dispatch record for display.
pub fn format_info(record: &DispatchRecord) -> String {
    let mut lines: Vec<String> = Vec::new();
    let label_width = 16; // width for colon-aligned labels

    let add_line = |lines: &mut Vec<String>, label: &str, value: &str| {
        lines.push(format!("  {:<label_width$}{}", format!("{label}:"), value));
    };

    add_line(&mut lines, "id", &record.id);
    if let Some(ref slug) = record.task_slug {
        add_line(&mut lines, "task_slug", slug);
    }
    add_line(&mut lines, "status", record.status.as_str());
    add_line(&mut lines, "directive", record.directive.as_str());
    add_line(&mut lines, "branch", &record.branch);
    add_line(&mut lines, "resolver", &record.resolver);

    let worktree_str = record.worktree_path.to_string_lossy();
    if !worktree_str.is_empty() {
        add_line(&mut lines, "worktree_path", &worktree_str);
    }

    add_line(&mut lines, "session", &record.session);

    if !record.pr_urls.is_empty() {
        add_line(&mut lines, "pr_urls", &record.pr_urls.join(", "));
    }

    if let Some(cost) = record.cost_usd {
        add_line(&mut lines, "cost_usd", &format!("${:.2}", cost));
    }

    if let Some(turns) = record.num_turns {
        add_line(&mut lines, "num_turns", &turns.to_string());
    }

    if let Some(ms) = record.duration_ms {
        add_line(&mut lines, "duration", &format_duration(ms));
    }

    if record.retries > 0 {
        add_line(&mut lines, "retries", &record.retries.to_string());
    }

    add_line(
        &mut lines,
        "dispatched_at",
        &record.dispatched_at.to_rfc3339(),
    );
    add_line(&mut lines, "updated_at", &record.updated_at.to_rfc3339());

    // Health checks
    let check = |b: bool| if b { "\u{2713}" } else { "\u{2717}" };
    lines.push("  checks:".to_string());
    lines.push(format!(
        "    {:<23}{}",
        "agent_exited_clean:",
        check(record.checks.agent_exited_clean)
    ));
    lines.push(format!(
        "    {:<23}{}",
        "branch_pushed:",
        check(record.checks.branch_pushed)
    ));
    lines.push(format!(
        "    {:<23}{}",
        "pr_created:",
        check(record.checks.pr_created)
    ));
    lines.push(format!(
        "    {:<23}{}",
        "ci_passed:",
        check(record.checks.ci_passed)
    ));
    lines.push(format!(
        "    {:<23}{}",
        "reviews_approved:",
        check(record.checks.reviews_approved)
    ));
    lines.push(format!(
        "    {:<23}{}",
        "threads_resolved:",
        check(record.checks.threads_resolved)
    ));

    lines.join("\n")
}

pub async fn run_info(registry: Arc<dyn Registry>, arg: &str, json: bool) -> Result<()> {
    let record = resolve_record(registry.as_ref(), arg).await?;
    if json {
        let envelope = InfoOutputV1 {
            schema_version: STATUS_SCHEMA_VERSION,
            record: &record,
        };
        println!("{}", serde_json::to_string_pretty(&envelope)?);
    } else {
        println!("{}", format_info(&record));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atc_core::types::{Directive, HealthChecks, Status};
    use chrono::{DateTime, Utc};
    use std::path::PathBuf;

    fn full_record() -> DispatchRecord {
        DispatchRecord {
            id: "tasks--gitkb-42@implement@1773293500".to_string(),
            task_slug: Some("tasks/gitkb-42".to_string()),
            branch: "tasks--gitkb-42".to_string(),
            worktree_path: PathBuf::from("/tmp/worktrees/harmony/tasks-gitkb-42/gitkb"),
            session: "tasks--gitkb-42@implement@1773293500".to_string(),
            log_file: PathBuf::from("/tmp/test.jsonl"),
            status: Status::Done,
            directive: Directive::Implement,
            retries: 1,
            resolver: "task".to_string(),
            pr_urls: vec!["https://github.com/acme-org/acme-core/pull/275".to_string()],
            no_worktree: false,
            original_input: None,
            checks: HealthChecks {
                agent_exited_clean: true,
                branch_pushed: true,
                pr_created: true,
                ci_passed: true,
                reviews_approved: true,
                threads_resolved: true,
            },
            kb_root: None,
            cost_usd: Some(8.59),
            num_turns: Some(47),
            duration_ms: Some(592_000),
            artifacts: None,
            work_unit_id: None,
            dispatched_at: DateTime::parse_from_rfc3339("2026-03-12T05:31:41+00:00")
                .unwrap()
                .with_timezone(&Utc),
            updated_at: DateTime::parse_from_rfc3339("2026-03-12T07:45:00+00:00")
                .unwrap()
                .with_timezone(&Utc),
        }
    }

    #[test]
    fn test_format_info_full_record() {
        let output = format_info(&full_record());
        assert!(output.contains("id:"));
        assert!(output.contains("task_slug:"));
        assert!(output.contains("tasks/gitkb-42"));
        assert!(output.contains("status:"));
        assert!(output.contains("done"));
        assert!(output.contains("directive:"));
        assert!(output.contains("implement"));
        assert!(output.contains("resolver:"));
        assert!(output.contains("task"));
        assert!(output.contains("branch:"));
        assert!(output.contains("pr_urls:"));
        assert!(output.contains("pull/275"));
        assert!(output.contains("cost_usd:"));
        assert!(output.contains("$8.59"));
        assert!(output.contains("checks:"));
        assert!(output.contains("\u{2713}")); // check mark
    }

    #[test]
    fn test_format_info_omits_none_fields() {
        let mut record = full_record();
        record.task_slug = None;
        record.pr_urls = vec![];
        record.cost_usd = None;
        record.num_turns = None;
        record.duration_ms = None;
        record.retries = 0;

        let output = format_info(&record);
        assert!(!output.contains("task_slug:"));
        assert!(!output.contains("pr_urls:"));
        assert!(!output.contains("cost_usd:"));
        assert!(output.contains("id:"));
        assert!(output.contains("checks:"));
    }
}
