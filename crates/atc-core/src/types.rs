use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DispatchRecord {
    /// Dispatch-unique ID: `<branch>@<mode>@<unix-timestamp>`
    pub id: String,
    /// Nullable — template/prompt dispatches have no task.
    pub task_slug: Option<String>,
    pub branch: String,
    pub worktree_path: PathBuf,
    pub session: String,
    pub log_file: PathBuf,
    pub status: Status,
    pub mode: Mode,
    pub retries: u32,
    /// Which InputResolver created this dispatch ("task", "template", "prompt").
    pub resolver: String,
    pub pr_url: Option<String>,
    pub checks: HealthChecks,
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u32>,
    pub duration_ms: Option<u64>,
    pub dispatched_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthChecks {
    /// True when the agent's tmux session has ended (the process terminated).
    /// This does NOT distinguish between a clean exit, crash, or OOM kill —
    /// it only reflects that the session no longer exists. Downstream signals
    /// (branch_pushed, ci_passed, reviews_approved, threads_resolved) drive
    /// the subsequent health/status transitions independently.
    pub agent_exited_clean: bool,
    pub branch_pushed: bool,
    pub pr_created: bool,
    pub ci_passed: bool,
    pub reviews_approved: bool,
    pub threads_resolved: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Running,
    Done,
    Failed,
    NeedsReview,
    NeedsHuman,
    Stopped,
    Retrying,
}

impl Status {
    /// Returns true for terminal states (Done, Failed, Stopped, NeedsHuman).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Status::Done | Status::Failed | Status::Stopped | Status::NeedsHuman
        )
    }

    /// Canonical string used in SQLite TEXT column and JSON serialization
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::NeedsReview => "needs-review",
            Status::NeedsHuman => "needs-human",
            Status::Stopped => "stopped",
            Status::Retrying => "retrying",
        }
    }
}

impl std::str::FromStr for Status {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "running" => Ok(Status::Running),
            "done" => Ok(Status::Done),
            "failed" => Ok(Status::Failed),
            "needs-review" => Ok(Status::NeedsReview),
            "needs-human" => Ok(Status::NeedsHuman),
            "stopped" => Ok(Status::Stopped),
            "retrying" => Ok(Status::Retrying),
            other => Err(anyhow::anyhow!("unknown status: {}", other)),
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[clap(rename_all = "kebab-case")]
pub enum Mode {
    Implement,
    Research,
    KbUpdate,
    ReviewFix,
    PrComments,
    Refine,
    CreateTask,
    Close,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Implement => "implement",
            Mode::Research => "research",
            Mode::KbUpdate => "kb-update",
            Mode::ReviewFix => "review-fix",
            Mode::PrComments => "pr-comments",
            Mode::Refine => "refine",
            Mode::CreateTask => "create-task",
            Mode::Close => "close",
        }
    }
}

impl std::str::FromStr for Mode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "implement" => Ok(Mode::Implement),
            "research" => Ok(Mode::Research),
            "kb-update" => Ok(Mode::KbUpdate),
            "review-fix" => Ok(Mode::ReviewFix),
            "pr-comments" => Ok(Mode::PrComments),
            "refine" => Ok(Mode::Refine),
            "create-task" => Ok(Mode::CreateTask),
            "close" => Ok(Mode::Close),
            other => Err(anyhow::anyhow!("unknown mode: {}", other)),
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Options for a single dispatch invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct DispatchOpts {
    pub slug: String,
    pub cli_mode: Option<Mode>,
    pub directive: Option<String>,
    pub pr_url: Option<String>,
    pub inline: bool,
    pub force: bool,
    pub dry_run: bool,
    pub max_budget_override: Option<f64>,
    pub max_turns_override: Option<u32>,
    /// Retry count to propagate into the new dispatch record (default 0).
    pub retries: u32,
}

/// Outcome of a successful dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchOutcome {
    pub id: String,
    pub session: String,
    pub inline_exit_code: Option<i32>,
}
