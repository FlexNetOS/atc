//! `atc sessions` — keyboard switchboard for ATC dispatch sessions.

use anyhow::{bail, Context, Result};
use atc_core::config::AtcConfig;
use atc_core::executor::AgentExecutor;
use atc_core::registry::{Registry, StatusFilter};
use atc_core::types::{
    atc_session_uri, DispatchRecord, OpenSessionPreview, RunOpts, Status, TerminalLocator,
    TerminalStatus, TerminalStatusState, WorkUnit,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use clap::ValueEnum;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{Frame, Terminal};
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::warn;

use crate::output_schema::SCHEMA_VERSION;
use crate::status::{format_pr_list, DEFAULT_STATUSES};
use atc_core::terminal_text::{display_text, terminal_safe_json_pretty};

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(250);
const DEFAULT_RECENT_TERMINAL_HOURS: i64 = 24;

#[derive(Debug, Clone)]
pub struct SessionsOpts {
    pub task: Option<String>,
    pub work_unit: Option<String>,
    pub branch: Option<String>,
    pub provider: Option<String>,
    pub status: Option<String>,
    pub search: Option<String>,
    pub group: SessionGroupBy,
    pub all: bool,
    pub poll_interval: Option<String>,
    pub once: bool,
    pub json: bool,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, ValueEnum, PartialOrd, Ord, Hash,
)]
#[serde(rename_all = "kebab-case")]
#[clap(rename_all = "kebab-case")]
pub enum SessionGroupBy {
    Task,
    WorkUnit,
    Branch,
    Provider,
    Status,
    #[default]
    None,
}

impl SessionGroupBy {
    fn label(self) -> &'static str {
        match self {
            SessionGroupBy::Task => "task",
            SessionGroupBy::WorkUnit => "work-unit",
            SessionGroupBy::Branch => "branch",
            SessionGroupBy::Provider => "provider",
            SessionGroupBy::Status => "status",
            SessionGroupBy::None => "none",
        }
    }

    fn next(self) -> Self {
        match self {
            SessionGroupBy::None => SessionGroupBy::Task,
            SessionGroupBy::Task => SessionGroupBy::WorkUnit,
            SessionGroupBy::WorkUnit => SessionGroupBy::Branch,
            SessionGroupBy::Branch => SessionGroupBy::Provider,
            SessionGroupBy::Provider => SessionGroupBy::Status,
            SessionGroupBy::Status => SessionGroupBy::None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionFilter {
    pub task: Option<String>,
    pub work_unit: Option<String>,
    pub branch: Option<String>,
    pub provider: Option<String>,
    pub status: Option<Status>,
    pub search: Option<String>,
    pub all: bool,
}

impl SessionFilter {
    fn from_opts(opts: &SessionsOpts) -> Result<Self> {
        let status = opts
            .status
            .as_deref()
            .map(str::parse::<Status>)
            .transpose()?;
        Ok(Self {
            task: opts.task.clone(),
            work_unit: opts.work_unit.clone(),
            branch: opts.branch.clone(),
            provider: opts.provider.clone(),
            status,
            search: opts.search.clone().filter(|s| !s.trim().is_empty()),
            all: opts.all,
        })
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ActionAvailability {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ActionAvailability {
    fn enabled() -> Self {
        Self {
            enabled: true,
            reason: None,
        }
    }

    fn disabled(reason: impl Into<String>) -> Self {
        Self {
            enabled: false,
            reason: Some(reason.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SessionActionState {
    pub info: ActionAvailability,
    pub logs: ActionAvailability,
    pub follow_logs: ActionAvailability,
    pub attach: ActionAvailability,
    pub redirect: ActionAvailability,
    pub resume: ActionAvailability,
    pub stop: ActionAvailability,
    pub cleanup: ActionAvailability,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SessionRow {
    pub id: String,
    pub uri: String,
    pub task_slug: Option<String>,
    pub work_unit_id: Option<String>,
    pub group_key: String,
    pub branch: String,
    pub provider: String,
    pub provider_session_id: Option<String>,
    pub transcript_cwd: Option<String>,
    pub resume_of_dispatch_id: Option<String>,
    pub status: Status,
    pub directive: String,
    pub resolver: String,
    pub session: String,
    pub worktree_path: String,
    pub log_file: Option<String>,
    pub pr_urls: Vec<String>,
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u32>,
    pub duration_ms: Option<u64>,
    pub terminal_locator: Option<TerminalLocator>,
    pub terminal_status: TerminalStatus,
    pub open_shell: OpenSessionPreview,
    pub dispatched_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub actions: SessionActionState,
}

impl SessionRow {
    fn display_task(&self) -> &str {
        self.task_slug.as_deref().unwrap_or("(no task)")
    }

    fn display_work_unit(&self) -> &str {
        self.work_unit_id.as_deref().unwrap_or("-")
    }

    fn display_provider_session(&self) -> &str {
        self.provider_session_id.as_deref().unwrap_or("-")
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct SessionSummary {
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

impl SessionSummary {
    fn from_rows(rows: &[SessionRow]) -> Self {
        let mut summary = Self::default();
        for row in rows {
            match row.status {
                Status::Running => summary.running += 1,
                Status::Done => summary.done += 1,
                Status::Failed => summary.failed += 1,
                Status::NeedsHuman => summary.needs_human += 1,
                Status::NeedsReview => summary.needs_review += 1,
                Status::Stopped => summary.stopped += 1,
                Status::Retrying => summary.retrying += 1,
            }
            if let Some(cost) = row.cost_usd {
                summary.total_cost_usd += cost;
            }
        }
        summary.total = rows.len() as u32;
        summary
    }

    fn human(&self) -> String {
        let mut parts = Vec::new();
        if self.running > 0 {
            parts.push(format!("{} running", self.running));
        }
        if self.retrying > 0 {
            parts.push(format!("{} retrying", self.retrying));
        }
        if self.needs_human > 0 {
            parts.push(format!("{} needs-human", self.needs_human));
        }
        if self.needs_review > 0 {
            parts.push(format!("{} needs-review", self.needs_review));
        }
        if self.done > 0 {
            parts.push(format!("{} done", self.done));
        }
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        if self.stopped > 0 {
            parts.push(format!("{} stopped", self.stopped));
        }
        if parts.is_empty() {
            parts.push("0 sessions".to_string());
        }
        format!(
            "{} (of {} total, ${:.2})",
            parts.join(", "),
            self.total,
            self.total_cost_usd
        )
    }
}

#[derive(Debug, Serialize)]
pub struct SessionsOutputV1 {
    pub schema_version: u32,
    pub rows: Vec<SessionRow>,
    pub work_units: Vec<WorkUnit>,
    pub summary: SessionSummary,
    pub group: SessionGroupBy,
}

#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub rows: Vec<SessionRow>,
    pub work_units: Vec<WorkUnit>,
    pub summary: SessionSummary,
    pub group: SessionGroupBy,
}

impl SessionSnapshot {
    fn output(&self) -> SessionsOutputV1 {
        SessionsOutputV1 {
            schema_version: SCHEMA_VERSION,
            rows: self.rows.clone(),
            work_units: self.work_units.clone(),
            summary: self.summary.clone(),
            group: self.group,
        }
    }
}

pub async fn run_sessions(
    config: &AtcConfig,
    registry: Arc<dyn Registry>,
    executor: Arc<dyn AgentExecutor>,
    opts: SessionsOpts,
) -> Result<()> {
    let filter = SessionFilter::from_opts(&opts)?;
    let snapshot = load_snapshot(registry.as_ref(), &filter, opts.group).await?;

    if opts.json {
        println!("{}", terminal_safe_json_pretty(&snapshot.output())?);
        return Ok(());
    }

    if opts.once {
        print!("{}", render_once(&snapshot, Utc::now()));
        return Ok(());
    }

    let poll_interval = parse_poll_interval(opts.poll_interval.as_deref())?;
    run_tui(
        config,
        registry,
        executor,
        filter,
        opts.group,
        poll_interval,
        snapshot,
    )
    .await
}

pub async fn load_snapshot(
    registry: &dyn Registry,
    filter: &SessionFilter,
    group: SessionGroupBy,
) -> Result<SessionSnapshot> {
    let now = Utc::now();
    let records = registry.list(snapshot_status_filter(filter, now)).await?;
    let work_unit_ids = work_unit_ids_for_records(&records);
    let work_units = registry.list_work_units_by_ids(&work_unit_ids).await?;
    let mut snapshot = build_snapshot_at(records, work_units, filter, group, now);
    refresh_terminal_fields(&mut snapshot.rows).await;
    Ok(snapshot)
}

fn work_unit_ids_for_records(records: &[DispatchRecord]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for id in records
        .iter()
        .filter_map(|record| record.work_unit_id.as_ref())
    {
        if seen.insert(id.as_str()) {
            ids.push(id.clone());
        }
    }
    ids
}

fn snapshot_status_filter(filter: &SessionFilter, now: DateTime<Utc>) -> StatusFilter {
    if let Some(status) = filter.status {
        StatusFilter::by_status(status)
    } else if filter.all {
        StatusFilter::All
    } else {
        let recent_terminal_cutoff = now - ChronoDuration::hours(DEFAULT_RECENT_TERMINAL_HOURS);
        StatusFilter::any_or_updated_since(DEFAULT_STATUSES.to_vec(), recent_terminal_cutoff)
    }
}

pub fn build_snapshot(
    records: Vec<DispatchRecord>,
    work_units: Vec<WorkUnit>,
    filter: &SessionFilter,
    group: SessionGroupBy,
) -> SessionSnapshot {
    build_snapshot_at(records, work_units, filter, group, Utc::now())
}

fn build_snapshot_at(
    records: Vec<DispatchRecord>,
    work_units: Vec<WorkUnit>,
    filter: &SessionFilter,
    group: SessionGroupBy,
    now: DateTime<Utc>,
) -> SessionSnapshot {
    let work_unit_ids: HashSet<String> = records
        .iter()
        .filter_map(|record| record.work_unit_id.clone())
        .collect();
    let visible_work_units: Vec<WorkUnit> = work_units
        .into_iter()
        .filter(|wu| work_unit_ids.contains(&wu.id))
        .collect();
    let work_units_by_id: HashMap<String, WorkUnit> = visible_work_units
        .iter()
        .cloned()
        .map(|wu| (wu.id.clone(), wu))
        .collect();

    let mut rows: Vec<SessionRow> = records
        .into_iter()
        .filter(|record| record_matches_filter(record, &work_units_by_id, filter, now))
        .map(|record| row_from_record(record, &work_units_by_id, group))
        .collect();
    sort_session_rows(&mut rows, group);

    let visible_ids: HashSet<String> = rows
        .iter()
        .filter_map(|row| row.work_unit_id.clone())
        .collect();
    let filtered_work_units = visible_work_units
        .into_iter()
        .filter(|wu| visible_ids.contains(&wu.id))
        .collect();
    let summary = SessionSummary::from_rows(&rows);

    SessionSnapshot {
        rows,
        work_units: filtered_work_units,
        summary,
        group,
    }
}

fn row_from_record(
    record: DispatchRecord,
    work_units_by_id: &HashMap<String, WorkUnit>,
    group: SessionGroupBy,
) -> SessionRow {
    let group_key = group_key(&record, work_units_by_id, group);
    let task_slug = task_slug_for_record(&record, work_units_by_id).map(str::to_string);
    let log_file = path_to_nonempty_string(&record.log_file);
    let terminal_locator = record.terminal_locator.clone();
    let effective_locator = crate::open_session::effective_terminal_locator(&record);
    let terminal_status = initial_terminal_status(effective_locator.as_ref());
    let open_shell =
        crate::open_session::open_shell_preview(effective_locator.as_ref(), &terminal_status);
    let actions = action_state(&record);
    SessionRow {
        uri: atc_session_uri(&record.id),
        id: record.id,
        task_slug,
        work_unit_id: record.work_unit_id,
        group_key,
        branch: record.branch,
        provider: record.agent_provider,
        provider_session_id: record.agent_session_id.map(|id| id.to_string()),
        transcript_cwd: record
            .agent_transcript_cwd
            .map(|path| path.to_string_lossy().to_string()),
        resume_of_dispatch_id: record.resume_of_dispatch_id,
        status: record.status,
        directive: record.directive.as_str().to_string(),
        resolver: record.resolver,
        session: record.session,
        worktree_path: record.worktree_path.to_string_lossy().to_string(),
        log_file,
        pr_urls: record.pr_urls,
        cost_usd: record.cost_usd,
        num_turns: record.num_turns,
        duration_ms: record.duration_ms,
        terminal_locator,
        terminal_status,
        open_shell,
        dispatched_at: record.dispatched_at,
        updated_at: record.updated_at,
        actions,
    }
}

async fn refresh_terminal_fields(rows: &mut [SessionRow]) {
    for row in rows {
        let effective_locator = effective_locator_for_row(row);
        row.terminal_status =
            crate::open_session::terminal_status_for_locator(effective_locator.as_ref()).await;
        row.open_shell = crate::open_session::open_shell_preview(
            effective_locator.as_ref(),
            &row.terminal_status,
        );
        row.actions.attach = availability_from_open_shell(&row.open_shell);
    }
}

fn initial_terminal_status(locator: Option<&TerminalLocator>) -> TerminalStatus {
    match locator {
        Some(locator) => TerminalStatus::new(TerminalStatusState::Unknown, Some(locator.backend())),
        None => TerminalStatus::unavailable("no terminal locator"),
    }
}

fn effective_locator_for_row(row: &SessionRow) -> Option<TerminalLocator> {
    row.terminal_locator.clone().or_else(|| {
        (row.actions.attach.enabled && !row.session.trim().is_empty()).then(|| {
            TerminalLocator::inferred_tmux(
                row.session.clone(),
                Some(PathBuf::from(row.worktree_path.clone())),
                Utc::now(),
            )
        })
    })
}

fn availability_from_open_shell(open_shell: &OpenSessionPreview) -> ActionAvailability {
    if open_shell.enabled {
        ActionAvailability::enabled()
    } else {
        ActionAvailability::disabled(
            open_shell
                .reason
                .clone()
                .unwrap_or_else(|| "open-session unavailable".to_string()),
        )
    }
}

fn sort_session_rows(rows: &mut [SessionRow], group: SessionGroupBy) {
    if group == SessionGroupBy::None {
        rows.sort_by(session_recency_cmp);
    } else {
        rows.sort_by(|a, b| {
            a.group_key
                .cmp(&b.group_key)
                .then_with(|| session_recency_cmp(a, b))
        });
    }
}

fn session_recency_cmp(a: &SessionRow, b: &SessionRow) -> Ordering {
    b.updated_at
        .cmp(&a.updated_at)
        .then_with(|| b.dispatched_at.cmp(&a.dispatched_at))
        .then_with(|| a.id.cmp(&b.id))
}

fn group_key(
    record: &DispatchRecord,
    work_units_by_id: &HashMap<String, WorkUnit>,
    group: SessionGroupBy,
) -> String {
    match group {
        SessionGroupBy::Task => task_slug_for_record(record, work_units_by_id)
            .unwrap_or("(no task)")
            .to_string(),
        SessionGroupBy::WorkUnit => record
            .work_unit_id
            .clone()
            .unwrap_or_else(|| "(no work unit)".to_string()),
        SessionGroupBy::Branch => record.branch.clone(),
        SessionGroupBy::Provider => record.agent_provider.clone(),
        SessionGroupBy::Status => record.status.as_str().to_string(),
        SessionGroupBy::None => "-".to_string(),
    }
}

fn task_slug_for_record<'a>(
    record: &'a DispatchRecord,
    work_units_by_id: &'a HashMap<String, WorkUnit>,
) -> Option<&'a str> {
    record.task_slug.as_deref().or_else(|| {
        record
            .work_unit_id
            .as_ref()
            .and_then(|id| work_units_by_id.get(id))
            .and_then(|wu| wu.task_slug.as_deref())
    })
}

fn record_matches_filter(
    record: &DispatchRecord,
    work_units_by_id: &HashMap<String, WorkUnit>,
    filter: &SessionFilter,
    now: DateTime<Utc>,
) -> bool {
    if let Some(task) = filter.task.as_deref() {
        if task_slug_for_record(record, work_units_by_id) != Some(task) {
            return false;
        }
    }
    if let Some(work_unit) = filter.work_unit.as_deref() {
        if record.work_unit_id.as_deref() != Some(work_unit) {
            return false;
        }
    }
    if let Some(branch) = filter.branch.as_deref() {
        if record.branch != branch {
            return false;
        }
    }
    if let Some(provider) = filter.provider.as_deref() {
        if record.agent_provider != provider {
            return false;
        }
    }
    if let Some(status) = filter.status {
        if record.status != status {
            return false;
        }
    } else if !filter.all {
        let recent_terminal_cutoff = now - ChronoDuration::hours(DEFAULT_RECENT_TERMINAL_HOURS);
        let recent_terminal =
            record.status.is_terminal() && record.updated_at >= recent_terminal_cutoff;
        if !DEFAULT_STATUSES.contains(&record.status) && !recent_terminal {
            return false;
        }
    }
    if let Some(search) = filter.search.as_deref() {
        let needle = search.to_ascii_lowercase();
        if !search_haystack(record, work_units_by_id).contains(&needle) {
            return false;
        }
    }
    true
}

fn search_haystack(
    record: &DispatchRecord,
    work_units_by_id: &HashMap<String, WorkUnit>,
) -> String {
    let mut parts = vec![
        record.id.clone(),
        record.branch.clone(),
        record.session.clone(),
        record.agent_provider.clone(),
        record.directive.as_str().to_string(),
        record.resolver.clone(),
        record.worktree_path.to_string_lossy().to_string(),
        record.log_file.to_string_lossy().to_string(),
    ];
    if let Some(task) = task_slug_for_record(record, work_units_by_id) {
        parts.push(task.to_string());
    }
    if let Some(work_unit) = &record.work_unit_id {
        parts.push(work_unit.clone());
    }
    if let Some(session_id) = record.agent_session_id {
        parts.push(session_id.to_string());
    }
    if let Some(transcript_cwd) = &record.agent_transcript_cwd {
        parts.push(transcript_cwd.to_string_lossy().to_string());
    }
    if let Some(resume_of) = &record.resume_of_dispatch_id {
        parts.push(resume_of.clone());
    }
    parts.extend(record.pr_urls.clone());
    parts.join("\n").to_ascii_lowercase()
}

pub fn action_state(record: &DispatchRecord) -> SessionActionState {
    let caps = record.agent_capabilities.unwrap_or_default();
    let log_readable = readable_file(&record.log_file);
    let has_tmux_session = !record.session.trim().is_empty();
    let resume_available = caps.supports_resume_by_session_id
        && record.agent_session_id.is_some()
        && record.agent_transcript_cwd.is_some()
        && record.status.is_terminal();

    SessionActionState {
        info: ActionAvailability::enabled(),
        logs: if log_readable {
            ActionAvailability::enabled()
        } else {
            ActionAvailability::disabled("log file missing or unreadable")
        },
        follow_logs: if log_readable {
            ActionAvailability::enabled()
        } else {
            ActionAvailability::disabled("log file missing or unreadable")
        },
        attach: if caps.supports_tmux_attach && has_tmux_session {
            ActionAvailability::enabled()
        } else if !caps.supports_tmux_attach {
            ActionAvailability::disabled("provider does not advertise tmux attach")
        } else {
            ActionAvailability::disabled("missing ATC tmux session")
        },
        redirect: if record.status == Status::Running
            && caps.supports_tmux_redirect
            && has_tmux_session
        {
            ActionAvailability::enabled()
        } else if record.status != Status::Running {
            ActionAvailability::disabled("dispatch is not running")
        } else if !caps.supports_tmux_redirect {
            ActionAvailability::disabled("provider does not advertise tmux redirect")
        } else {
            ActionAvailability::disabled("missing ATC tmux session")
        },
        resume: if resume_available {
            ActionAvailability::enabled()
        } else if !record.status.is_terminal() {
            ActionAvailability::disabled("provider session is still active")
        } else if !caps.supports_resume_by_session_id {
            ActionAvailability::disabled("provider does not advertise resume by session id")
        } else if record.agent_session_id.is_none() {
            ActionAvailability::disabled("missing provider session id")
        } else {
            ActionAvailability::disabled("missing provider transcript cwd")
        },
        stop: if !record.status.is_terminal() {
            ActionAvailability::enabled()
        } else {
            ActionAvailability::disabled("dispatch is already terminal")
        },
        cleanup: ActionAvailability::enabled(),
    }
}

fn readable_file(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path.metadata().map(|m| m.is_file()).unwrap_or(false)
        && std::fs::File::open(path).is_ok()
}

fn path_to_nonempty_string(path: &Path) -> Option<String> {
    if path.as_os_str().is_empty() {
        None
    } else {
        Some(path.to_string_lossy().to_string())
    }
}

pub fn render_once(snapshot: &SessionSnapshot, now: DateTime<Utc>) -> String {
    use comfy_table::{presets::NOTHING, ContentArrangement, Table as ComfyTable};

    let mut out = String::new();
    out.push_str(&format!(
        "ATC Sessions  {}  group: {}\n\n",
        snapshot.summary.human(),
        snapshot.group.label()
    ));

    if snapshot.rows.is_empty() {
        out.push_str("No sessions found.\n");
        return out;
    }

    let mut table = ComfyTable::new();
    table.load_preset(NOTHING);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec![
        "group", "task", "provider", "status", "age", "cost", "session", "actions",
    ]);

    for row in &snapshot.rows {
        table.add_row(vec![
            display_text(&row.group_key),
            display_text(row.display_task()),
            display_text(&row.provider),
            row.status.as_str().to_string(),
            format_age(now, row.updated_at),
            format_cost(row.cost_usd),
            truncate_middle(&display_text(&row.session), 40),
            enabled_action_labels(&row.actions).join(","),
        ]);
    }

    out.push_str(&table.to_string());
    out.push('\n');
    out
}

fn enabled_action_labels(actions: &SessionActionState) -> Vec<&'static str> {
    let mut labels = vec!["info"];
    if actions.logs.enabled {
        labels.push("logs");
    }
    if actions.follow_logs.enabled {
        labels.push("follow");
    }
    if actions.attach.enabled {
        labels.push("attach");
    }
    if actions.redirect.enabled {
        labels.push("redirect");
    }
    if actions.resume.enabled {
        labels.push("resume");
    }
    if actions.stop.enabled {
        labels.push("stop");
    }
    if actions.cleanup.enabled {
        labels.push("cleanup");
    }
    labels
}

fn format_cost(cost: Option<f64>) -> String {
    cost.map(|c| format!("${c:.2}"))
        .unwrap_or_else(|| "-".to_string())
}

fn format_age(now: DateTime<Utc>, since: DateTime<Utc>) -> String {
    let delta = now.signed_duration_since(since);
    let delta = if delta < ChronoDuration::zero() {
        ChronoDuration::zero()
    } else {
        delta
    };
    let secs = delta.num_seconds();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn truncate_middle(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    if max_chars <= 1 {
        return "\u{2026}".to_string();
    }
    let left = (max_chars - 1) / 2;
    let right = max_chars - 1 - left;
    let head: String = value.chars().take(left).collect();
    let tail: String = value
        .chars()
        .rev()
        .take(right)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}\u{2026}{tail}")
}

#[derive(Debug, Clone)]
pub struct SessionsApp {
    pub snapshot: SessionSnapshot,
    pub selected: usize,
    pub show_detail: bool,
    pub message: Option<String>,
    pub filter_input: Option<FilterEdit>,
    pub pending_confirmation: Option<PendingAction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterEdit {
    field: FilterField,
    value: String,
}

impl FilterEdit {
    fn new(field: FilterField, value: Option<String>) -> Self {
        Self {
            field,
            value: value.unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterField {
    Search,
    Task,
    WorkUnit,
    Branch,
    Provider,
    Status,
}

impl FilterField {
    fn label(self) -> &'static str {
        match self {
            FilterField::Search => "search",
            FilterField::Task => "task",
            FilterField::WorkUnit => "work-unit",
            FilterField::Branch => "branch",
            FilterField::Provider => "provider",
            FilterField::Status => "status",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingAction {
    Stop,
    Cleanup,
}

impl PendingAction {
    fn label(self) -> &'static str {
        match self {
            PendingAction::Stop => "stop",
            PendingAction::Cleanup => "cleanup",
        }
    }
}

impl SessionsApp {
    pub fn new(snapshot: SessionSnapshot) -> Self {
        Self {
            snapshot,
            selected: 0,
            show_detail: false,
            message: None,
            filter_input: None,
            pending_confirmation: None,
        }
    }

    fn selected_row(&self) -> Option<&SessionRow> {
        self.snapshot.rows.get(self.selected)
    }

    fn selected_id(&self) -> Option<String> {
        self.selected_row().map(|row| row.id.clone())
    }

    fn clamp_selection(&mut self) {
        if self.snapshot.rows.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.snapshot.rows.len() {
            self.selected = self.snapshot.rows.len() - 1;
        }
    }

    fn move_down(&mut self) {
        if !self.snapshot.rows.is_empty() {
            self.selected = (self.selected + 1).min(self.snapshot.rows.len() - 1);
        }
    }

    fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn set_snapshot(&mut self, snapshot: SessionSnapshot) {
        let previous_id = self.selected_id();
        self.snapshot = snapshot;
        if let Some(previous_id) = previous_id {
            if let Some(index) = self
                .snapshot
                .rows
                .iter()
                .position(|row| row.id == previous_id)
            {
                self.selected = index;
            }
        }
        self.clamp_selection();
    }
}

async fn run_tui(
    config: &AtcConfig,
    registry: Arc<dyn Registry>,
    executor: Arc<dyn AgentExecutor>,
    filter: SessionFilter,
    group: SessionGroupBy,
    poll_interval: Duration,
    initial_snapshot: SessionSnapshot,
) -> Result<()> {
    let mut terminal = setup_tui_terminal()?;

    let mut app = SessionsApp::new(initial_snapshot);
    let mut last_poll = Instant::now();
    let mut ctx = TuiContext {
        config,
        registry,
        executor,
        filter,
        group,
    };

    let loop_result: Result<()> = async {
        loop {
            terminal.draw(|frame| render_app(frame, &app))?;

            let timeout = poll_interval
                .checked_sub(last_poll.elapsed())
                .unwrap_or_else(|| Duration::from_millis(0));
            if event::poll(timeout)? {
                if let Event::Key(key) = event::read()? {
                    if handle_key(key, &mut app, &mut ctx, &mut terminal).await? {
                        break;
                    }
                }
            }

            if last_poll.elapsed() >= poll_interval {
                match load_snapshot(ctx.registry.as_ref(), &ctx.filter, ctx.group).await {
                    Ok(snapshot) => app.set_snapshot(snapshot),
                    Err(e) => {
                        warn!(error = %e, "atc sessions refresh failed");
                        app.message = Some(format!("refresh failed: {e}"));
                    }
                }
                last_poll = Instant::now();
            }
        }
        Ok(())
    }
    .await;

    let restore_result = leave_tui(&mut terminal);
    loop_result?;
    restore_result?;
    Ok(())
}

struct TuiContext<'a> {
    config: &'a AtcConfig,
    registry: Arc<dyn Registry>,
    executor: Arc<dyn AgentExecutor>,
    filter: SessionFilter,
    group: SessionGroupBy,
}

async fn handle_key(
    key: KeyEvent,
    app: &mut SessionsApp,
    ctx: &mut TuiContext<'_>,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<bool> {
    if let Some(edit) = app.filter_input.as_mut() {
        match key.code {
            KeyCode::Esc => app.filter_input = None,
            KeyCode::Enter => {
                let edit = app.filter_input.take().expect("filter edit is active");
                match apply_filter_edit(&mut ctx.filter, edit) {
                    Ok(message) => {
                        let snapshot =
                            load_snapshot(ctx.registry.as_ref(), &ctx.filter, ctx.group).await?;
                        app.set_snapshot(snapshot);
                        app.message = Some(message);
                    }
                    Err(e) => {
                        app.message = Some(format!("filter rejected: {e}"));
                    }
                }
            }
            KeyCode::Backspace => {
                edit.value.pop();
            }
            KeyCode::Char(c) => edit.value.push(c),
            _ => {}
        }
        return Ok(false);
    }

    if let Some(pending) = app.pending_confirmation {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.pending_confirmation = None;
                if let Some(row) = app.selected_row().cloned() {
                    let message = match pending {
                        PendingAction::Stop => {
                            let registry = ctx.registry.clone();
                            run_terminal_action(terminal, || async {
                                crate::stop::run_stop(ctx.config, registry.as_ref(), &row.id)
                                    .await?;
                                Ok(format!("stopped {}", row.id))
                            })
                            .await?
                        }
                        PendingAction::Cleanup => {
                            let registry = ctx.registry.clone();
                            run_terminal_action(terminal, || async {
                                crate::cleanup::run_cleanup(
                                    ctx.config,
                                    registry.as_ref(),
                                    Some(&row.id),
                                    false,
                                )
                                .await?;
                                Ok(format!("cleaned {}", row.id))
                            })
                            .await?
                        }
                    };
                    app.message = Some(message);
                    if let Ok(snapshot) =
                        load_snapshot(ctx.registry.as_ref(), &ctx.filter, ctx.group).await
                    {
                        app.set_snapshot(snapshot);
                    }
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                app.message = Some(format!("{} cancelled", pending.label()));
                app.pending_confirmation = None;
            }
            _ => {}
        }
        return Ok(false);
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
        KeyCode::Down | KeyCode::Char('j') => app.move_down(),
        KeyCode::Up | KeyCode::Char('k') => app.move_up(),
        KeyCode::Enter | KeyCode::Char('i') => app.show_detail = !app.show_detail,
        KeyCode::Char('/') => {
            app.filter_input = Some(FilterEdit::new(
                FilterField::Search,
                ctx.filter.search.clone(),
            ))
        }
        KeyCode::Char('t') => {
            app.filter_input = Some(FilterEdit::new(FilterField::Task, ctx.filter.task.clone()))
        }
        KeyCode::Char('w') => {
            app.filter_input = Some(FilterEdit::new(
                FilterField::WorkUnit,
                ctx.filter.work_unit.clone(),
            ))
        }
        KeyCode::Char('b') => {
            app.filter_input = Some(FilterEdit::new(
                FilterField::Branch,
                ctx.filter.branch.clone(),
            ))
        }
        KeyCode::Char('p') => {
            app.filter_input = Some(FilterEdit::new(
                FilterField::Provider,
                ctx.filter.provider.clone(),
            ))
        }
        KeyCode::Char('S') => {
            app.filter_input = Some(FilterEdit::new(
                FilterField::Status,
                ctx.filter.status.map(|status| status.as_str().to_string()),
            ))
        }
        KeyCode::Char('A') => {
            ctx.filter.all = !ctx.filter.all;
            let snapshot = load_snapshot(ctx.registry.as_ref(), &ctx.filter, ctx.group).await?;
            app.set_snapshot(snapshot);
            app.message = Some(format!(
                "all statuses {}",
                if ctx.filter.all { "included" } else { "hidden" }
            ));
        }
        KeyCode::Char('g') => {
            ctx.group = ctx.group.next();
            let snapshot = load_snapshot(ctx.registry.as_ref(), &ctx.filter, ctx.group).await?;
            app.set_snapshot(snapshot);
            app.message = Some(format!("grouped by {}", ctx.group.label()));
        }
        KeyCode::Char('r') => {
            let snapshot = load_snapshot(ctx.registry.as_ref(), &ctx.filter, ctx.group).await?;
            app.set_snapshot(snapshot);
            app.message = Some("refreshed".to_string());
        }
        KeyCode::Char('l') => {
            if let Some(row) = app.selected_row().cloned() {
                if row.actions.logs.enabled {
                    let registry = ctx.registry.clone();
                    app.message = Some(
                        run_terminal_action(terminal, || async {
                            crate::logs::run_logs(registry, ctx.config, &row.id, false).await?;
                            Ok(format!("viewed logs for {}", row.id))
                        })
                        .await?,
                    );
                } else {
                    app.message = row.actions.logs.reason.clone();
                }
            }
        }
        KeyCode::Char('f') => {
            if let Some(row) = app.selected_row().cloned() {
                if row.actions.follow_logs.enabled {
                    let registry = ctx.registry.clone();
                    app.message = Some(
                        run_terminal_action(terminal, || async {
                            crate::logs::run_logs(registry, ctx.config, &row.id, true).await?;
                            Ok(format!("followed logs for {}", row.id))
                        })
                        .await?,
                    );
                } else {
                    app.message = row.actions.follow_logs.reason.clone();
                }
            }
        }
        KeyCode::Char('a') => {
            if let Some(row) = app.selected_row().cloned() {
                if row.actions.attach.enabled {
                    let registry = ctx.registry.clone();
                    let target = row.uri.clone();
                    app.message = Some(
                        run_terminal_action(terminal, || async {
                            crate::open_session::run_open_session_action(registry.as_ref(), &target)
                                .await
                        })
                        .await?,
                    );
                } else {
                    app.message = row.actions.attach.reason.clone();
                }
            }
        }
        KeyCode::Char('d') => {
            if let Some(row) = app.selected_row().cloned() {
                if row.actions.redirect.enabled {
                    let registry = ctx.registry.clone();
                    app.message = Some(
                        run_terminal_action(terminal, || async {
                            let Some(message) = prompt_line("Message to redirect")? else {
                                return Ok("redirect cancelled".to_string());
                            };
                            crate::redirect::run_redirect(registry.as_ref(), &row.id, &message)
                                .await?;
                            Ok(format!("redirected message to {}", row.id))
                        })
                        .await?,
                    );
                } else {
                    app.message = row.actions.redirect.reason.clone();
                }
            }
        }
        KeyCode::Char('R') => {
            if let Some(row) = app.selected_row().cloned() {
                if row.actions.resume.enabled {
                    let registry = ctx.registry.clone();
                    let executor = ctx.executor.clone();
                    app.message = Some(
                        run_terminal_action(terminal, || async {
                            let Some(message) = prompt_line("Resume prompt")? else {
                                return Ok("resume cancelled".to_string());
                            };
                            let outcome = run_resume(
                                ctx.config,
                                registry.as_ref(),
                                executor.as_ref(),
                                &row.id,
                                &message,
                            )
                            .await?;
                            Ok(format!("resumed {} as {}", row.id, outcome.id))
                        })
                        .await?,
                    );
                    if let Ok(snapshot) =
                        load_snapshot(ctx.registry.as_ref(), &ctx.filter, ctx.group).await
                    {
                        app.set_snapshot(snapshot);
                    }
                } else {
                    app.message = row.actions.resume.reason.clone();
                }
            }
        }
        KeyCode::Char('s') => {
            if let Some(row) = app.selected_row() {
                if row.actions.stop.enabled {
                    app.pending_confirmation = Some(PendingAction::Stop);
                } else {
                    app.message = row.actions.stop.reason.clone();
                }
            }
        }
        KeyCode::Char('c') => {
            if let Some(row) = app.selected_row() {
                if row.actions.cleanup.enabled {
                    app.pending_confirmation = Some(PendingAction::Cleanup);
                } else {
                    app.message = row.actions.cleanup.reason.clone();
                }
            }
        }
        _ => {}
    }
    Ok(false)
}

fn apply_filter_edit(filter: &mut SessionFilter, edit: FilterEdit) -> Result<String> {
    let value = optional_filter_value(edit.value);
    match edit.field {
        FilterField::Search => filter.search = value,
        FilterField::Task => filter.task = value,
        FilterField::WorkUnit => filter.work_unit = value,
        FilterField::Branch => filter.branch = value,
        FilterField::Provider => filter.provider = value,
        FilterField::Status => {
            filter.status = value.as_deref().map(str::parse::<Status>).transpose()?;
        }
    }
    let label = edit.field.label();
    let value = match edit.field {
        FilterField::Search => filter.search.as_deref(),
        FilterField::Task => filter.task.as_deref(),
        FilterField::WorkUnit => filter.work_unit.as_deref(),
        FilterField::Branch => filter.branch.as_deref(),
        FilterField::Provider => filter.provider.as_deref(),
        FilterField::Status => filter.status.as_ref().map(Status::as_str),
    };
    Ok(match value {
        Some(value) => format!("{label} filter set to {value}"),
        None => format!("{label} filter cleared"),
    })
}

fn optional_filter_value(value: String) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

async fn run_resume(
    config: &AtcConfig,
    registry: &dyn Registry,
    executor: &dyn AgentExecutor,
    source_id: &str,
    message: &str,
) -> Result<atc_core::types::DispatchOutcome> {
    let opts = RunOpts {
        input: message.to_string(),
        directive: None,
        params: HashMap::new(),
        pr_url: None,
        repos: vec![],
        inline: false,
        force: false,
        dry_run: false,
        directives: None,
        no_worktree: false,
        max_budget_usd: None,
        max_turns: None,
        resume: Some(source_id.to_string()),
        retries: 0,
        list: false,
        ephemeral: false,
        timeout: None,
        json: false,
    };
    let pipeline = crate::pipeline::DispatchPipeline {
        resolvers: crate::resolvers::build_resolvers(config),
        config,
        registry,
        executor,
    };
    pipeline.execute(message, &opts).await
}

async fn run_terminal_action<F, Fut>(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    action: F,
) -> Result<String>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let mut runtime = CrosstermTerminalActionRuntime { terminal };
    run_terminal_action_with_runtime(&mut runtime, action).await
}

trait TerminalActionRuntime {
    fn leave_tui(&mut self) -> Result<()>;
    fn prompt_for_return(&mut self, message: &str);
    fn enter_tui(&mut self) -> Result<()>;
}

struct CrosstermTerminalActionRuntime<'a> {
    terminal: &'a mut Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalActionRuntime for CrosstermTerminalActionRuntime<'_> {
    fn leave_tui(&mut self) -> Result<()> {
        leave_tui(self.terminal)
    }

    fn prompt_for_return(&mut self, message: &str) {
        prompt_for_terminal_action_return(message);
    }

    fn enter_tui(&mut self) -> Result<()> {
        enter_tui(self.terminal)
    }
}

async fn run_terminal_action_with_runtime<R, F, Fut>(runtime: &mut R, action: F) -> Result<String>
where
    R: TerminalActionRuntime + ?Sized,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<String>>,
{
    runtime
        .leave_tui()
        .context("failed to restore terminal before running action")?;
    let result = action().await;
    let message = match result {
        Ok(message) => message,
        Err(e) => format!("action failed: {e}"),
    };
    runtime.prompt_for_return(&message);
    runtime
        .enter_tui()
        .context("failed to restore TUI after running action")?;
    Ok(message)
}

fn prompt_for_terminal_action_return(message: &str) {
    let display_message = display_text(message);
    eprintln!();
    eprintln!("{display_message}");
    eprint!("Press Enter to return to atc sessions...");
    let _ = io::stderr().flush();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
}

fn prompt_line(label: &str) -> Result<Option<String>> {
    print!("{label}: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let value = line.trim().to_string();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

#[derive(Debug, Default)]
struct TuiSetupCleanup {
    raw_mode_enabled: bool,
    alternate_screen_entered: bool,
}

impl TuiSetupCleanup {
    fn active() -> Self {
        Self {
            raw_mode_enabled: true,
            alternate_screen_entered: true,
        }
    }

    fn mark_raw_mode_enabled(&mut self) {
        self.raw_mode_enabled = true;
    }

    fn mark_alternate_screen_entered(&mut self) {
        self.alternate_screen_entered = true;
    }

    fn disarm(&mut self) {
        self.raw_mode_enabled = false;
        self.alternate_screen_entered = false;
    }

    fn cleanup_best_effort_stdout(&mut self) {
        let _ = self.cleanup_with_ops(
            || {
                let mut stdout = io::stdout();
                execute!(stdout, LeaveAlternateScreen).context("failed to leave alternate screen")
            },
            || disable_raw_mode().context("failed to disable raw mode"),
        );
    }

    fn cleanup_with_backend<W: Write>(&mut self, writer: &mut W) -> Result<()> {
        self.cleanup_with_ops(
            || execute!(writer, LeaveAlternateScreen).context("failed to leave alternate screen"),
            || disable_raw_mode().context("failed to disable raw mode"),
        )
    }

    fn cleanup_with_ops<LeaveAlternate, DisableRaw>(
        &mut self,
        mut leave_alternate: LeaveAlternate,
        mut disable_raw: DisableRaw,
    ) -> Result<()>
    where
        LeaveAlternate: FnMut() -> Result<()>,
        DisableRaw: FnMut() -> Result<()>,
    {
        let mut first_error = None;

        if self.alternate_screen_entered {
            if let Err(e) = leave_alternate() {
                first_error = Some(e);
            }
            self.alternate_screen_entered = false;
        }

        if self.raw_mode_enabled {
            if let Err(e) = disable_raw() {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
            self.raw_mode_enabled = false;
        }

        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

fn setup_tui_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    let mut cleanup = TuiSetupCleanup::default();

    enable_raw_mode().context("failed to enable raw mode")?;
    cleanup.mark_raw_mode_enabled();

    let mut stdout = io::stdout();
    if let Err(e) =
        execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")
    {
        cleanup.cleanup_best_effort_stdout();
        return Err(e);
    }
    cleanup.mark_alternate_screen_entered();

    let backend = CrosstermBackend::new(stdout);
    match Terminal::new(backend).context("failed to initialize terminal") {
        Ok(terminal) => {
            cleanup.disarm();
            Ok(terminal)
        }
        Err(e) => {
            cleanup.cleanup_best_effort_stdout();
            Err(e)
        }
    }
}

fn leave_tui(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    TuiSetupCleanup::active().cleanup_with_backend(terminal.backend_mut())?;
    terminal.show_cursor()?;
    Ok(())
}

fn enter_tui(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let mut cleanup = TuiSetupCleanup::default();

    enable_raw_mode().context("failed to enable raw mode")?;
    cleanup.mark_raw_mode_enabled();

    if let Err(e) = execute!(terminal.backend_mut(), EnterAlternateScreen)
        .context("failed to enter alternate screen")
    {
        cleanup.cleanup_best_effort_stdout();
        return Err(e);
    }
    cleanup.mark_alternate_screen_entered();

    if let Err(e) = terminal.clear().context("failed to clear terminal") {
        let _ = cleanup.cleanup_with_backend(terminal.backend_mut());
        return Err(e);
    }

    cleanup.disarm();
    Ok(())
}

fn parse_poll_interval(value: Option<&str>) -> Result<Duration> {
    let interval = match value {
        Some(value) => humantime::parse_duration(value)
            .with_context(|| format!("invalid --poll-interval value '{value}'"))?,
        None => DEFAULT_POLL_INTERVAL,
    };
    if interval < MIN_POLL_INTERVAL {
        bail!(
            "--poll-interval must be at least {}",
            humantime::format_duration(MIN_POLL_INTERVAL)
        );
    }
    Ok(interval)
}

pub fn render_app(frame: &mut Frame<'_>, app: &SessionsApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(if app.show_detail { 14 } else { 3 }),
        ])
        .split(frame.area());

    render_header(frame, app, chunks[0]);
    render_rows(frame, app, chunks[1]);
    render_footer(frame, app, chunks[2]);

    if let Some(pending) = app.pending_confirmation {
        render_confirmation(frame, pending);
    }
}

fn render_header(frame: &mut Frame<'_>, app: &SessionsApp, area: Rect) {
    let filter = app
        .filter_input
        .as_ref()
        .map(|edit| {
            format!(
                "editing {}: {}",
                edit.field.label(),
                display_text(&edit.value)
            )
        })
        .unwrap_or_else(|| {
            "filters: / search  t task  w work-unit  b branch  p provider  S status  A all  g group"
                .to_string()
        });
    let title = format!(
        " ATC Sessions  {}  group: {}  {} ",
        app.snapshot.summary.human(),
        app.snapshot.group.label(),
        filter
    );
    let paragraph = Paragraph::new(title)
        .block(Block::default().borders(Borders::ALL))
        .style(Style::default().fg(Color::Cyan));
    frame.render_widget(paragraph, area);
}

fn render_rows(frame: &mut Frame<'_>, app: &SessionsApp, area: Rect) {
    if app.snapshot.rows.is_empty() {
        let paragraph = Paragraph::new("No sessions found.")
            .block(Block::default().borders(Borders::ALL).title(" sessions "))
            .style(Style::default().fg(Color::Gray));
        frame.render_widget(paragraph, area);
        return;
    }

    let grouped = app.snapshot.group != SessionGroupBy::None;
    let mut header_cells = Vec::new();
    if grouped {
        header_cells.push(Cell::from("group"));
    }
    header_cells.extend([
        Cell::from("task/work-unit"),
        Cell::from("provider"),
        Cell::from("status"),
        Cell::from("cost"),
        Cell::from("session"),
    ]);

    let header = Row::new(header_cells).style(
        Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::BOLD),
    );

    let rows = app.snapshot.rows.iter().map(|row| {
        let task = format!(
            "{}  {}",
            display_text(row.display_task()),
            display_text(row.display_work_unit())
        );
        let mut cells = Vec::new();
        if grouped {
            cells.push(Cell::from(truncate_middle(
                &display_text(&row.group_key),
                22,
            )));
        }
        cells.extend([
            Cell::from(task),
            Cell::from(display_text(&row.provider)),
            Cell::from(row.status.as_str().to_string()),
            Cell::from(format_cost(row.cost_usd)),
            Cell::from(truncate_middle(&display_text(&row.session), 42)),
        ]);
        Row::new(cells)
    });

    let widths = if grouped {
        vec![
            Constraint::Length(24),
            Constraint::Percentage(30),
            Constraint::Length(12),
            Constraint::Length(14),
            Constraint::Length(10),
            Constraint::Min(20),
        ]
    } else {
        vec![
            Constraint::Percentage(38),
            Constraint::Length(12),
            Constraint::Length(14),
            Constraint::Length(10),
            Constraint::Min(20),
        ]
    };

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" sessions "))
        .row_highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );

    let mut state = TableState::default();
    if !app.snapshot.rows.is_empty() {
        state.select(Some(app.selected));
    }
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_footer(frame: &mut Frame<'_>, app: &SessionsApp, area: Rect) {
    let text = if app.show_detail {
        app.selected_row()
            .map(detail_text)
            .unwrap_or_else(|| "No session selected.".to_string())
    } else {
        let mut lines = vec![
            "Enter details  l logs  f follow  a attach  R resume  d redirect  s stop  c cleanup  / search  t/w/b/p/S filters  A all  g group  r refresh  q quit".to_string(),
        ];
        if let Some(message) = &app.message {
            lines.push(display_text(message));
        }
        lines.join("\n")
    };
    let paragraph = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(" details "))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn detail_text(row: &SessionRow) -> String {
    let actions = [
        ("logs", &row.actions.logs),
        ("follow", &row.actions.follow_logs),
        ("attach", &row.actions.attach),
        ("redirect", &row.actions.redirect),
        ("resume", &row.actions.resume),
        ("stop", &row.actions.stop),
        ("cleanup", &row.actions.cleanup),
    ];
    let action_text = actions
        .iter()
        .map(|(name, state)| {
            if state.enabled {
                (*name).to_string()
            } else {
                format!(
                    "{}({})",
                    name,
                    state.reason.as_deref().unwrap_or("disabled")
                )
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "id: {}\ntask: {}\nwork_unit: {}\nprovider: {}  provider_session: {}\nstatus: {}  directive: {}  resolver: {}\nbranch: {}\nworktree: {}\nlog: {}\nprs: {}\nresume_of: {}\nactions: {}",
        display_text(&row.id),
        display_text(row.display_task()),
        display_text(row.display_work_unit()),
        display_text(&row.provider),
        display_text(row.display_provider_session()),
        row.status,
        display_text(&row.directive),
        display_text(&row.resolver),
        display_text(&row.branch),
        display_text(&row.worktree_path),
        display_text(row.log_file.as_deref().unwrap_or("-")),
        display_text(&format_pr_list(&row.pr_urls)),
        display_text(row.resume_of_dispatch_id.as_deref().unwrap_or("-")),
        action_text
    )
}

fn render_confirmation(frame: &mut Frame<'_>, pending: PendingAction) {
    let area = centered_rect(60, 20, frame.area());
    let text = vec![
        Line::from(Span::styled(
            format!("Confirm {} selected dispatch?", pending.label()),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Press y to confirm, n or Esc to cancel."),
    ];
    let paragraph = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(" confirm "))
        .style(Style::default().bg(Color::Black));
    frame.render_widget(paragraph, area);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use atc_core::registry::{Registry, StatusFilter};
    use atc_core::types::{
        claude_agent_capabilities, AgentCapabilities, AgentSessionId, Directive, HealthChecks,
        Status, WorkUnitStatus, CLAUDE_AGENT_PROVIDER,
    };
    use chrono::TimeZone;
    use ratatui::backend::TestBackend;
    use std::cell::{Cell, RefCell};
    use std::path::PathBuf;

    use crate::test_support::MockRegistry;

    fn record(id: &str, status: Status) -> DispatchRecord {
        DispatchRecord {
            id: id.to_string(),
            task_slug: Some("tasks/harmony-794".to_string()),
            branch: "tasks-harmony-794-atc-sessions".to_string(),
            worktree_path: PathBuf::from("/tmp/worktree"),
            session: format!("session-{id}"),
            log_file: PathBuf::from("/tmp/missing-log.jsonl"),
            status,
            directive: Directive::Implement,
            retries: 0,
            resolver: "task".to_string(),
            pr_urls: vec!["https://github.com/gitkb/atc/pull/99".to_string()],
            no_worktree: false,
            original_input: Some("tasks/harmony-794".to_string()),
            checks: HealthChecks::default(),
            kb_root: None,
            cost_usd: Some(1.25),
            num_turns: Some(7),
            duration_ms: Some(65_000),
            artifacts: None,
            work_unit_id: Some("wu-1".to_string()),
            agent_provider: CLAUDE_AGENT_PROVIDER.to_string(),
            agent_session_id: Some(
                AgentSessionId::parse_str("00000000-0000-4000-8000-000000000794").unwrap(),
            ),
            agent_transcript_cwd: Some(PathBuf::from("/tmp/worktree")),
            resume_of_dispatch_id: None,
            agent_capabilities: Some(claude_agent_capabilities()),
            terminal_locator: None,
            dispatched_at: Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 6, 3, 12, 5, 0).unwrap(),
        }
    }

    fn work_unit() -> WorkUnit {
        WorkUnit {
            id: "wu-1".to_string(),
            task_slug: Some("tasks/harmony-794".to_string()),
            branch: Some("tasks-harmony-794-atc-sessions".to_string()),
            repos: vec!["open-source/atc".to_string()],
            pr_urls: vec![],
            status: WorkUnitStatus::Active,
            created_at: Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 6, 3, 12, 5, 0).unwrap(),
        }
    }

    #[test]
    fn snapshot_defaults_to_active_and_recent_terminal_statuses() {
        let filter = SessionFilter::default();
        let now = Utc.with_ymd_and_hms(2026, 6, 3, 13, 0, 0).unwrap();
        let running = record("running", Status::Running);
        let recent_done = record("recent-done", Status::Done);
        let mut old_done = record("old-done", Status::Done);
        old_done.updated_at = now - ChronoDuration::hours(DEFAULT_RECENT_TERMINAL_HOURS + 1);

        let snapshot = build_snapshot_at(
            vec![running, recent_done, old_done],
            vec![work_unit()],
            &filter,
            SessionGroupBy::Task,
            now,
        );
        let ids: Vec<&str> = snapshot.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, vec!["recent-done", "running"]);
        assert_eq!(snapshot.summary.running, 1);
        assert_eq!(snapshot.summary.done, 1);
    }

    #[test]
    fn snapshot_status_filter_uses_narrow_query_for_explicit_status() {
        let filter = SessionFilter {
            status: Some(Status::Failed),
            ..SessionFilter::default()
        };
        let now = Utc.with_ymd_and_hms(2026, 6, 3, 13, 0, 0).unwrap();
        match snapshot_status_filter(&filter, now) {
            StatusFilter::One(Status::Failed) => {}
            other => panic!("expected failed status filter, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_status_filter_bounds_default_recent_terminal_query() {
        let filter = SessionFilter::default();
        let now = Utc.with_ymd_and_hms(2026, 6, 3, 13, 0, 0).unwrap();
        match snapshot_status_filter(&filter, now) {
            StatusFilter::AnyOrUpdatedSince {
                statuses,
                updated_since,
            } => {
                assert_eq!(statuses, DEFAULT_STATUSES);
                assert_eq!(
                    updated_since,
                    now - ChronoDuration::hours(DEFAULT_RECENT_TERMINAL_HOURS)
                );
            }
            other => panic!("expected bounded default status filter, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_status_filter_all_uses_full_query() {
        let filter = SessionFilter {
            all: true,
            ..SessionFilter::default()
        };
        let now = Utc.with_ymd_and_hms(2026, 6, 3, 13, 0, 0).unwrap();
        match snapshot_status_filter(&filter, now) {
            StatusFilter::All => {}
            other => panic!("expected all status filter, got {other:?}"),
        }
    }

    #[test]
    fn work_unit_ids_for_records_deduplicates_in_first_seen_order() {
        let mut first = record("first", Status::Running);
        first.work_unit_id = Some("wu-shared".to_string());
        let mut second = record("second", Status::Running);
        second.work_unit_id = Some("wu-other".to_string());
        let mut third = record("third", Status::Running);
        third.work_unit_id = Some("wu-shared".to_string());
        let mut no_work_unit = record("none", Status::Running);
        no_work_unit.work_unit_id = None;

        assert_eq!(
            work_unit_ids_for_records(&[first, second, third, no_work_unit]),
            vec!["wu-shared".to_string(), "wu-other".to_string()]
        );
    }

    #[tokio::test]
    async fn load_snapshot_reads_updated_registry_state_on_each_call() {
        let registry = MockRegistry::new(vec![record("reactive", Status::Running)]);
        let filter = SessionFilter::default();

        let first = load_snapshot(&registry, &filter, SessionGroupBy::None)
            .await
            .unwrap();
        assert_eq!(first.rows[0].status, Status::Running);

        registry
            .update_status("reactive", Status::Done)
            .await
            .unwrap();

        let second = load_snapshot(&registry, &filter, SessionGroupBy::None)
            .await
            .unwrap();
        assert_eq!(second.rows[0].status, Status::Done);
    }

    #[test]
    fn snapshot_allows_all_and_search_filters() {
        let filter = SessionFilter {
            all: true,
            search: Some("pull/99".to_string()),
            ..SessionFilter::default()
        };
        let snapshot = build_snapshot(
            vec![
                record("done", Status::Done),
                record("failed", Status::Failed),
            ],
            vec![work_unit()],
            &filter,
            SessionGroupBy::Provider,
        );
        assert_eq!(snapshot.rows.len(), 2);
        assert!(snapshot.rows.iter().all(|row| row.group_key == "claude"));
    }

    #[test]
    fn snapshot_filters_and_searches_work_unit_task_fallback() {
        let mut fallback = record("fallback", Status::Running);
        fallback.task_slug = None;
        fallback.work_unit_id = Some("wu-1".to_string());

        let task_filter = SessionFilter {
            task: Some("tasks/harmony-794".to_string()),
            ..SessionFilter::default()
        };
        let snapshot = build_snapshot(
            vec![fallback.clone()],
            vec![work_unit()],
            &task_filter,
            SessionGroupBy::Task,
        );
        assert_eq!(snapshot.rows.len(), 1);
        assert_eq!(snapshot.rows[0].id, "fallback");
        assert_eq!(
            snapshot.rows[0].task_slug.as_deref(),
            Some("tasks/harmony-794")
        );
        assert_eq!(snapshot.rows[0].group_key, "tasks/harmony-794");

        let search_filter = SessionFilter {
            search: Some("harmony-794".to_string()),
            ..SessionFilter::default()
        };
        let snapshot = build_snapshot(
            vec![fallback],
            vec![work_unit()],
            &search_filter,
            SessionGroupBy::None,
        );
        assert_eq!(snapshot.rows.len(), 1);
        assert_eq!(snapshot.rows[0].id, "fallback");
    }

    #[test]
    fn snapshot_grouping_sorts_by_group_then_recency() {
        let filter = SessionFilter {
            all: true,
            ..SessionFilter::default()
        };
        let mut zeta_new = record("zeta-new", Status::Running);
        zeta_new.agent_provider = "zeta".to_string();
        zeta_new.updated_at = Utc.with_ymd_and_hms(2026, 6, 3, 12, 30, 0).unwrap();
        zeta_new.dispatched_at = zeta_new.updated_at;

        let mut alpha_old = record("alpha-old", Status::Running);
        alpha_old.agent_provider = "alpha".to_string();
        alpha_old.updated_at = Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap();
        alpha_old.dispatched_at = alpha_old.updated_at;

        let mut alpha_new = record("alpha-new", Status::Running);
        alpha_new.agent_provider = "alpha".to_string();
        alpha_new.updated_at = Utc.with_ymd_and_hms(2026, 6, 3, 12, 15, 0).unwrap();
        alpha_new.dispatched_at = alpha_new.updated_at;

        let grouped = build_snapshot(
            vec![zeta_new.clone(), alpha_old.clone(), alpha_new.clone()],
            vec![work_unit()],
            &filter,
            SessionGroupBy::Provider,
        );
        let ids: Vec<&str> = grouped.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, vec!["alpha-new", "alpha-old", "zeta-new"]);

        let ungrouped = build_snapshot(
            vec![zeta_new, alpha_old, alpha_new],
            vec![work_unit()],
            &filter,
            SessionGroupBy::None,
        );
        let ids: Vec<&str> = ungrouped.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, vec!["zeta-new", "alpha-new", "alpha-old"]);
    }

    #[test]
    fn action_state_gates_resume_and_redirect() {
        let mut running = record("running", Status::Running);
        let running_actions = action_state(&running);
        assert!(running_actions.redirect.enabled);
        assert!(!running_actions.resume.enabled);

        running.status = Status::NeedsHuman;
        let terminal_actions = action_state(&running);
        assert!(!terminal_actions.redirect.enabled);
        assert!(terminal_actions.resume.enabled);

        running.agent_capabilities = Some(AgentCapabilities::default());
        let unsupported = action_state(&running);
        assert!(!unsupported.resume.enabled);
        assert!(unsupported
            .resume
            .reason
            .unwrap()
            .contains("provider does not advertise"));
    }

    #[test]
    fn action_state_enables_logs_only_for_openable_files() {
        let tempdir = tempfile::tempdir().unwrap();
        let log_file = tempdir.path().join("dispatch.jsonl");
        std::fs::write(&log_file, "{}\n").unwrap();

        let mut readable = record("readable", Status::Running);
        readable.log_file = log_file;
        let actions = action_state(&readable);
        assert!(actions.logs.enabled);
        assert!(actions.follow_logs.enabled);
    }

    #[cfg(unix)]
    #[test]
    fn action_state_rejects_unreadable_log_files() {
        use std::os::unix::fs::PermissionsExt;

        let tempdir = tempfile::tempdir().unwrap();
        let log_file = tempdir.path().join("dispatch.jsonl");
        std::fs::write(&log_file, "{}\n").unwrap();

        let mut permissions = std::fs::metadata(&log_file).unwrap().permissions();
        permissions.set_mode(0o000);
        std::fs::set_permissions(&log_file, permissions).unwrap();

        let mut unreadable = record("unreadable", Status::Running);
        unreadable.log_file = log_file.clone();
        let actions = action_state(&unreadable);
        assert!(!actions.logs.enabled);
        assert!(!actions.follow_logs.enabled);

        let mut permissions = std::fs::metadata(&log_file).unwrap().permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&log_file, permissions).unwrap();
    }

    #[test]
    fn filter_edits_apply_and_clear_values() {
        let mut filter = SessionFilter::default();

        let message = apply_filter_edit(
            &mut filter,
            FilterEdit {
                field: FilterField::Provider,
                value: " claude ".to_string(),
            },
        )
        .unwrap();
        assert_eq!(message, "provider filter set to claude");
        assert_eq!(filter.provider.as_deref(), Some("claude"));

        let message = apply_filter_edit(
            &mut filter,
            FilterEdit {
                field: FilterField::Provider,
                value: " ".to_string(),
            },
        )
        .unwrap();
        assert_eq!(message, "provider filter cleared");
        assert!(filter.provider.is_none());

        apply_filter_edit(
            &mut filter,
            FilterEdit {
                field: FilterField::Status,
                value: "needs-review".to_string(),
            },
        )
        .unwrap();
        assert_eq!(filter.status, Some(Status::NeedsReview));
    }

    #[test]
    fn filter_edits_reject_invalid_status() {
        let mut filter = SessionFilter::default();
        let err = apply_filter_edit(
            &mut filter,
            FilterEdit {
                field: FilterField::Status,
                value: "not-a-status".to_string(),
            },
        )
        .unwrap_err();
        assert!(!err.to_string().is_empty());
        assert!(filter.status.is_none());
    }

    #[test]
    fn group_by_cycles_through_interactive_order() {
        let mut group = SessionGroupBy::None;
        group = group.next();
        assert_eq!(group, SessionGroupBy::Task);
        group = group.next();
        assert_eq!(group, SessionGroupBy::WorkUnit);
        group = group.next();
        assert_eq!(group, SessionGroupBy::Branch);
        group = group.next();
        assert_eq!(group, SessionGroupBy::Provider);
        group = group.next();
        assert_eq!(group, SessionGroupBy::Status);
        group = group.next();
        assert_eq!(group, SessionGroupBy::None);
    }

    #[test]
    fn parse_poll_interval_rejects_zero_and_too_fast_values() {
        assert_eq!(parse_poll_interval(None).unwrap(), DEFAULT_POLL_INTERVAL);
        assert_eq!(
            parse_poll_interval(Some("250ms")).unwrap(),
            MIN_POLL_INTERVAL
        );
        assert!(parse_poll_interval(Some("0s"))
            .unwrap_err()
            .to_string()
            .contains("at least 250ms"));
        assert!(parse_poll_interval(Some("249ms"))
            .unwrap_err()
            .to_string()
            .contains("at least 250ms"));
    }

    #[derive(Default)]
    struct FakeTerminalActionRuntime {
        fail_leave: bool,
        fail_enter: bool,
        calls: Vec<&'static str>,
        prompts: Vec<String>,
    }

    impl TerminalActionRuntime for FakeTerminalActionRuntime {
        fn leave_tui(&mut self) -> Result<()> {
            self.calls.push("leave");
            if self.fail_leave {
                anyhow::bail!("leave failed");
            }
            Ok(())
        }

        fn prompt_for_return(&mut self, message: &str) {
            self.calls.push("prompt");
            self.prompts.push(message.to_string());
        }

        fn enter_tui(&mut self) -> Result<()> {
            self.calls.push("enter");
            if self.fail_enter {
                anyhow::bail!("enter failed");
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn terminal_action_propagates_leave_failure_without_running_action() {
        let ran = Cell::new(false);
        let mut runtime = FakeTerminalActionRuntime {
            fail_leave: true,
            ..Default::default()
        };

        let err = run_terminal_action_with_runtime(&mut runtime, || async {
            ran.set(true);
            Ok("completed".to_string())
        })
        .await
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("failed to restore terminal before running action"));
        assert!(!ran.get());
        assert_eq!(runtime.calls, vec!["leave"]);
        assert!(runtime.prompts.is_empty());
    }

    #[tokio::test]
    async fn terminal_action_propagates_enter_failure_after_prompting() {
        let mut runtime = FakeTerminalActionRuntime {
            fail_enter: true,
            ..Default::default()
        };

        let err = run_terminal_action_with_runtime(&mut runtime, || async {
            Ok("completed".to_string())
        })
        .await
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("failed to restore TUI after running action"));
        assert_eq!(runtime.calls, vec!["leave", "prompt", "enter"]);
        assert_eq!(runtime.prompts, vec!["completed"]);
    }

    #[test]
    fn tui_setup_cleanup_runs_registered_cleanup_in_reverse_order() {
        let calls = RefCell::new(Vec::new());
        let mut cleanup = TuiSetupCleanup::active();

        cleanup
            .cleanup_with_ops(
                || {
                    calls.borrow_mut().push("leave-alternate");
                    Ok(())
                },
                || {
                    calls.borrow_mut().push("disable-raw");
                    Ok(())
                },
            )
            .unwrap();

        assert_eq!(calls.into_inner(), vec!["leave-alternate", "disable-raw"]);
        assert!(!cleanup.alternate_screen_entered);
        assert!(!cleanup.raw_mode_enabled);
    }

    #[test]
    fn tui_setup_cleanup_reports_first_error_and_still_cleans_all_state() {
        let calls = RefCell::new(Vec::new());
        let mut cleanup = TuiSetupCleanup::active();

        let err = cleanup
            .cleanup_with_ops(
                || {
                    calls.borrow_mut().push("leave-alternate");
                    Err(anyhow::anyhow!("leave failed"))
                },
                || {
                    calls.borrow_mut().push("disable-raw");
                    Err(anyhow::anyhow!("raw failed"))
                },
            )
            .unwrap_err();

        assert_eq!(err.to_string(), "leave failed");
        assert_eq!(calls.into_inner(), vec!["leave-alternate", "disable-raw"]);
        assert!(!cleanup.alternate_screen_entered);
        assert!(!cleanup.raw_mode_enabled);
    }

    #[test]
    fn app_navigation_and_refresh_preserve_selection() {
        let filter = SessionFilter {
            all: true,
            ..SessionFilter::default()
        };
        let snapshot = build_snapshot(
            vec![
                record("one", Status::Running),
                record("two", Status::Running),
            ],
            vec![work_unit()],
            &filter,
            SessionGroupBy::Task,
        );
        let mut app = SessionsApp::new(snapshot);

        assert_eq!(app.selected_row().map(|row| row.id.as_str()), Some("one"));
        app.move_down();
        assert_eq!(app.selected_row().map(|row| row.id.as_str()), Some("two"));
        app.move_up();
        assert_eq!(app.selected_row().map(|row| row.id.as_str()), Some("one"));
        app.move_down();

        let refreshed = build_snapshot(
            vec![record("one", Status::Running)],
            vec![work_unit()],
            &filter,
            SessionGroupBy::Task,
        );
        app.set_snapshot(refreshed);
        assert_eq!(app.selected_row().map(|row| row.id.as_str()), Some("one"));

        let empty = build_snapshot(vec![], vec![], &filter, SessionGroupBy::Task);
        app.set_snapshot(empty);
        assert_eq!(app.selected, 0);
        assert!(app.selected_row().is_none());
    }

    #[test]
    fn once_render_contains_core_fields_without_shell_interpolation() {
        let filter = SessionFilter {
            all: true,
            ..SessionFilter::default()
        };
        let mut hostile = record("disp-$(touch pwned)", Status::NeedsHuman);
        hostile.session = "tmux; touch pwned".to_string();
        hostile.branch = "branch && rm -rf /".to_string();
        let snapshot = build_snapshot(
            vec![hostile],
            vec![work_unit()],
            &filter,
            SessionGroupBy::None,
        );
        let output = render_once(
            &snapshot,
            Utc.with_ymd_and_hms(2026, 6, 3, 13, 0, 0).unwrap(),
        );
        assert!(output.contains("ATC Sessions"));
        assert!(output.contains("tasks/harmony-794"));
        assert!(output.contains("tmux; touch pwned"));
        assert!(!PathBuf::from("pwned").exists());
    }

    #[test]
    fn once_render_escapes_terminal_control_sequences() {
        let filter = SessionFilter {
            all: true,
            ..SessionFilter::default()
        };
        let mut hostile = record("disp-\x1b]52;c;payload\x07", Status::Running);
        hostile.session = "tmux-\x1b[31mred\x1b[0m".to_string();
        hostile.agent_provider = "claude-\x1b[2J".to_string();
        let snapshot = build_snapshot(
            vec![hostile],
            vec![work_unit()],
            &filter,
            SessionGroupBy::Provider,
        );
        let output = render_once(
            &snapshot,
            Utc.with_ymd_and_hms(2026, 6, 3, 13, 0, 0).unwrap(),
        );
        assert!(output.contains("\\x1b"));
        assert!(!output.contains('\x1b'));
        assert!(!output.contains('\x07'));
    }

    #[test]
    fn renderer_draws_main_layout() {
        let filter = SessionFilter {
            all: true,
            ..SessionFilter::default()
        };
        let snapshot = build_snapshot(
            vec![record("running", Status::Running)],
            vec![work_unit()],
            &filter,
            SessionGroupBy::Task,
        );
        let app = SessionsApp::new(snapshot);
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_app(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = format!("{buffer:?}");
        assert!(rendered.contains("ATC Sessions"));
        assert!(rendered.contains("tasks/harmony-794"));
        assert!(rendered.contains("session-running"));
    }

    #[test]
    fn renderer_draws_group_column_when_grouped() {
        let filter = SessionFilter {
            all: true,
            ..SessionFilter::default()
        };
        let mut grouped = record("running", Status::Running);
        grouped.branch = "visible-group-branch".to_string();
        let snapshot = build_snapshot(
            vec![grouped],
            vec![work_unit()],
            &filter,
            SessionGroupBy::Branch,
        );
        let app = SessionsApp::new(snapshot);
        let backend = TestBackend::new(140, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_app(frame, &app)).unwrap();
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("group"));
        assert!(rendered.contains("visible-group-branch"));
    }

    #[test]
    fn renderer_draws_empty_state() {
        let filter = SessionFilter::default();
        let snapshot = build_snapshot(vec![], vec![], &filter, SessionGroupBy::None);
        let app = SessionsApp::new(snapshot);
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_app(frame, &app)).unwrap();
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("No sessions found"));
    }

    #[test]
    fn renderer_draws_detail_with_disabled_actions() {
        let filter = SessionFilter {
            all: true,
            ..SessionFilter::default()
        };
        let mut unsupported = record("unsupported", Status::NeedsHuman);
        unsupported.agent_capabilities = Some(AgentCapabilities::default());
        unsupported.agent_session_id = None;
        unsupported.agent_transcript_cwd = None;
        let snapshot = build_snapshot(
            vec![unsupported],
            vec![work_unit()],
            &filter,
            SessionGroupBy::Task,
        );
        let mut app = SessionsApp::new(snapshot);
        app.show_detail = true;
        let backend = TestBackend::new(140, 36);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_app(frame, &app)).unwrap();
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("provider_session"));
        assert!(rendered.contains("provider does not advertise"));
        assert!(rendered.contains("log file missing or unreadable"));
    }

    #[test]
    fn detail_text_escapes_terminal_controls_in_pr_list() {
        let filter = SessionFilter {
            all: true,
            ..SessionFilter::default()
        };
        let mut hostile = record("hostile-pr", Status::NeedsHuman);
        hostile.pr_urls =
            vec!["https://github.com/acme/repo\x1b\x07\u{202e}gpj.exe/pull/99".to_string()];
        let snapshot = build_snapshot(
            vec![hostile],
            vec![work_unit()],
            &filter,
            SessionGroupBy::Task,
        );

        let detail = detail_text(&snapshot.rows[0]);

        assert!(detail.contains("repo\\x1b\\x07\\u{202e}gpj.exe#99"));
        assert!(!detail.contains('\x1b'));
        assert!(!detail.contains('\x07'));
        assert!(!detail.contains('\u{202e}'));
    }

    #[test]
    fn renderer_draws_confirmation_dialog() {
        let filter = SessionFilter {
            all: true,
            ..SessionFilter::default()
        };
        let snapshot = build_snapshot(
            vec![record("running", Status::Running)],
            vec![work_unit()],
            &filter,
            SessionGroupBy::Task,
        );
        let mut app = SessionsApp::new(snapshot);
        app.pending_confirmation = Some(PendingAction::Cleanup);
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_app(frame, &app)).unwrap();
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("Confirm cleanup selected dispatch"));
        assert!(rendered.contains("Press y to confirm"));
    }
}
