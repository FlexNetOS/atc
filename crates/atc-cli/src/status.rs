//! `atc status` — table view of all dispatch records.

use anyhow::{Context, Result};
use atc_core::config::PagerConfig;
use atc_core::registry::{Registry, StatusFilter};
use atc_core::types::{DispatchRecord, Status, WorkUnit};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::sync::Arc;

use crate::pager::setup_pager;
use crate::style::{apply, dim, render_cost, render_status, render_work_unit_status, strong};

/// Maximum PR URLs to render inline in a cell. Excess collapses to `+N more`.
const PR_LIST_INLINE_CAP: usize = 3;

use crate::output_schema::SCHEMA_VERSION;

/// Default statuses shown by `atc status` when no filter is given.
/// Status-only filter — predictable, no hidden time component.
pub const DEFAULT_STATUSES: &[Status] = &[
    Status::Running,
    Status::Retrying,
    Status::NeedsHuman,
    Status::NeedsReview,
];

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
        apply("dispatched_at", dim()),
        apply("status", dim()),
        apply("task", dim()),
        apply("directive", dim()),
        apply("pr_urls", dim()),
        apply("cost", dim()),
        apply("turns", dim()),
        apply("duration", dim()),
        apply("worktree", dim()),
    ]);

    for r in records {
        let dispatched = r.dispatched_at.format("%Y-%m-%dT%H:%M:%S").to_string();
        let status = render_status(r.status);
        let task = r.task_slug.as_deref().unwrap_or(&r.id);
        let task_display = if narrow {
            truncate(task, 40)
        } else {
            task.to_string()
        };
        let task_styled = apply(task_display, strong());
        let directive_str = r.directive.as_str().to_string();
        let pr_urls_display = format_pr_list(&r.pr_urls);
        let cost = render_cost(r.cost_usd);
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
            task_styled,
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
    let counts = StatusSummary::from_records(records);

    let mut parts = Vec::new();
    if counts.running > 0 {
        parts.push(format!("{} running", counts.running));
    }
    if counts.done > 0 {
        parts.push(format!("{} done", counts.done));
    }
    if counts.failed > 0 {
        parts.push(format!("{} failed", counts.failed));
    }
    if counts.needs_human > 0 {
        parts.push(format!("{} needs-human", counts.needs_human));
    }
    if counts.needs_review > 0 {
        parts.push(format!("{} needs-review", counts.needs_review));
    }
    if counts.stopped > 0 {
        parts.push(format!("{} stopped", counts.stopped));
    }
    if counts.retrying > 0 {
        parts.push(format!("{} retrying", counts.retrying));
    }
    if parts.is_empty() {
        parts.push("0 records".to_string());
    }

    format!(
        "{} (of {} total, ${:.2})",
        parts.join(", "),
        counts.total,
        counts.total_cost_usd,
    )
}

/// Format a list of PR URLs as a comma-separated compact string, or "-" if empty.
///
/// When more than [`PR_LIST_INLINE_CAP`] URLs are present, the excess collapses
/// into `+N more`. Invalid URLs render as `(invalid)` and emit a warn.
pub fn format_pr_list(urls: &[String]) -> String {
    if urls.is_empty() {
        return "-".to_string();
    }
    let rendered: Vec<String> = urls.iter().map(|u| format_pr_url(u)).collect();
    if rendered.len() <= PR_LIST_INLINE_CAP {
        rendered.join(", ")
    } else {
        let head = &rendered[..PR_LIST_INLINE_CAP];
        let extra = rendered.len() - PR_LIST_INLINE_CAP;
        format!("{}, +{} more", head.join(", "), extra)
    }
}

/// Strict GitHub PR URL validator. Allows optional `#issuecomment-NNN` suffix.
fn is_valid_github_pr_url(url: &str) -> bool {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"^https://github\.com/[^/]+/[^/]+/pull/\d+(#issuecomment-\d+)?$")
            .expect("PR URL regex compiles")
    });
    re.is_match(url)
}

/// Format PR URLs as compact "repo#N" references. Rejects malformed URLs.
pub fn format_pr_url(url: &str) -> String {
    if !is_valid_github_pr_url(url) {
        tracing::warn!(url = %url, "rejected malformed PR URL during render");
        return "(invalid)".to_string();
    }
    url.strip_prefix("https://github.com/")
        .and_then(|path| {
            // Strip optional #issuecomment-N suffix before splitting
            let path = path.split('#').next().unwrap_or(path);
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() >= 4 && parts[2] == "pull" {
                Some(format!("{}#{}", parts[1], parts[3]))
            } else {
                None
            }
        })
        // Validation above guarantees Some(...), but fall through defensively.
        .unwrap_or_else(|| "(invalid)".to_string())
}

/// Build a work-unit-grouped status table.
pub fn build_grouped_table(work_units: &[WorkUnit], records: &[DispatchRecord]) -> String {
    use comfy_table::{presets::NOTHING, Table};
    use std::collections::HashMap;

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
        apply("task", dim()),
        apply("branch", dim()),
        apply("PRs", dim()),
        apply("dispatches", dim()),
        apply("status", dim()),
        apply("cost", dim()),
    ]);

    for wu in work_units {
        let task = wu.task_slug.as_deref().unwrap_or("(none)");
        let branch = wu.branch.as_deref().unwrap_or("-");
        let prs = format_pr_list(&wu.pr_urls);
        let dispatches_for_wu = by_wu.get(wu.id.as_str());
        let dispatch_count = dispatches_for_wu.map(|d| d.len()).unwrap_or(0);
        let dispatch_label = format!(
            "{} run{}",
            dispatch_count,
            if dispatch_count == 1 { "" } else { "s" }
        );
        let has_any_cost = dispatches_for_wu
            .map(|ds| ds.iter().any(|r| r.cost_usd.is_some()))
            .unwrap_or(false);
        let cost_str = if has_any_cost {
            let total: f64 = dispatches_for_wu
                .map(|ds| ds.iter().filter_map(|r| r.cost_usd).sum())
                .unwrap_or(0.0);
            render_cost(Some(total))
        } else {
            render_cost(None)
        };
        table.add_row(vec![
            apply(task, strong()),
            branch.to_string(),
            prs,
            dispatch_label,
            render_work_unit_status(wu.status),
            cost_str,
        ]);
    }

    for r in &orphan_records {
        let task = r.task_slug.as_deref().unwrap_or(r.id.as_str());
        let prs = format_pr_list(&r.pr_urls);
        let cost_str = render_cost(r.cost_usd);
        table.add_row(vec![
            apply(task, strong()),
            r.branch.clone(),
            prs,
            "1 run".to_string(),
            render_status(r.status),
            cost_str,
        ]);
    }

    table.to_string()
}

/// Aggregated counts used for both the human summary and the JSON envelope.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StatusSummary {
    pub running: u32,
    pub done: u32,
    pub failed: u32,
    pub needs_human: u32,
    pub needs_review: u32,
    pub stopped: u32,
    pub retrying: u32,
    pub total: u32,
    pub total_cost_usd: f64,
}

impl StatusSummary {
    pub fn from_records(records: &[DispatchRecord]) -> Self {
        let mut s = StatusSummary::default();
        for r in records {
            match r.status {
                Status::Running => s.running += 1,
                Status::Done => s.done += 1,
                Status::Failed => s.failed += 1,
                Status::NeedsHuman => s.needs_human += 1,
                Status::NeedsReview => s.needs_review += 1,
                Status::Stopped => s.stopped += 1,
                Status::Retrying => s.retrying += 1,
            }
            if let Some(c) = r.cost_usd {
                s.total_cost_usd += c;
            }
        }
        s.total = records.len() as u32;
        s
    }
}

/// JSON envelope for `atc status --json`. Stable across v1 of the schema.
#[derive(Debug, Serialize)]
pub struct StatusOutputV1<'a> {
    pub schema_version: u32,
    pub records: &'a [DispatchRecord],
    pub work_units: &'a [WorkUnit],
    pub summary: StatusSummary,
}

/// CLI options collected by the `Status` subcommand. Kept as a struct so the
/// arg list stays maintainable as filters grow.
#[derive(Debug, Clone)]
pub struct StatusOpts {
    pub status_filter: Option<String>,
    pub json: bool,
    pub flat: bool,
    pub all: bool,
    pub include_done: bool,
    pub since: Option<String>,
    pub reverse: bool,
    pub no_pager: bool,
}

/// Parse `--since 24h` / `--since 2d` etc. via `humantime`.
fn parse_since(s: &str) -> Result<Duration> {
    let std_dur = humantime::parse_duration(s)
        .with_context(|| format!("invalid --since value '{s}' (try 24h, 2d, 1w)"))?;
    Duration::from_std(std_dur).context("--since duration too large")
}

/// Apply CLI filters to a registry result set.
///
/// `--since` is honored even when `--status` or `--all` narrows the set —
/// status selection narrows the set; it does not disable the recency bound.
pub fn apply_filters(
    mut records: Vec<DispatchRecord>,
    explicit_status: Option<&str>,
    all: bool,
    include_done: bool,
    since: Option<&Duration>,
    now: DateTime<Utc>,
) -> Result<Vec<DispatchRecord>> {
    let explicit_status = explicit_status
        .map(|s| s.to_lowercase().parse::<Status>())
        .transpose()?;
    let cutoff = since.map(|d| now - *d);

    records.retain(|r| {
        if let Some(status) = explicit_status {
            // `--status X --since Y` narrows by status AND bounds by recency.
            if r.status != status {
                return false;
            }
            if let Some(c) = cutoff {
                if r.updated_at < c {
                    return false;
                }
            }
            return true;
        }
        if all {
            // `--all --since Y` keeps every status but still bounds by recency.
            if let Some(c) = cutoff {
                if r.updated_at < c {
                    return false;
                }
            }
            return true;
        }

        // Default mode: interesting statuses (running/retrying/needs-*) are
        // kept unconditionally; --since only bounds non-default-status rows.
        let in_default = DEFAULT_STATUSES.contains(&r.status);
        if include_done {
            // Drop stopped; keep interesting unconditionally; bound done/failed
            // by --since when set (otherwise keep all).
            if matches!(r.status, Status::Stopped) {
                return false;
            }
            if !in_default {
                if let Some(c) = cutoff {
                    if r.updated_at < c {
                        return false;
                    }
                }
            }
        } else if !in_default {
            // Outside the default-interesting set; only kept by --since fallback.
            if let Some(c) = cutoff {
                if r.updated_at < c {
                    return false;
                }
            } else {
                return false;
            }
        }
        true
    });
    Ok(records)
}

/// Order records for rendering. Default: newest at the bottom of the buffer.
pub fn order_for_render(mut records: Vec<DispatchRecord>, reverse: bool) -> Vec<DispatchRecord> {
    if reverse {
        // Newest at top — registry already returned DESC, leave as-is.
        records
    } else {
        // Newest at bottom — flip DESC into ASC.
        records.reverse();
        records
    }
}

pub async fn run_status(
    registry: Arc<dyn Registry>,
    pager_config: Option<&PagerConfig>,
    opts: StatusOpts,
) -> Result<()> {
    let raw_records = registry.list(StatusFilter::All).await?;

    let since = match opts.since.as_deref() {
        Some(s) => Some(parse_since(s)?),
        None => None,
    };

    let filtered = apply_filters(
        raw_records,
        opts.status_filter.as_deref(),
        opts.all,
        opts.include_done,
        since.as_ref(),
        Utc::now(),
    )?;
    let records = order_for_render(filtered, opts.reverse);

    if opts.json {
        let work_units = registry.list_work_units().await?;
        let visible_ids: std::collections::HashSet<&str> = records
            .iter()
            .filter_map(|r| r.work_unit_id.as_deref())
            .collect();
        let work_units_filtered: Vec<WorkUnit> = work_units
            .into_iter()
            .filter(|wu| visible_ids.contains(wu.id.as_str()))
            .collect();
        let summary = StatusSummary::from_records(&records);
        let envelope = StatusOutputV1 {
            schema_version: SCHEMA_VERSION,
            records: &records,
            work_units: &work_units_filtered,
            summary,
        };
        println!("{}", serde_json::to_string_pretty(&envelope)?);
        return Ok(());
    }

    // Capture terminal width *before* the pager replaces fd 1 with a pipe.
    // After setup_pager(), terminal_size() consults the pipe and falls back
    // to the default, defeating narrow-terminal truncation.
    let width = terminal_width();

    // Pager — only attached for non-JSON, non-no-pager runs. Must be acquired
    // before any colored writes so `less -R` sees the escapes.
    let _pager_guard = if opts.no_pager {
        None
    } else {
        setup_pager(pager_config)
    };

    if records.is_empty() {
        println!("No dispatch records found.");
        if !opts.all && opts.status_filter.is_none() {
            println!(
                "(default filter: status ∈ {{running, retrying, needs-human, needs-review}})."
            );
            println!("hint: try `atc status --all` or `atc status --since 24h`.");
        }
        return Ok(());
    }

    if opts.flat {
        let table = build_table(&records, width);
        println!("{table}");
        println!("{}", build_summary(&records));
    } else {
        let mut work_units = registry.list_work_units().await?;
        let visible_ids: std::collections::HashSet<&str> = records
            .iter()
            .filter_map(|r| r.work_unit_id.as_deref())
            .collect();
        work_units.retain(|wu| visible_ids.contains(wu.id.as_str()));
        if work_units.is_empty() {
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
        assert_eq!(result.chars().count(), 41);
        assert!(result.ends_with('\u{2026}'));
    }

    #[test]
    fn test_build_table_wide_terminal() {
        crate::style::set_color_mode(crate::style::ColorMode::Never);
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

    #[test]
    fn test_format_pr_list_collapses_excess() {
        let urls: Vec<String> = (1..=7)
            .map(|i| format!("https://github.com/acme/repo/pull/{i}"))
            .collect();
        let out = format_pr_list(&urls);
        assert!(
            out.contains("repo#1, repo#2, repo#3, +4 more"),
            "got: {out}"
        );
    }

    #[test]
    fn test_format_pr_list_under_cap_no_summary() {
        let urls = vec![
            "https://github.com/acme/repo/pull/1".to_string(),
            "https://github.com/acme/repo/pull/2".to_string(),
        ];
        let out = format_pr_list(&urls);
        assert_eq!(out, "repo#1, repo#2");
    }

    #[test]
    fn test_format_pr_url_invalid_renders_invalid() {
        // Garbage like the bug report — code fragment leaking through extraction.
        let bad = "atc#36\".to_string()],\n";
        assert_eq!(format_pr_url(bad), "(invalid)");
    }

    #[test]
    fn test_format_pr_url_valid_with_issuecomment() {
        let url = "https://github.com/acme/repo/pull/42#issuecomment-12345";
        assert_eq!(format_pr_url(url), "repo#42");
    }

    #[test]
    fn test_apply_filters_default_keeps_running_and_actionable() {
        let now = DateTime::parse_from_rfc3339("2026-04-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut r1 = sample_record("id-1", Status::Running);
        r1.updated_at = now;
        let mut r2 = sample_record("id-2", Status::Done);
        r2.updated_at = now;
        let mut r3 = sample_record("id-3", Status::NeedsReview);
        r3.updated_at = now;

        let out = apply_filters(vec![r1, r2, r3], None, false, false, None, now).unwrap();
        let statuses: Vec<Status> = out.iter().map(|r| r.status).collect();
        assert_eq!(statuses, vec![Status::Running, Status::NeedsReview]);
    }

    #[test]
    fn test_apply_filters_all_keeps_everything() {
        let now = Utc::now();
        let r1 = sample_record("id-1", Status::Running);
        let r2 = sample_record("id-2", Status::Done);
        let r3 = sample_record("id-3", Status::Stopped);

        let out = apply_filters(vec![r1, r2, r3], None, true, false, None, now).unwrap();
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn test_apply_filters_include_done_with_since_bounds_done_records() {
        // --include-done alone keeps all done/failed; combined with --since,
        // it bounds non-default-status records by the cutoff. Interesting
        // statuses (running/retrying/needs-*) are always kept.
        let now = DateTime::parse_from_rfc3339("2026-04-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut running = sample_record("running", Status::Running);
        running.updated_at = now - Duration::days(30); // ancient — still kept
        let mut recent_done = sample_record("done-recent", Status::Done);
        recent_done.updated_at = now - Duration::hours(2);
        let mut old_done = sample_record("done-old", Status::Done);
        old_done.updated_at = now - Duration::days(10);

        let since = Duration::hours(24);
        let out = apply_filters(
            vec![running.clone(), recent_done.clone(), old_done.clone()],
            None,
            false,
            true,
            Some(&since),
            now,
        )
        .unwrap();
        let ids: Vec<&str> = out.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["running", "done-recent"]);

        // Without --since, --include-done keeps all done.
        let out = apply_filters(
            vec![running, recent_done, old_done],
            None,
            false,
            true,
            None,
            now,
        )
        .unwrap();
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn test_apply_filters_status_filter_respects_since() {
        // `--status done --since 24h` must drop records older than the cutoff,
        // not bypass `--since` via an early return.
        let now = DateTime::parse_from_rfc3339("2026-04-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut recent_done = sample_record("done-recent", Status::Done);
        recent_done.updated_at = now - Duration::hours(2);
        let mut old_done = sample_record("done-old", Status::Done);
        old_done.updated_at = now - Duration::days(10);
        let mut recent_running = sample_record("running-recent", Status::Running);
        recent_running.updated_at = now - Duration::hours(1);

        let since = Duration::hours(24);
        let out = apply_filters(
            vec![recent_done, old_done, recent_running],
            Some("done"),
            false,
            false,
            Some(&since),
            now,
        )
        .unwrap();
        let ids: Vec<&str> = out.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["done-recent"]);
    }

    #[test]
    fn test_apply_filters_all_respects_since() {
        // `--all --since 24h` keeps every status but still bounds by recency.
        let now = DateTime::parse_from_rfc3339("2026-04-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut recent_running = sample_record("running-recent", Status::Running);
        recent_running.updated_at = now - Duration::hours(2);
        let mut old_running = sample_record("running-old", Status::Running);
        old_running.updated_at = now - Duration::days(10);
        let mut recent_stopped = sample_record("stopped-recent", Status::Stopped);
        recent_stopped.updated_at = now - Duration::hours(3);

        let since = Duration::hours(24);
        let out = apply_filters(
            vec![recent_running, old_running, recent_stopped],
            None,
            true,
            false,
            Some(&since),
            now,
        )
        .unwrap();
        let mut ids: Vec<&str> = out.iter().map(|r| r.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["running-recent", "stopped-recent"]);
    }

    #[test]
    fn test_apply_filters_since_admits_recent_done() {
        let now = DateTime::parse_from_rfc3339("2026-04-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut recent_done = sample_record("done-recent", Status::Done);
        recent_done.updated_at = now - Duration::hours(2);
        let mut old_done = sample_record("done-old", Status::Done);
        old_done.updated_at = now - Duration::days(10);

        let since = Duration::hours(24);
        let out = apply_filters(
            vec![recent_done, old_done],
            None,
            false,
            false,
            Some(&since),
            now,
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "done-recent");
    }

    #[test]
    fn test_order_for_render_default_newest_at_bottom() {
        // Registry returns DESC (newest first); we flip to ASC so newest sits at bottom.
        let mut a = sample_record("a", Status::Running);
        a.dispatched_at = DateTime::parse_from_rfc3339("2026-04-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut b = sample_record("b", Status::Running);
        b.dispatched_at = DateTime::parse_from_rfc3339("2026-04-25T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        // DESC input: a (newest) first, b (older) second.
        let out = order_for_render(vec![a.clone(), b.clone()], false);
        assert_eq!(out[0].id, "b");
        assert_eq!(out[1].id, "a");

        // --reverse keeps DESC.
        let out_rev = order_for_render(vec![a, b], true);
        assert_eq!(out_rev[0].id, "a");
        assert_eq!(out_rev[1].id, "b");
    }

    #[test]
    fn test_status_summary_counts_and_total_cost() {
        let records = vec![
            sample_record("a", Status::Running),
            sample_record("b", Status::Done),
            sample_record("c", Status::Failed),
        ];
        let s = StatusSummary::from_records(&records);
        assert_eq!(s.total, 3);
        assert_eq!(s.running, 1);
        assert_eq!(s.done, 1);
        assert_eq!(s.failed, 1);
        assert!((s.total_cost_usd - 4.5).abs() < 1e-9);
    }

    #[test]
    fn test_json_envelope_has_schema_version() {
        let records: Vec<DispatchRecord> = vec![sample_record("id", Status::Running)];
        let work_units: Vec<WorkUnit> = vec![];
        let envelope = StatusOutputV1 {
            schema_version: SCHEMA_VERSION,
            records: &records,
            work_units: &work_units,
            summary: StatusSummary::from_records(&records),
        };
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert!(json["records"].is_array());
        assert!(json["work_units"].is_array());
        assert!(json["summary"].is_object());
        assert_eq!(json["summary"]["total"], 1);
    }
}
