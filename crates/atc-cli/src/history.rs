//! `atc history` — show all dispatches for a work unit (by task, PR, or branch).

use anyhow::Result;
use atc_core::registry::Registry;
use atc_core::types::{DispatchRecord, WorkUnit};
use std::sync::Arc;

#[cfg(test)]
use crate::status::format_pr_url;
use crate::status::{format_duration, format_pr_list};

/// Build the history table for a work unit's dispatches.
pub fn build_history_table(unit: &WorkUnit, dispatches: &[DispatchRecord]) -> String {
    use comfy_table::{presets::NOTHING, Table};

    let mut out = String::new();

    // Header
    let task_display = unit.task_slug.as_deref().unwrap_or(unit.id.as_str());
    out.push_str(&format!("Work Unit: {}\n", task_display));
    if let Some(ref branch) = unit.branch {
        out.push_str(&format!("  Branch: {}\n", branch));
    }
    if !unit.repos.is_empty() {
        out.push_str(&format!("  Repos:  {}\n", unit.repos.join(", ")));
    }
    if !unit.pr_urls.is_empty() {
        out.push_str(&format!("  PRs:    {}\n", format_pr_list(&unit.pr_urls)));
    }
    out.push_str(&format!("  Status: {}\n", unit.status.as_str()));
    out.push('\n');

    // Dispatch table
    let mut table = Table::new();
    table.load_preset(NOTHING);
    table.set_header(vec![
        "dispatched_at",
        "directive",
        "status",
        "cost",
        "duration",
        "pr_urls",
    ]);

    let total_cost: f64 = dispatches.iter().filter_map(|r| r.cost_usd).sum();

    for r in dispatches {
        let dispatched = r.dispatched_at.format("%Y-%m-%d %H:%M").to_string();
        let directive = r.directive.as_str().to_string();
        let status = r.status.as_str().to_string();
        let cost = r
            .cost_usd
            .map(|c| format!("${:.2}", c))
            .unwrap_or_else(|| "-".to_string());
        let duration = r
            .duration_ms
            .map(format_duration)
            .unwrap_or_else(|| "-".to_string());
        let prs = format_pr_list(&r.pr_urls);

        table.add_row(vec![dispatched, directive, status, cost, duration, prs]);
    }

    out.push_str(&table.to_string());
    out.push_str(&format!(
        "\nTotal: ${:.2} across {} dispatch{}",
        total_cost,
        dispatches.len(),
        if dispatches.len() == 1 { "" } else { "es" }
    ));
    out
}

pub async fn run_history(
    registry: Arc<dyn Registry>,
    slug: Option<&str>,
    pr: Option<&str>,
    branch: Option<&str>,
    json: bool,
) -> Result<()> {
    // Resolve to a work unit (search all statuses — history needs merged/closed units too)
    let unit = if let Some(slug) = slug {
        registry.find_work_unit_by_task_any_status(slug).await?
    } else if let Some(pr_url) = pr {
        registry.find_work_unit_by_pr(pr_url).await?
    } else if let Some(branch_name) = branch {
        registry
            .find_work_unit_by_branch_any_status(branch_name)
            .await?
    } else {
        anyhow::bail!("provide a task slug, --pr URL, or --branch name");
    };

    let Some(unit) = unit else {
        let target = slug.or(pr).or(branch).unwrap_or("(unknown)");
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({ "work_unit": null, "dispatches": [] })
                )?
            );
        } else {
            println!("No work unit found for: {}", target);
        }
        return Ok(());
    };

    let dispatches = registry.list_dispatches_for_work_unit(&unit.id).await?;

    if json {
        let out = serde_json::json!({
            "work_unit": unit,
            "dispatches": dispatches,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if dispatches.is_empty() {
        println!("Work unit {} has no dispatches.", unit.id);
        return Ok(());
    }

    println!("{}", build_history_table(&unit, &dispatches));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atc_core::types::*;
    use chrono::{DateTime, Utc};
    use std::path::PathBuf;

    fn sample_work_unit() -> WorkUnit {
        WorkUnit {
            id: "wu-test-001".to_string(),
            task_slug: Some("tasks/harmony-370".to_string()),
            branch: Some("tasks-harmony-370".to_string()),
            repos: vec!["open-source/atc".to_string()],
            pr_urls: vec!["https://github.com/harmony-labs/atc/pull/30".to_string()],
            status: WorkUnitStatus::Merged,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn sample_dispatch(directive: Directive, cost: Option<f64>) -> DispatchRecord {
        DispatchRecord {
            id: "test@implement@123".to_string(),
            task_slug: Some("tasks/harmony-370".to_string()),
            branch: "tasks-harmony-370".to_string(),
            worktree_path: PathBuf::from("/tmp/test"),
            session: "session".to_string(),
            log_file: PathBuf::from("/tmp/test.jsonl"),
            status: Status::Done,
            directive,
            retries: 0,
            resolver: "task".to_string(),
            pr_urls: vec![],
            no_worktree: false,
            original_input: None,
            checks: HealthChecks::default(),
            kb_root: None,
            cost_usd: cost,
            num_turns: Some(10),
            duration_ms: Some(300_000),
            artifacts: None,
            work_unit_id: Some("wu-test-001".to_string()),
            dispatched_at: DateTime::parse_from_rfc3339("2026-03-26T18:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn test_build_history_table_shows_task_and_totals() {
        let unit = sample_work_unit();
        let dispatches = vec![
            sample_dispatch(Directive::Implement, Some(4.20)),
            sample_dispatch(Directive::ReviewFix, Some(2.10)),
        ];
        let table = build_history_table(&unit, &dispatches);
        assert!(table.contains("tasks/harmony-370"));
        assert!(table.contains("tasks-harmony-370"));
        assert!(table.contains("atc#30"));
        assert!(table.contains("merged"));
        assert!(table.contains("$6.30"));
        assert!(table.contains("2 dispatches"));
    }

    #[test]
    fn test_build_history_table_single_dispatch() {
        let unit = sample_work_unit();
        let dispatches = vec![sample_dispatch(Directive::Implement, Some(3.50))];
        let table = build_history_table(&unit, &dispatches);
        assert!(table.contains("$3.50"));
        assert!(table.contains("1 dispatch"));
        assert!(!table.contains("dispatches"));
    }

    #[test]
    fn test_format_pr_url_github() {
        assert_eq!(
            format_pr_url("https://github.com/harmony-labs/atc/pull/30"),
            "atc#30"
        );
    }

    #[test]
    fn test_format_pr_url_unknown() {
        assert_eq!(
            format_pr_url("http://other.com/pr/1"),
            "http://other.com/pr/1"
        );
    }
}
