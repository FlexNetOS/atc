//! `atc status` — table view of all dispatch records.

use anyhow::Result;
use atc_core::registry::{Registry, StatusFilter};
use atc_core::types::{DispatchRecord, Status};
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
        "mode",
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
        let mode = r.mode.as_str().to_string();
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
            mode,
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

pub async fn run_status(
    registry: Arc<dyn Registry>,
    status_filter: Option<String>,
    json: bool,
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

    let width = terminal_width();
    let table = build_table(&records, width);
    println!("{table}");
    println!("{}", build_summary(&records));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atc_core::types::{HealthChecks, Mode};
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
            mode: Mode::Implement,
            retries: 0,
            resolver: "task".to_string(),
            pr_url: None,
            no_worktree: false,
            checks: HealthChecks::default(),
            cost_usd: Some(1.50),
            num_turns: Some(10),
            duration_ms: Some(592_000),
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
