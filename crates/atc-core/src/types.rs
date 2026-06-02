use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

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
    /// Work unit this dispatch belongs to (nullable for pre-work-unit dispatches).
    pub work_unit_id: Option<String>,
    /// Agent harness/provider that ran this dispatch, initially `claude`.
    pub agent_provider: String,
    /// Provider-native durable session/conversation ID, distinct from ATC's tmux session name.
    pub agent_session_id: Option<AgentSessionId>,
    /// CWD the provider uses for transcript/session persistence.
    pub agent_transcript_cwd: Option<PathBuf>,
    /// Dispatch ID this record resumes from. Populated by future resume work.
    pub resume_of_dispatch_id: Option<String>,
    /// Capability snapshot for the provider at dispatch time.
    pub agent_capabilities: Option<AgentCapabilities>,
    pub dispatched_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub const CLAUDE_AGENT_PROVIDER: &str = "claude";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentSessionId(uuid::Uuid);

impl AgentSessionId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    pub fn parse_str(value: &str) -> anyhow::Result<Self> {
        let uuid = uuid::Uuid::parse_str(value)
            .map_err(|e| anyhow::anyhow!("invalid agent session id {value:?}: {e}"))?;
        Ok(Self(uuid))
    }
}

impl Default for AgentSessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for AgentSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for AgentSessionId {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse_str(value)
    }
}

impl Serialize for AgentSessionId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for AgentSessionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse_str(&value).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentCapabilities {
    pub supports_resume_by_session_id: bool,
    pub supports_explicit_session_id_on_start: bool,
    pub supports_tmux_attach: bool,
    pub supports_tmux_redirect: bool,
    pub supports_stream_json_output: bool,
    pub supports_cost_and_turn_reporting: bool,
}

pub fn claude_agent_capabilities() -> AgentCapabilities {
    AgentCapabilities {
        supports_resume_by_session_id: true,
        supports_explicit_session_id_on_start: true,
        supports_tmux_attach: true,
        supports_tmux_redirect: true,
        supports_stream_json_output: true,
        supports_cost_and_turn_reporting: true,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionMetadata {
    pub provider: String,
    pub session_id: Option<AgentSessionId>,
    pub transcript_cwd: Option<PathBuf>,
    pub resume_of_dispatch_id: Option<String>,
    pub capabilities: Option<AgentCapabilities>,
}

impl AgentSessionMetadata {
    pub fn new_claude(transcript_cwd: PathBuf) -> Self {
        Self {
            provider: CLAUDE_AGENT_PROVIDER.to_string(),
            session_id: Some(AgentSessionId::new()),
            transcript_cwd: Some(transcript_cwd),
            resume_of_dispatch_id: None,
            capabilities: Some(claude_agent_capabilities()),
        }
    }

    /// Ephemeral/preview dispatches know the provider but deliberately do not
    /// create a durable provider-native session that ATC cannot persist.
    pub fn claude_without_session() -> Self {
        Self {
            provider: CLAUDE_AGENT_PROVIDER.to_string(),
            session_id: None,
            transcript_cwd: None,
            resume_of_dispatch_id: None,
            capabilities: None,
        }
    }
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

/// Worktree routing policy for template-based dispatches.
///
/// Controls how the pipeline resolves the agent's working directory:
/// - `Branch`: create/reuse a worktree by branch name (default, current behavior)
/// - `Document`: resolve CWD from where the target document is checked out
/// - `None`: run in canonical repo root, no worktree creation
/// - `Current`: run in the current working directory as-is
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorktreePolicy {
    #[default]
    Branch,
    Document,
    None,
    Current,
}

impl WorktreePolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorktreePolicy::Branch => "branch",
            WorktreePolicy::Document => "document",
            WorktreePolicy::None => "none",
            WorktreePolicy::Current => "current",
        }
    }
}

impl std::str::FromStr for WorktreePolicy {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "branch" => Ok(WorktreePolicy::Branch),
            "document" => Ok(WorktreePolicy::Document),
            "none" => Ok(WorktreePolicy::None),
            "current" => Ok(WorktreePolicy::Current),
            other => Err(anyhow::anyhow!("unknown worktree policy: {}", other)),
        }
    }
}

impl std::fmt::Display for WorktreePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A work unit groups all dispatches, PRs, and branches for a piece of work.
/// It's the backing data for the desktop/web task lifecycle strip and session history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkUnit {
    /// ULID
    pub id: String,
    /// KB task slug (nullable — not all work has a task)
    pub task_slug: Option<String>,
    /// Shared branch name across all repos
    pub branch: Option<String>,
    /// Repo paths involved (e.g., ["open-source/atc", "platform/api"])
    pub repos: Vec<String>,
    /// Accumulates PR URLs across dispatches and repos
    pub pr_urls: Vec<String>,
    /// active, merged, closed, abandoned
    pub status: WorkUnitStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkUnitStatus {
    Active,
    Merged,
    Closed,
    Abandoned,
}

impl WorkUnitStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkUnitStatus::Active => "active",
            WorkUnitStatus::Merged => "merged",
            WorkUnitStatus::Closed => "closed",
            WorkUnitStatus::Abandoned => "abandoned",
        }
    }
}

impl std::str::FromStr for WorkUnitStatus {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(WorkUnitStatus::Active),
            "merged" => Ok(WorkUnitStatus::Merged),
            "closed" => Ok(WorkUnitStatus::Closed),
            "abandoned" => Ok(WorkUnitStatus::Abandoned),
            other => Err(anyhow::anyhow!("unknown work unit status: {}", other)),
        }
    }
}

impl std::fmt::Display for WorkUnitStatus {
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
    /// Ephemeral mode: skip registry, logs, system prompt, providers.
    pub ephemeral: bool,
    /// Inline timeout in seconds.
    pub timeout: Option<u32>,
    /// Emit a structured JSON envelope on stdout instead of human-readable
    /// confirmation. Suppresses the post-dispatch text block and the dry-run
    /// preview text. See `atc run --help` for the v1 schema.
    pub json: bool,
}

/// Outcome of a successful dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchOutcome {
    pub id: String,
    pub session: String,
    pub inline_exit_code: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_opts_ephemeral_defaults() {
        let opts = RunOpts {
            input: "test".to_string(),
            directive: None,
            params: std::collections::HashMap::new(),
            pr_url: None,
            repos: vec![],
            inline: false,
            force: false,
            dry_run: false,
            directives: None,
            no_worktree: false,
            max_budget_usd: None,
            max_turns: None,
            retries: 0,
            list: false,
            ephemeral: false,
            timeout: None,
            json: false,
        };
        assert!(!opts.ephemeral, "ephemeral should default to false");
        assert_eq!(opts.timeout, None, "timeout should default to None");
        assert!(!opts.json, "json should default to false");
    }

    #[test]
    fn test_agent_session_id_new_is_uuid() {
        let id = AgentSessionId::new();
        uuid::Uuid::parse_str(&id.to_string()).expect("agent session id should be a valid UUID");
    }

    #[test]
    fn test_agent_session_id_rejects_invalid_values() {
        assert!(AgentSessionId::parse_str("not-a-uuid").is_err());
        assert!(AgentSessionId::parse_str("00000000-0000-4000-8000-000000000001\0").is_err());
    }

    #[test]
    fn test_claude_agent_metadata_creates_durable_session() {
        let metadata = AgentSessionMetadata::new_claude(PathBuf::from("/tmp/worktree"));
        assert_eq!(metadata.provider, CLAUDE_AGENT_PROVIDER);
        assert!(metadata.session_id.is_some());
        assert_eq!(
            metadata.transcript_cwd.as_deref(),
            Some(std::path::Path::new("/tmp/worktree"))
        );
        assert!(metadata.capabilities.is_some());
    }

    #[test]
    fn test_claude_without_session_is_not_durable() {
        let metadata = AgentSessionMetadata::claude_without_session();
        assert_eq!(metadata.provider, CLAUDE_AGENT_PROVIDER);
        assert!(metadata.session_id.is_none());
        assert!(metadata.transcript_cwd.is_none());
        assert!(metadata.capabilities.is_none());
    }

    #[test]
    fn test_agent_capabilities_missing_fields_default_false() {
        let value: AgentCapabilities =
            serde_json::from_str(r#"{"supports_resume_by_session_id":true}"#).unwrap();
        assert!(value.supports_resume_by_session_id);
        assert!(!value.supports_explicit_session_id_on_start);
        assert!(!value.supports_tmux_attach);
        assert!(!value.supports_tmux_redirect);
        assert!(!value.supports_stream_json_output);
        assert!(!value.supports_cost_and_turn_reporting);
    }

    #[test]
    fn test_claude_agent_capabilities_shape() {
        let value = claude_agent_capabilities();
        assert!(value.supports_resume_by_session_id);
        assert!(value.supports_explicit_session_id_on_start);
        assert!(value.supports_stream_json_output);
    }

    #[test]
    fn test_worktree_policy_from_str() {
        assert_eq!(
            "branch".parse::<WorktreePolicy>().unwrap(),
            WorktreePolicy::Branch
        );
        assert_eq!(
            "document".parse::<WorktreePolicy>().unwrap(),
            WorktreePolicy::Document
        );
        assert_eq!(
            "none".parse::<WorktreePolicy>().unwrap(),
            WorktreePolicy::None
        );
        assert_eq!(
            "current".parse::<WorktreePolicy>().unwrap(),
            WorktreePolicy::Current
        );
        assert!("invalid".parse::<WorktreePolicy>().is_err());
    }

    #[test]
    fn test_worktree_policy_display_roundtrip() {
        for policy in [
            WorktreePolicy::Branch,
            WorktreePolicy::Document,
            WorktreePolicy::None,
            WorktreePolicy::Current,
        ] {
            let s = policy.to_string();
            let parsed: WorktreePolicy = s.parse().unwrap();
            assert_eq!(parsed, policy, "roundtrip failed for {:?}", policy);
        }
    }

    #[test]
    fn test_worktree_policy_default() {
        assert_eq!(WorktreePolicy::default(), WorktreePolicy::Branch);
    }
}
