use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DispatchRecord {
    pub slug: String,
    pub branch: String,
    pub worktree_path: PathBuf,
    pub session: String,
    pub log_file: PathBuf,
    pub status: Status,
    pub mode: Mode,
    pub retries: u32,
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
    pub agent_exited_clean: bool,
    pub branch_pushed: bool,
    pub pr_created: bool,
    pub ci_passed: bool,
    pub reviews_approved: bool,
    pub threads_resolved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Running,
    Done,
    Failed,
    NeedsReview,
    NeedsHuman,
}

impl Status {
    /// Canonical string used in SQLite TEXT column and JSON serialization
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::NeedsReview => "needs-review",
            Status::NeedsHuman => "needs-human",
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
        }
    }

    /// TOML config key for this mode (matches CLI mode names, hyphenated).
    pub fn config_key(&self) -> &'static str {
        self.as_str()
    }

    /// Built-in default template for this mode (compiled into the binary).
    pub fn default_template(&self) -> &'static str {
        match self {
            Mode::Implement => crate::templates::IMPLEMENT,
            Mode::Research => crate::templates::RESEARCH,
            Mode::KbUpdate => crate::templates::KB_UPDATE,
            Mode::ReviewFix => crate::templates::REVIEW_FIX,
            Mode::PrComments => crate::templates::PR_COMMENTS,
            Mode::Refine => crate::templates::REFINE,
            Mode::CreateTask => crate::templates::CREATE_TASK,
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
            other => Err(anyhow::anyhow!("unknown mode: {}", other)),
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
