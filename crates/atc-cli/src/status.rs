//! `atc status` — table view of all dispatch records.

use anyhow::Result;
use atc_core::registry::{Registry, StatusFilter};
use atc_core::types::{DispatchRecord, Status, WorkUnit};
use std::sync::Arc;

/// Format duration_ms as human-friendly "Nm NNs".
pub(crate) fn format_duration(ms: u64) -> String {
    let total_secs = ms / 1000;
    let minutes = total_secs / 60;
    let seconds = total_secs % 60;
    if minutes > 0 {
        format!("{}m {:02}s", minutes, seconds)
    } else {
        format!("{}s", seconds)
    }
}

/// Truncate a string to `max_len` chars, appending `...` if truncated.
fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_len).collect();
        format!("{truncated}\u{2026}")
    }
}

/// Detect terminal width. Returns 120 if stdout is not a tty.
fn terminal_width() -> u16 {
    terminal_size::terminal_size()
        .map(|(w, _)| w.0)
        .unwrap_or(120)
}

/// Build the status table and summary.
pub fn build_table(records: &[DispatchRecord], width: u16) -> String {
    use comfy_table::{presets::NOTHING, Table};

    let narrow = width < 120;

    let mut table = Table::new();
    table.load_preset(NOTHING);
    table.set_header(vec![
        "dispatched_at",
        "status",
        "task",
        "directive",
        "pr_urls",
        "cost",
        "turns",
        "duration",
        "worktree",
    ]);

    for r in records {
        let dispatched = r.dispatched_at.format("%Y-%m-%dT%H:%M:%S").to_string();
        let status = r.status.as_str().to_string();
        let task = r.task_slug.as_deref().unwrap_or(&r.id);
        let task_display = if narrow {
            truncate(task, 40)
        } else {
            task.to_string()
        };
        let directive_str = r.directive.as_str().to_string();
        // Format PR URLs as compact "owner/repo#N" references
        let pr_urls_display = if r.pr_urls.is_empty() {
            "-".to_string()
        } else {
            r.pr_urls
                .iter()
                .map(|url| {
                    // "https://github.com/org/repo/pull/42" → "repo#42"
                    url.strip_prefix("https://github.com/")
                        .and_then(|path| {
                            let parts: Vec<&str> = path.split('/').collect();
                            if parts.len() >= 4 && parts[2] == "pull" {
                                Some(format!("{}#{}", parts[1], parts[3]))
                            } else {
                                None
                            }
                        })
                        .unwrap_or_else(|| url.clone())
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let cost = r
            .cost_usd
            .map(|c| format!("${:.2}", c))
            .unwrap_or_else(|| "-".to_string());
        let turns = r
            .num_turns
            .map(|t| t.to_string())
            .unwrap_or_else(|| "-".to_string());
        let duration = r
            .duration_ms
            .map(format_duration)
            .unwrap_or_else(|| "-".to_string());
        let worktree_str = r.worktree_path.to_string_lossy();
        let worktree = if narrow {
            truncate(&worktree_str, 40)
        } else {
            worktree_str.to_string()
        };

        table.add_row(vec![
            dispatched,
            status,
            task_display,
            directive_str,
            pr_urls_display,
            cost,
            turns,
            duration,
            worktree,
        ]);
    }

    table.to_string()
}

/// Build the summary line.
pub fn build_summary(records: &[DispatchRecord]) -> String {
    let mut running = 0u32;
    let mut done = 0u32;
    let mut failed = 0u32;
    let mut needs_human = 0u32;
    let mut needs_review = 0u32;
    let mut stopped = 0u32;
    let mut retrying = 0u32;
    let mut total_cost = 0.0f64;

    for r in records {
        match r.status {
            Status::Running => running += 1,
            Status::Done => done += 1,
            Status::Failed => failed += 1,
            Status::NeedsHuman => needs_human += 1,
            Status::NeedsReview => needs_review += 1,
            Status::Stopped => stopped += 1,
            Status::Retrying => retrying += 1,
        }
        if let Some(c) = r.cost_usd {
            total_cost += c;
        }
    }

    let mut parts = Vec::new();
    if running > 0 {
        parts.push(format!("{running} running"));
    }
    if done > 0 {
        parts.push(format!("{done} done"));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if needs_human > 0 {
        parts.push(format!("{needs_human} needs-human"));
    }
    if needs_review > 0 {
        parts.push(format!("{needs_review} needs-review"));
    }
    if stopped > 0 {
        parts.push(format!("{stopped} stopped"));
    }
    if retrying > 0 {
        parts.push(format!("{retrying} retrying"));
    }
    if parts.is_empty() {
        parts.push("0 records".to_string());
    }

    format!(
        "{} (of {} total, ${:.2})",
        parts.join(", "),
        records.len(),
        total_cost,
    )
}

/// Format PR URLs as compact "repo#N" references.
fn format_pr_url(url: &str) -> String {
    url.strip_prefix("https://github.com/")
        .and_then(|path| {
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() >= 4 && parts[2] == "pull" {
                Some(format!("{}#{}", parts[1], parts[3]))
            } else {
                None
            }
        })
        .unwrap_or_else(|| url.to_string())
}

/// Build a work-unit-grouped status table.
pub fn build_grouped_table(work_units: &[WorkUnit], records: &[DispatchRecord]) -> String {
    use comfy_table::{presets::NOTHING, Table};
    use std::collections::HashMap;

    // Group dispatches by work_unit_id
    let mut by_wu: HashMap<&str, Vec<&DispatchRecord>> = HashMap::new();
    let mut orphan_records: Vec<&DispatchRecord> = Vec::new();
    for r in records {
        if let Some(ref wu_id) = r.work_unit_id {
            by_wu.entry(wu_id.as_str()).or_default().push(r);
        } else {
            orphan_records.push(r);
        }
    }

    let mut table = Table::new();
    table.load_preset(NOTHING);
    table.set_header(vec![
        "task",
        "branch",
        "PRs",
        "dispatches",
        "status",
        "cost",
    ]);

    for wu in work_units {
        let task = wu.task_slug.as_deref().unwrap_or("(none)");
        let branch = wu.branch.as_deref().unwrap_or("-");
        let prs = if wu.pr_urls.is_empty() {
            "-".to_string()
        } else {
            wu.pr_urls
                .iter()
                .map(|u| format_pr_url(u))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let dispatches_for_wu = by_wu.get(wu.id.as_str());
        let dispatch_count = dispatches_for_wu.map(|d| d.len()).unwrap_or(0);
        let total_cost: f64 = dispatches_for_wu
            .map(|ds| ds.iter().filter_map(|r| r.cost_usd).sum())
            .unwrap_or(0.0);
        let dispatch_label = format!(
            "{} run{}",
            dispatch_count,
            if dispatch_count == 1 { "" } else { "s" }
        );
        let cost_str = if total_cost > 0.0 {
            format!("${:.2}", total_cost)
        } else {
            "-".to_string()
        };
        table.add_row(vec![
            task.to_string(),
            branch.to_string(),
            prs,
            dispatch_label,
            wu.status.as_str().to_string(),
            cost_str,
        ]);
    }

    // Show orphan dispatches (no work unit) as individual rows
    for r in &orphan_records {
        let task = r.task_slug.as_deref().unwrap_or("(none)");
        let prs = if r.pr_urls.is_empty() {
            "-".to_string()
        } else {
            r.pr_urls
                .iter()
                .map(|u| format_pr_url(u))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let cost_str = r
            .cost_usd
            .map(|c| format!("${:.2}", c))
            .unwrap_or_else(|| "-".to_string());
        table.add_row(vec![
            task.to_string(),
            r.branch.clone(),
            prs,
            "1 run".to_string(),
            r.status.as_str().to_string(),
            cost_str,
        ]);
    }

    table.to_string()
}

pub async fn run_status(
    registry: Arc<dyn Registry>,
    status_filter: Option<String>,
    json: bool,
    flat: bool,
) -> Result<()> {
    let filter = match &status_filter {
        Some(s) => {
            let parsed: Status = s.to_lowercase().parse()?;
            StatusFilter::One(parsed)
        }
        None => StatusFilter::All,
    };

    let records = registry.list(filter).await?;

    if json {
        let json_str = serde_json::to_string_pretty(&records)?;
        println!("{json_str}");
        return Ok(());
    }

    if records.is_empty() {
        println!("No dispatch records found.");
        return Ok(());
    }

    if flat {
        let width = terminal_width();
        let table = build_table(&records, width);
        println!("{table}");
        println!("{}", build_summary(&records));
    } else {
        // Default: work-unit-grouped view
        let work_units = registry.list_work_units().await?;
        if work_units.is_empty() {
            // No work units yet — fall back to flat view
            let width = terminal_width();
            let table = build_table(&records, width);
            println!("{table}");
            println!("{}", build_summary(&records));
        } else {
            let table = build_grouped_table(&work_units, &records);
            println!("{table}");
            println!("{}", build_summary(&records));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atc_core::types::{Directive, HealthChecks};
    use chrono::{DateTime, Utc};
    use std::path::PathBuf;

    fn sample_record(id: &str, status: Status) -> DispatchRecord {
        DispatchRecord {
            id: id.to_string(),
            task_slug: Some("tasks/gitkb-42".to_string()),
            branch: "branch".to_string(),
            worktree_path: PathBuf::from("/tmp/worktrees/harmony/my-task/gitkb"),
            session: "session@implement@123".to_string(),
            log_file: PathBuf::from("/tmp/test.jsonl"),
            status,
            directive: Directive::Implement,
            retries: 0,
            resolver: "task".to_string(),
            pr_urls: vec![],
            no_worktree: false,
            original_input: None,
            checks: HealthChecks::default(),
            kb_root: None,
            cost_usd: Some(1.50),
            num_turns: Some(10),
            duration_ms: Some(592_000),
            artifacts: None,
            work_unit_id: None,
            dispatched_at: DateTime::parse_from_rfc3339("2026-03-12T05:31:41Z")
                .unwrap()
                .with_timezone(&Utc),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(592_000), "9m 52s");
        assert_eq!(format_duration(60_000), "1m 00s");
        assert_eq!(format_duration(5_000), "5s");
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(3_661_000), "61m 01s");
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("short", 40), "short");
        let long = "a".repeat(50);
        let result = truncate(&long, 40);
        assert_eq!(result.chars().count(), 41); // 40 + ellipsis
        assert!(result.ends_with('\u{2026}'));
    }

    #[test]
    fn test_build_table_wide_terminal() {
        let records = vec![sample_record("id-1", Status::Running)];
        let table = build_table(&records, 160);
        assert!(table.contains("tasks/gitkb-42"));
        assert!(table.contains("running"));
        assert!(table.contains("$1.50"));
        assert!(table.contains("9m 52s"));
    }

    #[test]
    fn test_build_summary() {
        let records = vec![
            sample_record("id-1", Status::Running),
            sample_record("id-2", Status::Done),
            sample_record("id-3", Status::Failed),
            sample_record("id-4", Status::NeedsHuman),
        ];
        let summary = build_summary(&records);
        assert!(summary.contains("1 running"));
        assert!(summary.contains("1 done"));
        assert!(summary.contains("1 failed"));
        assert!(summary.contains("1 needs-human"));
        assert!(summary.contains("of 4 total"));
        assert!(summary.contains("$6.00"));
    }

    #[test]
    fn test_build_summary_includes_all_statuses() {
        let records = vec![
            sample_record("id-1", Status::Running),
            sample_record("id-2", Status::NeedsReview),
            sample_record("id-3", Status::Stopped),
            sample_record("id-4", Status::Retrying),
        ];
        let summary = build_summary(&records);
        assert!(summary.contains("1 running"));
        assert!(summary.contains("1 needs-review"));
        assert!(summary.contains("1 stopped"));
        assert!(summary.contains("1 retrying"));
        assert!(summary.contains("of 4 total"));
    }

    #[test]
    fn test_build_summary_omits_zero_counts() {
        let records = vec![sample_record("id-1", Status::Done)];
        let summary = build_summary(&records);
        assert!(summary.contains("1 done"));
        assert!(!summary.contains("running"));
        assert!(!summary.contains("failed"));
    }
}
