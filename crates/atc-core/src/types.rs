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
    /// Dispatch ID this record resumes from, when created by `atc run --resume`.
    pub resume_of_dispatch_id: Option<String>,
    /// Capability snapshot for the provider at dispatch time.
    pub agent_capabilities: Option<AgentCapabilities>,
    /// Structured terminal locator captured for this dispatch, when ATC owns
    /// the terminal/session lifecycle.
    pub terminal_locator: Option<TerminalLocator>,
    pub dispatched_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub const CLAUDE_AGENT_PROVIDER: &str = "claude";
pub const ATC_SESSION_URI_PREFIX: &str = "atc://session/";
const TMUX_TERMINAL_LOCATOR_VERSION: u32 = 1;
const CLOUD_TERMINAL_LOCATOR_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TerminalLocator {
    Tmux(TmuxTerminalLocator),
    /// A remote agent running on a cloud worker (e.g. a Fly Machine). The
    /// `session` field carries the worker/Machine id rather than a tmux session
    /// name. Introduced for Cloud ATC ([[specs/cloud-atc]] P1).
    Cloud(CloudTerminalLocator),
}

impl<'de> Deserialize<'de> for TerminalLocator {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct TerminalLocatorDiscriminator {
            #[serde(default)]
            kind: Option<String>,
            #[serde(default)]
            backend: Option<String>,
        }

        #[derive(Deserialize)]
        struct TmuxTerminalLocatorWire {
            version: u32,
            session: String,
            #[serde(default)]
            cwd: Option<PathBuf>,
            detected_at: DateTime<Utc>,
            source: TerminalLocatorSource,
            confidence: TerminalLocatorConfidence,
        }

        #[derive(Deserialize)]
        struct CloudTerminalLocatorWire {
            version: u32,
            session: String,
            #[serde(default)]
            app: Option<String>,
            #[serde(default)]
            region: Option<String>,
            #[serde(default)]
            cwd: Option<PathBuf>,
            detected_at: DateTime<Utc>,
            source: TerminalLocatorSource,
            confidence: TerminalLocatorConfidence,
        }

        let value = serde_json::Value::deserialize(deserializer)?;
        let wire = TerminalLocatorDiscriminator::deserialize(&value).map_err(de::Error::custom)?;
        let discriminator = match (wire.kind.as_deref(), wire.backend.as_deref()) {
            (Some(kind), Some(backend)) if kind != backend => {
                return Err(de::Error::custom(format!(
                    "terminal locator kind/backend mismatch: {} != {}",
                    crate::terminal_text::display_text(kind),
                    crate::terminal_text::display_text(backend)
                )));
            }
            (Some(kind), _) => kind,
            (_, Some(backend)) => backend,
            (None, None) => {
                return Err(de::Error::custom(
                    "terminal locator is missing kind or backend discriminator",
                ));
            }
        };

        match discriminator {
            "tmux" => {
                let tmux =
                    TmuxTerminalLocatorWire::deserialize(value).map_err(de::Error::custom)?;
                if tmux.version != TMUX_TERMINAL_LOCATOR_VERSION {
                    return Err(de::Error::custom(format!(
                        "unsupported tmux terminal locator version: {}",
                        tmux.version
                    )));
                }
                if tmux.session.trim().is_empty() {
                    return Err(de::Error::custom("tmux terminal locator session is empty"));
                }
                Ok(Self::Tmux(TmuxTerminalLocator {
                    version: tmux.version,
                    session: tmux.session,
                    cwd: tmux.cwd,
                    detected_at: tmux.detected_at,
                    source: tmux.source,
                    confidence: tmux.confidence,
                }))
            }
            "cloud" => {
                let cloud =
                    CloudTerminalLocatorWire::deserialize(value).map_err(de::Error::custom)?;
                if cloud.version != CLOUD_TERMINAL_LOCATOR_VERSION {
                    return Err(de::Error::custom(format!(
                        "unsupported cloud terminal locator version: {}",
                        cloud.version
                    )));
                }
                if cloud.session.trim().is_empty() {
                    return Err(de::Error::custom("cloud terminal locator session is empty"));
                }
                Ok(Self::Cloud(CloudTerminalLocator {
                    version: cloud.version,
                    session: cloud.session,
                    app: cloud.app,
                    region: cloud.region,
                    cwd: cloud.cwd,
                    detected_at: cloud.detected_at,
                    source: cloud.source,
                    confidence: cloud.confidence,
                }))
            }
            other => Err(de::Error::custom(format!(
                "unsupported terminal locator kind: {}",
                crate::terminal_text::display_text(other)
            ))),
        }
    }
}

impl TerminalLocator {
    pub fn atc_tmux(
        session: impl Into<String>,
        cwd: Option<PathBuf>,
        detected_at: DateTime<Utc>,
    ) -> Self {
        Self::Tmux(TmuxTerminalLocator {
            version: TMUX_TERMINAL_LOCATOR_VERSION,
            session: session.into(),
            cwd,
            detected_at,
            source: TerminalLocatorSource::AtcDispatch,
            confidence: TerminalLocatorConfidence::Exact,
        })
    }

    pub fn inferred_tmux(
        session: impl Into<String>,
        cwd: Option<PathBuf>,
        detected_at: DateTime<Utc>,
    ) -> Self {
        Self::Tmux(TmuxTerminalLocator {
            version: TMUX_TERMINAL_LOCATOR_VERSION,
            session: session.into(),
            cwd,
            detected_at,
            source: TerminalLocatorSource::LegacySessionField,
            confidence: TerminalLocatorConfidence::Inferred,
        })
    }

    /// Build a locator for a remote agent running on a cloud worker. `session`
    /// carries the worker/Machine id. Used by the remote executor path.
    pub fn atc_cloud(
        session: impl Into<String>,
        app: Option<String>,
        region: Option<String>,
        cwd: Option<PathBuf>,
        detected_at: DateTime<Utc>,
    ) -> Self {
        Self::Cloud(CloudTerminalLocator {
            version: CLOUD_TERMINAL_LOCATOR_VERSION,
            session: session.into(),
            app,
            region,
            cwd,
            detected_at,
            source: TerminalLocatorSource::AtcDispatch,
            confidence: TerminalLocatorConfidence::Exact,
        })
    }

    pub fn backend(&self) -> &'static str {
        match self {
            Self::Tmux(_) => "tmux",
            Self::Cloud(_) => "cloud",
        }
    }

    pub fn tmux_session(&self) -> Option<&str> {
        match self {
            Self::Tmux(locator) => Some(locator.session.as_str()),
            Self::Cloud(_) => None,
        }
    }

    /// The worker/Machine id for a cloud locator, if any.
    pub fn cloud_worker_id(&self) -> Option<&str> {
        match self {
            Self::Cloud(locator) => Some(locator.session.as_str()),
            Self::Tmux(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TmuxTerminalLocator {
    pub version: u32,
    pub session: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    pub detected_at: DateTime<Utc>,
    pub source: TerminalLocatorSource,
    pub confidence: TerminalLocatorConfidence,
}

/// Locator for a remote agent running on a cloud worker (Fly Machine). The
/// `session` field holds the worker/Machine id. `app`/`region` identify the
/// Fly app and region the Machine ran in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudTerminalLocator {
    pub version: u32,
    /// Worker/Machine id (analogous to a tmux session name for local dispatch).
    pub session: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    pub detected_at: DateTime<Utc>,
    pub source: TerminalLocatorSource,
    pub confidence: TerminalLocatorConfidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalLocatorSource {
    AtcDispatch,
    LegacySessionField,
}

impl TerminalLocatorSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AtcDispatch => "atc-dispatch",
            Self::LegacySessionField => "legacy-session-field",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalLocatorConfidence {
    Exact,
    Inferred,
}

impl TerminalLocatorConfidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Inferred => "inferred",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalStatusState {
    Focusable,
    Attached,
    Detached,
    Running,
    Stale,
    Unavailable,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalStatus {
    pub state: TerminalStatusState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl TerminalStatus {
    pub fn new(state: TerminalStatusState, backend: Option<impl Into<String>>) -> Self {
        Self {
            state,
            backend: backend.map(Into::into),
            reason: None,
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            state: TerminalStatusState::Unavailable,
            backend: None,
            reason: Some(reason.into()),
        }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    pub fn is_openable(&self) -> bool {
        matches!(
            self.state,
            TerminalStatusState::Focusable
                | TerminalStatusState::Attached
                | TerminalStatusState::Detached
                | TerminalStatusState::Running
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenSessionPreview {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attach_command: Option<Vec<String>>,
}

impl OpenSessionPreview {
    pub fn enabled(
        action: impl Into<String>,
        backend: impl Into<String>,
        attach_command: Vec<String>,
    ) -> Self {
        Self {
            enabled: true,
            reason: None,
            action: action.into(),
            backend: Some(backend.into()),
            attach_command: Some(attach_command),
        }
    }

    pub fn disabled(action: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            enabled: false,
            reason: Some(reason.into()),
            action: action.into(),
            backend: None,
            attach_command: None,
        }
    }
}

pub fn atc_session_uri(dispatch_id: &str) -> String {
    format!(
        "{ATC_SESSION_URI_PREFIX}{}",
        percent_encode_resource_id(dispatch_id)
    )
}

pub fn parse_atc_session_uri(value: &str) -> anyhow::Result<String> {
    let Some(encoded) = value.strip_prefix(ATC_SESSION_URI_PREFIX) else {
        anyhow::bail!("unsupported atc resource URI; expected atc://session/<id>");
    };
    anyhow::ensure!(!encoded.is_empty(), "atc session URI is missing an id");
    percent_decode_resource_id(encoded)
}

fn percent_encode_resource_id(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if is_unreserved_uri_byte(byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(hex_char(byte >> 4));
            encoded.push(hex_char(byte & 0x0f));
        }
    }
    encoded
}

fn percent_decode_resource_id(value: &str) -> anyhow::Result<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b'%' => {
                anyhow::ensure!(idx + 2 < bytes.len(), "truncated percent escape in atc URI");
                let hi = from_hex(bytes[idx + 1])?;
                let lo = from_hex(bytes[idx + 2])?;
                decoded.push((hi << 4) | lo);
                idx += 3;
            }
            byte if is_unreserved_uri_byte(byte) => {
                decoded.push(byte);
                idx += 1;
            }
            byte => {
                anyhow::bail!("reserved byte in atc URI id must be percent-encoded: 0x{byte:02X}")
            }
        }
    }
    let decoded =
        String::from_utf8(decoded).map_err(|e| anyhow::anyhow!("atc URI id is not UTF-8: {e}"))?;
    anyhow::ensure!(
        !decoded.chars().any(is_disallowed_uri_id_char),
        "atc URI id contains a disallowed control or format character"
    );
    Ok(decoded)
}

fn is_unreserved_uri_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~'
    )
}

fn hex_char(value: u8) -> char {
    match value {
        0..=9 => char::from(b'0' + value),
        10..=15 => char::from(b'A' + (value - 10)),
        _ => unreachable!("hex nibble is always <= 15"),
    }
}

fn from_hex(value: u8) -> anyhow::Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        other => anyhow::bail!("invalid percent escape byte: 0x{other:02X}"),
    }
}

fn is_disallowed_uri_id_char(value: char) -> bool {
    value.is_control() || crate::terminal_text::is_dangerous_format_control(value)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentProviderDescriptor {
    pub name: &'static str,
    pub supports_durable_session_on_start: bool,
    pub capabilities: AgentCapabilities,
}

impl AgentProviderDescriptor {
    pub fn durable_session_metadata(&self, transcript_cwd: PathBuf) -> AgentSessionMetadata {
        AgentSessionMetadata {
            provider: self.name.to_string(),
            session_id: self
                .supports_durable_session_on_start
                .then(AgentSessionId::new),
            transcript_cwd: Some(transcript_cwd),
            resume_of_dispatch_id: None,
            capabilities: Some(self.capabilities()),
        }
    }

    pub fn resume_session_metadata(
        &self,
        transcript_cwd: PathBuf,
        session_id: AgentSessionId,
        resume_of_dispatch_id: impl Into<String>,
    ) -> AgentSessionMetadata {
        AgentSessionMetadata {
            provider: self.name.to_string(),
            session_id: Some(session_id),
            transcript_cwd: Some(transcript_cwd),
            resume_of_dispatch_id: Some(resume_of_dispatch_id.into()),
            capabilities: Some(self.capabilities()),
        }
    }

    /// Ephemeral/preview dispatches know the provider but deliberately do not
    /// create a durable provider-native session that ATC cannot persist.
    pub fn ephemeral_session_metadata(&self) -> AgentSessionMetadata {
        AgentSessionMetadata {
            provider: self.name.to_string(),
            session_id: None,
            transcript_cwd: None,
            resume_of_dispatch_id: None,
            capabilities: Some(self.capabilities),
        }
    }

    pub fn capabilities(&self) -> AgentCapabilities {
        self.capabilities
    }
}

pub const CLAUDE_AGENT_DESCRIPTOR: AgentProviderDescriptor = AgentProviderDescriptor {
    name: CLAUDE_AGENT_PROVIDER,
    supports_durable_session_on_start: true,
    capabilities: CLAUDE_AGENT_CAPABILITIES,
};

pub static AGENT_PROVIDER_DESCRIPTORS: &[AgentProviderDescriptor] = &[CLAUDE_AGENT_DESCRIPTOR];

pub fn agent_provider_descriptor(name: &str) -> Option<AgentProviderDescriptor> {
    AGENT_PROVIDER_DESCRIPTORS
        .iter()
        .copied()
        .find(|provider| provider.name == name)
}

pub fn agent_provider_descriptors() -> &'static [AgentProviderDescriptor] {
    AGENT_PROVIDER_DESCRIPTORS
}

pub fn claude_agent_provider() -> AgentProviderDescriptor {
    agent_provider_descriptor(CLAUDE_AGENT_PROVIDER).expect("Claude provider must be registered")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentSessionId(uuid::Uuid);

impl AgentSessionId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    pub fn parse_str(value: &str) -> anyhow::Result<Self> {
        let uuid = uuid::Uuid::parse_str(value).map_err(|e| {
            anyhow::anyhow!(
                "invalid agent session id '{}': {e}",
                crate::terminal_text::display_text(value)
            )
        })?;
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentCapabilities {
    pub supports_resume_by_session_id: bool,
    pub supports_explicit_session_id_on_start: bool,
    pub supports_tmux_attach: bool,
    pub supports_tmux_redirect: bool,
    pub supports_stream_json_output: bool,
    pub supports_cost_and_turn_reporting: bool,
}

pub const CLAUDE_AGENT_CAPABILITIES: AgentCapabilities = AgentCapabilities {
    supports_resume_by_session_id: true,
    supports_explicit_session_id_on_start: true,
    supports_tmux_attach: true,
    supports_tmux_redirect: true,
    supports_stream_json_output: true,
    supports_cost_and_turn_reporting: true,
};

pub fn claude_agent_capabilities() -> AgentCapabilities {
    CLAUDE_AGENT_CAPABILITIES
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
        claude_agent_provider().durable_session_metadata(transcript_cwd)
    }

    pub fn claude_without_session() -> Self {
        claude_agent_provider().ephemeral_session_metadata()
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
            other => Err(anyhow::anyhow!(
                "unknown status: {}",
                crate::terminal_text::display_text(other)
            )),
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
            other => Err(anyhow::anyhow!(
                "unknown directive: {}",
                crate::terminal_text::display_text(other)
            )),
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
            other => Err(anyhow::anyhow!(
                "unknown worktree policy: {}",
                crate::terminal_text::display_text(other)
            )),
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
            other => Err(anyhow::anyhow!(
                "unknown work unit status: {}",
                crate::terminal_text::display_text(other)
            )),
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
    /// Dispatch ID or task slug whose provider-native session should be resumed.
    pub resume: Option<String>,
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
            resume: None,
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
    fn test_agent_session_id_serde_round_trip() {
        let id = AgentSessionId::parse_str("00000000-0000-4000-8000-000000000010").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"00000000-0000-4000-8000-000000000010\"");

        let parsed: AgentSessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, id);
        assert!(serde_json::from_str::<AgentSessionId>("\"not-a-uuid\"").is_err());
    }

    #[test]
    fn test_claude_agent_provider_descriptor_creates_metadata() {
        let provider = claude_agent_provider();
        assert_eq!(provider.name, CLAUDE_AGENT_PROVIDER);
        assert!(provider.supports_durable_session_on_start);
        assert_eq!(provider.capabilities(), claude_agent_capabilities());

        let metadata = provider.durable_session_metadata(PathBuf::from("/tmp/worktree"));
        assert_eq!(metadata.provider, CLAUDE_AGENT_PROVIDER);
        assert!(metadata.session_id.is_some());
        assert_eq!(
            metadata.transcript_cwd.as_deref(),
            Some(std::path::Path::new("/tmp/worktree"))
        );
        assert!(metadata.capabilities.is_some());

        let ephemeral = provider.ephemeral_session_metadata();
        assert_eq!(ephemeral.provider, CLAUDE_AGENT_PROVIDER);
        assert!(ephemeral.session_id.is_none());
        assert!(ephemeral.transcript_cwd.is_none());
        assert_eq!(ephemeral.capabilities, Some(claude_agent_capabilities()));
    }

    #[test]
    fn test_agent_provider_registry_contains_claude() {
        let providers = agent_provider_descriptors();
        assert_eq!(providers, &[CLAUDE_AGENT_DESCRIPTOR]);
        assert_eq!(
            agent_provider_descriptor(CLAUDE_AGENT_PROVIDER),
            Some(CLAUDE_AGENT_DESCRIPTOR)
        );
        assert_eq!(agent_provider_descriptor("unknown"), None);
    }

    #[test]
    fn test_agent_provider_descriptor_creates_resume_metadata() {
        let provider = claude_agent_provider();
        let session_id = AgentSessionId::parse_str("00000000-0000-4000-8000-000000000011").unwrap();

        let metadata = provider.resume_session_metadata(
            PathBuf::from("/tmp/worktree"),
            session_id,
            "dispatch-1",
        );

        assert_eq!(metadata.provider, CLAUDE_AGENT_PROVIDER);
        assert_eq!(metadata.session_id, Some(session_id));
        assert_eq!(
            metadata.transcript_cwd.as_deref(),
            Some(std::path::Path::new("/tmp/worktree"))
        );
        assert_eq!(
            metadata.resume_of_dispatch_id.as_deref(),
            Some("dispatch-1")
        );
        assert_eq!(metadata.capabilities, Some(claude_agent_capabilities()));
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
        assert_eq!(metadata.capabilities, Some(claude_agent_capabilities()));
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

    #[test]
    fn test_terminal_locator_tmux_json_shape() {
        let detected_at = DateTime::parse_from_rfc3339("2026-06-05T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let locator = TerminalLocator::atc_tmux(
            "session@with spaces",
            Some(PathBuf::from("/tmp/worktree")),
            detected_at,
        );
        let json = serde_json::to_value(&locator).unwrap();
        assert_eq!(json["kind"], "tmux");
        assert_eq!(json["version"], 1);
        assert_eq!(json["session"], "session@with spaces");
        assert_eq!(json["cwd"], "/tmp/worktree");
        assert_eq!(json["source"], "atc-dispatch");
        assert_eq!(json["confidence"], "exact");

        let round_trip: TerminalLocator = serde_json::from_value(json).unwrap();
        assert_eq!(round_trip, locator);
    }

    #[test]
    fn test_terminal_locator_accepts_legacy_backend_discriminator() {
        let legacy_json = serde_json::json!({
            "backend": "tmux",
            "version": 1,
            "session": "legacy-session",
            "cwd": "/tmp/worktree",
            "detected_at": "2026-06-05T00:00:00Z",
            "source": "atc-dispatch",
            "confidence": "exact"
        });

        let locator: TerminalLocator = serde_json::from_value(legacy_json).unwrap();
        let TerminalLocator::Tmux(tmux) = locator else {
            panic!("expected tmux locator");
        };
        assert_eq!(tmux.session, "legacy-session");
        assert_eq!(tmux.cwd, Some(PathBuf::from("/tmp/worktree")));
    }

    #[test]
    fn test_cloud_terminal_locator_round_trip() {
        let detected_at = DateTime::parse_from_rfc3339("2026-06-05T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let locator = TerminalLocator::atc_cloud(
            "machine-0123456789",
            Some("atc-workers".to_string()),
            Some("iad".to_string()),
            Some(PathBuf::from("/workspace/atc")),
            detected_at,
        );
        assert_eq!(locator.backend(), "cloud");
        assert_eq!(locator.cloud_worker_id(), Some("machine-0123456789"));
        assert_eq!(locator.tmux_session(), None);

        let json = serde_json::to_value(&locator).unwrap();
        assert_eq!(json["kind"], "cloud");
        assert_eq!(json["version"], 1);
        assert_eq!(json["session"], "machine-0123456789");
        assert_eq!(json["app"], "atc-workers");
        assert_eq!(json["region"], "iad");

        let round_trip: TerminalLocator = serde_json::from_value(json).unwrap();
        assert_eq!(round_trip, locator);
    }

    #[test]
    fn test_cloud_terminal_locator_rejects_unsupported_version() {
        let json = serde_json::json!({
            "kind": "cloud",
            "version": 2,
            "session": "machine-1",
            "detected_at": "2026-06-05T00:00:00Z",
            "source": "atc-dispatch",
            "confidence": "exact"
        });

        let error = serde_json::from_value::<TerminalLocator>(json)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported cloud terminal locator version: 2"));
    }

    #[test]
    fn test_terminal_locator_rejects_conflicting_discriminators() {
        let json = serde_json::json!({
            "kind": "tmux",
            "backend": "terminal-app",
            "version": 1,
            "session": "session",
            "detected_at": "2026-06-05T00:00:00Z",
            "source": "atc-dispatch",
            "confidence": "exact"
        });

        let error = serde_json::from_value::<TerminalLocator>(json)
            .unwrap_err()
            .to_string();
        assert!(error.contains("kind/backend mismatch"));
    }

    #[test]
    fn test_terminal_locator_rejects_unsupported_tmux_version() {
        let json = serde_json::json!({
            "kind": "tmux",
            "version": 2,
            "session": "session",
            "detected_at": "2026-06-05T00:00:00Z",
            "source": "atc-dispatch",
            "confidence": "exact"
        });

        let error = serde_json::from_value::<TerminalLocator>(json)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported tmux terminal locator version: 2"));
    }

    #[test]
    fn test_terminal_locator_rejects_empty_tmux_session() {
        let json = serde_json::json!({
            "kind": "tmux",
            "version": 1,
            "session": "  ",
            "detected_at": "2026-06-05T00:00:00Z",
            "source": "atc-dispatch",
            "confidence": "exact"
        });

        let error = serde_json::from_value::<TerminalLocator>(json)
            .unwrap_err()
            .to_string();
        assert!(error.contains("tmux terminal locator session is empty"));
    }

    #[test]
    fn test_terminal_locator_reports_unsupported_kind_before_tmux_fields() {
        let json = serde_json::json!({
            "kind": "terminal-app",
            "version": 1,
            "window_id": "123",
            "detected_at": "2026-06-05T00:00:00Z"
        });

        let error = serde_json::from_value::<TerminalLocator>(json)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported terminal locator kind: terminal-app"));
        assert!(!error.contains("missing field `session`"));
    }

    #[test]
    fn test_terminal_locator_errors_escape_hostile_discriminators() {
        let json = serde_json::json!({
            "kind": "tmux\u{1b}[2J",
            "version": 1,
            "session": "bad",
            "detected_at": "2026-06-05T00:00:00Z",
            "source": "atc-dispatch",
            "confidence": "exact"
        });

        let error = serde_json::from_value::<TerminalLocator>(json)
            .unwrap_err()
            .to_string();
        assert!(error.contains("tmux\\x1b[2J"));
        assert!(!error.contains('\u{1b}'));
    }

    #[test]
    fn test_terminal_status_openable_only_for_explicit_live_states() {
        for state in [
            TerminalStatusState::Focusable,
            TerminalStatusState::Attached,
            TerminalStatusState::Detached,
            TerminalStatusState::Running,
        ] {
            assert!(
                TerminalStatus::new(state, Some("tmux")).is_openable(),
                "{state:?} should be openable"
            );
        }

        for state in [
            TerminalStatusState::Stale,
            TerminalStatusState::Unavailable,
            TerminalStatusState::Unknown,
        ] {
            assert!(
                !TerminalStatus::new(state, Some("tmux")).is_openable(),
                "{state:?} should not be openable"
            );
        }
    }

    #[test]
    fn test_atc_session_uri_percent_encodes_dispatch_id() {
        let id = "branch/with space@implement@1";
        let uri = atc_session_uri(id);
        assert_eq!(uri, "atc://session/branch%2Fwith%20space%40implement%401");
        assert_eq!(parse_atc_session_uri(&uri).unwrap(), id);
    }

    #[test]
    fn test_atc_session_uri_rejects_invalid_inputs() {
        assert!(parse_atc_session_uri("https://example.invalid/dispatch").is_err());
        assert!(parse_atc_session_uri("atc://dispatch/id").is_err());
        assert!(parse_atc_session_uri("atc://session/").is_err());
        assert!(parse_atc_session_uri("atc://session/%").is_err());
        assert!(parse_atc_session_uri("atc://session/%GG").is_err());
        assert!(parse_atc_session_uri("atc://session/id/extra").is_err());
        assert!(parse_atc_session_uri("atc://session/id@example").is_err());
        assert!(parse_atc_session_uri("atc://session/%FF").is_err());
        assert!(parse_atc_session_uri("atc://session/%1Bbad").is_err());
        assert!(parse_atc_session_uri("atc://session/%E2%80%AEgpj.exe").is_err());
        assert!(parse_atc_session_uri("atc://session/%E2%80%8Bhidden").is_err());
        assert!(parse_atc_session_uri("atc://session/%E2%81%A0hidden").is_err());
        assert!(parse_atc_session_uri("atc://session/%EF%BB%BFhidden").is_err());
        assert!(parse_atc_session_uri("atc://session/%E2%80%A8hidden").is_err());
    }

    #[test]
    fn test_atc_session_uri_errors_do_not_echo_raw_terminal_controls() {
        for value in [
            "atc://dispatch/id\x1b[2J",
            "atc://session/id\x1b[2J",
            "atc://session/%\x1b0",
            "atc://session/id\u{202e}gpj.exe",
        ] {
            let error = parse_atc_session_uri(value).unwrap_err().to_string();
            assert!(
                !error.contains('\x1b'),
                "raw escape leaked in error for {value:?}: {error:?}"
            );
            assert!(
                !error.contains('\u{202e}'),
                "raw bidi control leaked in error for {value:?}: {error:?}"
            );
        }

        let reserved = parse_atc_session_uri("atc://session/id\x1b[2J")
            .unwrap_err()
            .to_string();
        assert!(reserved.contains("0x1B"));

        let invalid_escape = parse_atc_session_uri("atc://session/%\x1b0")
            .unwrap_err()
            .to_string();
        assert!(invalid_escape.contains("0x1B"));
    }

    #[test]
    fn test_required_enum_parse_errors_escape_terminal_controls() {
        for error in [
            "running\x1b[2J\u{202e}gpj".parse::<Status>().unwrap_err(),
            "implement\x1b[2J\u{202e}gpj"
                .parse::<Directive>()
                .unwrap_err(),
            "branch\x1b[2J\u{202e}gpj"
                .parse::<WorktreePolicy>()
                .unwrap_err(),
            "active\x1b[2J\u{202e}gpj"
                .parse::<WorkUnitStatus>()
                .unwrap_err(),
        ] {
            let error = error.to_string();
            assert!(error.contains("\\x1b[2J\\u{202e}gpj"));
            assert!(!error.contains('\x1b'));
            assert!(!error.contains('\u{202e}'));
        }
    }
}
