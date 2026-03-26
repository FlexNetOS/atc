use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DispatchRecord {
    /// Dispatch-unique ID: `<branch>@<directive>@<unix-timestamp>`
    pub id: String,
    /// Nullable — template/prompt dispatches have no task.
    pub task_slug: Option<String>,
    pub branch: String,
    pub worktree_path: PathBuf,
    pub session: String,
    pub log_file: PathBuf,
    pub status: Status,
    pub directive: Directive,
    pub retries: u32,
    /// Which InputResolver created this dispatch ("task", "template", "prompt").
    pub resolver: String,
    pub pr_urls: Vec<String>,
    /// Whether the dispatch was created with `--no-worktree` (run in current directory).
    pub no_worktree: bool,
    /// The raw input string passed to the pipeline (slug, template name, or prompt).
    /// Used by retry to faithfully reconstruct the original `RunOpts`.
    pub original_input: Option<String>,
    pub checks: HealthChecks,
    /// The KB root used by this dispatch (for task-based dispatches where the
    /// KB root may differ from the workspace root via multi-KB discovery).
    /// Persisted so `on_cleanup` can unassign without re-discovering.
    pub kb_root: Option<PathBuf>,
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u32>,
    pub duration_ms: Option<u64>,
    /// JSON blob stored by post-completion (always written, even when no result event
    /// is found). `Some` means post-completion already ran for this record.
    pub artifacts: Option<String>,
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
    /// Returns true for terminal states (Done, Failed, Stopped, NeedsHuman, NeedsReview).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Status::Done
                | Status::Failed
                | Status::NeedsHuman
                | Status::NeedsReview
                | Status::Stopped
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
pub enum Directive {
    Implement,
    Research,
    KbUpdate,
    ReviewFix,
    PrComments,
    Refine,
    CreateTask,
    Close,
}

impl Directive {
    pub fn as_str(&self) -> &'static str {
        match self {
            Directive::Implement => "implement",
            Directive::Research => "research",
            Directive::KbUpdate => "kb-update",
            Directive::ReviewFix => "review-fix",
            Directive::PrComments => "pr-comments",
            Directive::Refine => "refine",
            Directive::CreateTask => "create-task",
            Directive::Close => "close",
        }
    }
}

impl std::str::FromStr for Directive {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "implement" => Ok(Directive::Implement),
            "research" => Ok(Directive::Research),
            "kb-update" => Ok(Directive::KbUpdate),
            "review-fix" => Ok(Directive::ReviewFix),
            "pr-comments" => Ok(Directive::PrComments),
            "refine" => Ok(Directive::Refine),
            "create-task" => Ok(Directive::CreateTask),
            "close" => Ok(Directive::Close),
            other => Err(anyhow::anyhow!("unknown directive: {}", other)),
        }
    }
}

impl std::fmt::Display for Directive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Options for an `atc run` invocation.
#[derive(Debug, Clone)]
pub struct RunOpts {
    /// Raw input string (joined from CLI positional args).
    pub input: String,
    /// Explicit directive override from `--directive`.
    pub directive: Option<Directive>,
    /// Key=value pairs for template rendering.
    pub params: std::collections::HashMap<String, String>,
    /// PR URL for review-fix / pr-comments directives.
    pub pr_url: Option<String>,
    /// Target repo path(s) within meta workspace (e.g., "open-source/atc").
    /// Overrides auto-discovered repo path from PR URL or config.
    pub repos: Vec<String>,
    /// Run inline (synchronous, no tmux).
    pub inline: bool,
    /// Force dispatch even if worktree is in use.
    pub force: bool,
    /// Preview config without launching.
    pub dry_run: bool,
    /// Comma-separated directive override.
    pub directives: Option<String>,
    /// Skip worktree creation (run in current directory).
    pub no_worktree: bool,
    /// Override max budget (USD).
    pub max_budget_usd: Option<f64>,
    /// Override max turns.
    pub max_turns: Option<u32>,
    /// Retry count (propagated on retry).
    pub retries: u32,
    /// List available templates instead of dispatching.
    pub list: bool,
}

/// Outcome of a successful dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchOutcome {
    pub id: String,
    pub session: String,
    pub inline_exit_code: Option<i32>,
}
