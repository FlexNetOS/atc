use crate::source::SourceConfig;
use crate::terminal_text::display_text;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Top-level ATC configuration. Loaded from TOML file.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AtcConfig {
    /// Directory containing the config file that was loaded.
    /// Used to resolve relative paths in DispatchConfig.
    #[serde(skip)]
    pub config_dir: Option<PathBuf>,

    /// Whether config was loaded from `.atc/config.toml` (true) vs legacy `atc.toml` (false).
    #[serde(skip)]
    pub atc_dir_mode: bool,

    #[serde(default)]
    pub registry: RegistryConfig,
    #[serde(default)]
    pub dispatch: DispatchConfig,
    #[serde(default)]
    pub batch: BatchConfig,
    #[serde(default)]
    pub health: HealthConfig,
    #[serde(default)]
    pub notifications: Option<NotificationsConfig>,
    #[serde(default)]
    pub watch: WatchConfig,
    /// Prompt engine configuration (components, templates, partials directories).
    #[serde(default)]
    pub prompt: PromptConfig,
    /// Per-directive template overrides. Keys are directive names (e.g. "implement", "review-fix").
    #[serde(default, alias = "modes")]
    pub directives: HashMap<String, DirectiveConfig>,
    /// Resolver chain configuration.
    #[serde(default)]
    pub resolvers: ResolversConfig,
    /// Search path configuration for `.atc/` directory resolution.
    #[serde(default)]
    pub paths: PathsConfig,
    /// Daemon configuration.
    #[serde(default)]
    pub daemon: DaemonConfig,
    /// Named source configurations for daemon auto-feed.
    #[serde(default)]
    pub sources: HashMap<String, SourceConfig>,
    /// Pager configuration for human-facing CLI output.
    #[serde(default)]
    pub pager: PagerConfig,
    /// Cloud ATC configuration (remote Fly worker executor + Postgres registry).
    #[serde(default)]
    pub cloud: CloudConfig,
}

/// `[pager]` section — controls pager program for long human-facing output.
///
/// Precedence (Plain class, used by status/history/info/health/logs):
/// 1. `ATC_PAGER` env var
/// 2. `pager.plain` in config.toml
/// 3. `PAGER` env var
/// 4. Built-in default: `less -R +G`
///
/// Set to empty string or `cat` to disable.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PagerConfig {
    /// Pager command for plain/tabular output. Default: `less -R +G`.
    #[serde(default)]
    pub plain: Option<String>,
}

/// `[resolvers]` section — controls resolver order and per-resolver settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResolversConfig {
    /// Ordered list of resolver names. Default: ["task", "template", "prompt"].
    #[serde(default = "default_resolver_order")]
    pub order: Vec<String>,
    /// Per-resolver settings.
    #[serde(default)]
    pub task: ResolverEntryConfig,
    #[serde(default)]
    pub template: ResolverEntryConfig,
    #[serde(default)]
    pub prompt: ResolverEntryConfig,
}

fn default_resolver_order() -> Vec<String> {
    vec![
        "task".to_string(),
        "template".to_string(),
        "prompt".to_string(),
    ]
}

impl Default for ResolversConfig {
    fn default() -> Self {
        Self {
            order: default_resolver_order(),
            task: ResolverEntryConfig::default(),
            template: ResolverEntryConfig::default(),
            prompt: ResolverEntryConfig::default(),
        }
    }
}

/// Per-resolver toggle.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResolverEntryConfig {
    /// Whether this resolver is enabled. Default: true.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for ResolverEntryConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl AtcConfig {
    fn parse_and_validate(contents: &str) -> anyhow::Result<Self> {
        let cfg: Self = toml::from_str(contents)?;
        anyhow::ensure!(
            cfg.health.signal_timeout_secs > 0,
            "health.signal_timeout_secs must be >= 1"
        );
        anyhow::ensure!(
            cfg.batch.max_concurrency > 0,
            "batch.max_concurrency must be >= 1"
        );
        anyhow::ensure!(
            cfg.dispatch.max_turns > 0,
            "dispatch.max_turns must be >= 1"
        );
        anyhow::ensure!(
            cfg.dispatch.max_budget_usd > 0.0 && cfg.dispatch.max_budget_usd.is_finite(),
            "dispatch.max_budget_usd must be a positive finite number"
        );
        anyhow::ensure!(
            cfg.dispatch.max_retries > 0,
            "dispatch.max_retries must be >= 1"
        );
        anyhow::ensure!(
            cfg.daemon.drain_interval_secs > 0,
            "daemon.drain_interval_secs must be >= 1"
        );
        anyhow::ensure!(
            cfg.daemon.max_concurrent > 0,
            "daemon.max_concurrent must be >= 1"
        );
        anyhow::ensure!(
            cfg.watch.poll_interval_secs > 0,
            "watch.poll_interval_secs must be >= 1"
        );
        anyhow::ensure!(
            cfg.watch.cost_threshold.is_finite() && cfg.watch.cost_threshold >= 0.0,
            "watch.cost_threshold must be a finite non-negative number"
        );
        anyhow::ensure!(
            cfg.health.cost_warning_threshold.is_finite()
                && cfg.health.cost_warning_threshold >= 0.0,
            "health.cost_warning_threshold must be a finite non-negative number"
        );
        anyhow::ensure!(
            cfg.cloud.liveness_ttl_secs > 0,
            "cloud.liveness_ttl_secs must be >= 1"
        );
        // Validate source poll intervals
        for (name, source) in &cfg.sources {
            anyhow::ensure!(
                source.poll_interval_secs() > 0,
                "sources.{}.poll_interval_secs must be >= 1",
                display_text(name)
            );
        }
        // Validate directive keys against known Directive variants + per-directive overrides
        for (key, directive_cfg) in &cfg.directives {
            Self::validate_directive(key, directive_cfg)?;
        }
        // Validate resolver order — warn on unknown resolver names (typos)
        let known_resolvers = ["task", "template", "prompt"];
        for name in &cfg.resolvers.order {
            anyhow::ensure!(
                known_resolvers.contains(&name.as_str()),
                "unknown resolver '{}' in resolvers.order; valid resolvers: {}",
                display_text(name),
                known_resolvers.join(", ")
            );
        }
        Ok(cfg)
    }

    /// Load config using resolution order:
    /// 1. `--config <path>` CLI flag (passed as argument)
    /// 2. `ATC_CONFIG` environment variable
    /// 3. Walk up from CWD looking for `.atc/config.toml` or `atc.toml` (project-level discovery)
    /// 4. `~/.config/atc/config.toml` (XDG user config)
    ///
    /// Returns default config if no file is found.
    pub fn load(config_path: Option<&std::path::Path>) -> anyhow::Result<Self> {
        // 1. Explicit path from CLI flag
        if let Some(path) = config_path {
            let path = expand_tilde(path);
            let contents = std::fs::read_to_string(&path)?;
            let mut cfg = Self::parse_and_validate(&contents)?;
            cfg.apply_file_context(&path);
            return Ok(cfg);
        }

        // 2. ATC_CONFIG env var (error if set but missing, matching --config behavior)
        if let Ok(env_path) = std::env::var("ATC_CONFIG") {
            let path = expand_tilde(Path::new(&env_path));
            let contents = std::fs::read_to_string(&path).map_err(|e| {
                anyhow::anyhow!(
                    "ATC_CONFIG={} is set but file cannot be read: {}",
                    env_path,
                    e
                )
            })?;
            let mut cfg = Self::parse_and_validate(&contents)?;
            cfg.apply_file_context(&path);
            return Ok(cfg);
        }

        // 3. Walk up from CWD looking for atc.toml
        if let Ok(start) = std::env::current_dir() {
            if let Some(cfg) = Self::find_config_upward(&start) {
                return Ok(cfg);
            }
        }

        // 4. XDG config path ($XDG_CONFIG_HOME/atc/config.toml, fallback ~/.config)
        let xdg_path = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir().join(".config"))
            .join("atc/config.toml");
        if xdg_path.exists() {
            let contents = std::fs::read_to_string(&xdg_path)?;
            let mut cfg = Self::parse_and_validate(&contents)?;
            cfg.config_dir = xdg_path.parent().map(|p| p.to_path_buf());
            return Ok(cfg);
        }

        Ok(Self::default())
    }

    /// Walk up from `start` looking for `.atc/config.toml` or `atc.toml`.
    /// Prefers `.atc/config.toml` at each directory level, falling back to
    /// `atc.toml`. Returns the first valid config found, or `None` if no
    /// config is discovered before reaching the filesystem root.
    fn find_config_upward(start: &Path) -> Option<Self> {
        let mut dir = Some(start.to_path_buf());
        while let Some(d) = dir {
            // Try .atc/config.toml first, then atc.toml
            for candidate in [d.join(".atc/config.toml"), d.join("atc.toml")] {
                match std::fs::read_to_string(&candidate) {
                    Ok(contents) => match Self::parse_and_validate(&contents) {
                        Ok(mut cfg) => {
                            cfg.apply_file_context(&candidate);
                            return Some(cfg);
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Ignoring malformed config at {}: {}",
                                candidate.display(),
                                e
                            );
                        }
                    },
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
                    Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => {}
                    Err(e) => {
                        tracing::warn!(
                            "Unexpected I/O error reading {}: {e}; skipping",
                            candidate.display()
                        );
                    }
                }
            }
            dir = d.parent().map(|p| p.to_path_buf());
        }
        None
    }

    /// Check if a config file path looks like `.atc/config.toml`.
    fn is_atc_dir_config(path: &Path) -> bool {
        path.file_name().and_then(|f| f.to_str()) == Some("config.toml")
            && path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|f| f.to_str())
                == Some(".atc")
    }

    /// Apply `.atc/` directory post-processing to a freshly-parsed config.
    ///
    /// Sets `config_dir` (project root for `.atc/config.toml`, parent dir for
    /// legacy `atc.toml`), `atc_dir_mode`, and loads directive files + defaults
    /// when in `.atc/` mode. Call this after `parse_and_validate` for any config
    /// loaded from a file path.
    fn apply_file_context(&mut self, path: &Path) {
        let is_atc_dir = Self::is_atc_dir_config(path);
        self.config_dir = if is_atc_dir {
            // .atc/config.toml → config_dir is project root (parent of .atc/)
            path.parent()
                .and_then(|p| p.parent())
                .map(|p| p.to_path_buf())
        } else {
            path.parent().map(|p| p.to_path_buf())
        };
        self.atc_dir_mode = is_atc_dir;
        if is_atc_dir {
            self.load_directive_files();
        }
    }

    /// Validate a single directive entry (name + config).
    /// Used by both `parse_and_validate` and `load_directive_files`.
    fn validate_directive(key: &str, directive_cfg: &DirectiveConfig) -> anyhow::Result<()> {
        let display_key = display_text(key);
        key.parse::<crate::types::Directive>().map_err(|_| {
            anyhow::anyhow!(
                "unknown directive '{}' in [directives.{}]; valid directives: implement, research, kb-update, review-fix, pr-comments, refine, create-task, close",
                display_key, display_key,
            )
        })?;
        if let Some(components) = &directive_cfg.components {
            anyhow::ensure!(
                !components.is_empty(),
                "directives.{}.components must contain at least one component name",
                display_key
            );
            for name in components {
                anyhow::ensure!(
                    !name.trim().is_empty(),
                    "directives.{}.components contains an empty component name",
                    display_key
                );
                anyhow::ensure!(
                    !name.contains('/') && !name.contains('\\') && !name.contains(".."),
                    "directives.{}.components contains an invalid component name '{}': must not contain '/', '\\', or '..'",
                    display_key,
                    display_text(name)
                );
            }
        }
        if let Some(budget) = directive_cfg.max_budget_usd {
            anyhow::ensure!(
                budget > 0.0 && budget.is_finite(),
                "directives.{}.max_budget_usd must be a positive finite number",
                display_key
            );
        }
        if let Some(turns) = directive_cfg.max_turns {
            anyhow::ensure!(
                turns > 0,
                "directives.{}.max_turns must be >= 1",
                display_key
            );
        }
        if let Some(providers) = &directive_cfg.providers {
            for name in providers {
                anyhow::ensure!(
                    crate::providers::KNOWN_PROVIDERS.contains(&name.as_str()),
                    "unknown provider '{}' in directives.{}.providers; valid providers: {}",
                    display_text(name),
                    display_key,
                    crate::providers::KNOWN_PROVIDERS.join(", ")
                );
            }
        }
        Ok(())
    }

    /// Load directive config files from `.atc/directives/*.toml`.
    /// File-based directives are loaded first; then `[directives.*]` from
    /// config.toml overrides them (config takes priority).
    fn load_directive_files(&mut self) {
        let Some(ref config_dir) = self.config_dir else {
            return;
        };
        let directives_dir = config_dir.join(".atc/directives");
        let entries = match std::fs::read_dir(&directives_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        // Save config-level overrides so they take priority
        let config_overrides = self.directives.clone();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let directive_name = stem.to_string();
            // Skip if config.toml already has this directive (config overrides files)
            if config_overrides.contains_key(&directive_name) {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(contents) => match toml::from_str::<DirectiveConfig>(&contents) {
                    Ok(dcfg) => {
                        if let Err(e) = Self::validate_directive(&directive_name, &dcfg) {
                            tracing::warn!(
                                "Ignoring invalid directive file {}: {}",
                                path.display(),
                                e
                            );
                            continue;
                        }
                        self.directives.insert(directive_name, dcfg);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Ignoring malformed directive file {}: {}",
                            path.display(),
                            e
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!("Cannot read directive file {}: {}", path.display(), e);
                }
            }
        }
    }
}

/// `[registry]` section
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RegistryConfig {
    /// Path to SQLite DB file. Supports ~ expansion.
    /// Default: ~/.local/share/atc/registry.db
    pub path: Option<PathBuf>,
}

impl RegistryConfig {
    /// Resolve effective DB path: config value or default under ATC_ROOT.
    pub fn resolved_path(&self) -> PathBuf {
        if let Some(ref p) = self.path {
            return expand_tilde(p);
        }
        let root = std::env::var("ATC_ROOT")
            .map(|p| expand_tilde(Path::new(&p)))
            .unwrap_or_else(|_| home_dir().join(".local/share/atc"));
        root.join("registry.db")
    }
}

/// `[dispatch]` section
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchConfig {
    /// Repo alias passed to `meta git worktree create --repo`. Required.
    pub repo: Option<String>,
    /// Base directory for all worktrees (META_WORKTREES env var).
    /// Default: "/tmp/worktrees". Supports ~ expansion.
    pub worktree_base: Option<PathBuf>,
    /// Directory containing the `.meta.yaml` that governs the sub-workspace.
    /// Used as `cwd` when invoking `meta git worktree create`.
    /// Default: "." → resolved relative to the ATC config file's parent dir.
    /// Supports ~ expansion.
    pub meta_workspace_root: Option<PathBuf>,
    /// Directory where stream-json log files are written.
    /// Default: ~/.local/share/atc/logs/
    pub log_dir: Option<PathBuf>,
    /// Path to the `claude` binary. Default: "claude" (found via $PATH).
    pub claude_bin: Option<PathBuf>,
    /// false = write sandbox settings JSON and pass --settings to claude.
    /// Default: false (sandbox disabled, matches dispatch.sh).
    #[serde(default)]
    pub sandbox: bool,
    /// --max-turns flag for claude. Default: 10000.
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    /// --max-budget-usd flag for claude. Default: 25.0.
    #[serde(default = "default_max_budget_usd")]
    pub max_budget_usd: f64,
    /// Maximum number of retries before marking a task as needs-human. Default: 3.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Whether to load `.dispatch/env` from the worktree. Default: true.
    #[serde(default = "default_true")]
    pub project_env: bool,
}

fn default_max_turns() -> u32 {
    10_000
}
fn default_max_budget_usd() -> f64 {
    25.0
}
fn default_max_retries() -> u32 {
    3
}

impl Default for DispatchConfig {
    fn default() -> Self {
        Self {
            repo: None,
            worktree_base: None,
            meta_workspace_root: None,
            log_dir: None,
            claude_bin: None,
            sandbox: false,
            max_turns: default_max_turns(),
            max_budget_usd: default_max_budget_usd(),
            max_retries: default_max_retries(),
            project_env: true,
        }
    }
}

impl DispatchConfig {
    /// Resolve effective log directory: config value or default under ATC_ROOT.
    pub fn resolved_log_dir(&self) -> PathBuf {
        if let Some(ref p) = self.log_dir {
            return expand_tilde(p);
        }
        let root = std::env::var("ATC_ROOT")
            .map(|p| expand_tilde(Path::new(&p)))
            .unwrap_or_else(|_| home_dir().join(".local/share/atc"));
        root.join("logs")
    }

    /// Resolve effective claude binary path: config value or "claude".
    pub fn resolved_claude_bin(&self) -> PathBuf {
        self.claude_bin
            .as_ref()
            .map(|p| expand_tilde(p))
            .unwrap_or_else(|| PathBuf::from("claude"))
    }

    /// Resolve effective worktree base directory.
    /// Default: "/tmp/worktrees". Supports ~ expansion.
    pub fn resolved_worktree_base(&self) -> PathBuf {
        self.worktree_base
            .as_ref()
            .map(|p| expand_tilde(p))
            .unwrap_or_else(|| PathBuf::from("/tmp/worktrees"))
    }

    /// Resolve the repo alias. Returns None if not configured (triggers auto-discovery).
    pub fn resolved_repo(&self) -> Option<&str> {
        self.repo.as_deref()
    }

    /// Resolve meta_workspace_root to an absolute, canonicalized path.
    /// If relative or ".", resolves relative to `config_dir`.
    /// If `config_dir` is None, resolves relative to CWD.
    pub fn resolved_meta_workspace_root(
        &self,
        config_dir: Option<&Path>,
    ) -> anyhow::Result<PathBuf> {
        let raw = self
            .meta_workspace_root
            .as_ref()
            .map(|p| expand_tilde(p))
            .unwrap_or_else(|| PathBuf::from("."));

        let absolute = if raw.is_absolute() {
            raw
        } else {
            let base = config_dir.unwrap_or_else(|| Path::new("."));
            base.join(&raw)
        };

        let canonical = std::fs::canonicalize(&absolute).map_err(|e| {
            anyhow::anyhow!(
                "cannot resolve meta_workspace_root '{}': {}",
                absolute.display(),
                e
            )
        })?;

        anyhow::ensure!(
            canonical.file_name().is_some(),
            "meta_workspace_root must not be the filesystem root"
        );

        Ok(canonical)
    }
}

/// `[health]` section
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HealthConfig {
    /// Timeout in seconds per subprocess call per signal per record.
    /// Default: 30.
    #[serde(default = "default_signal_timeout_secs")]
    pub signal_timeout_secs: u64,
    /// When true, auto-dispatch review-fix for NeedsReview records even
    /// without the `--auto` CLI flag. Default: false.
    #[serde(default)]
    pub auto_review: bool,
    /// Print a warning when a dispatch's cost exceeds this threshold (USD).
    /// Default: 10.0.
    #[serde(default = "default_cost_warning_threshold")]
    pub cost_warning_threshold: f64,
}

fn default_signal_timeout_secs() -> u64 {
    30
}

fn default_cost_warning_threshold() -> f64 {
    10.0
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            signal_timeout_secs: default_signal_timeout_secs(),
            auto_review: false,
            cost_warning_threshold: default_cost_warning_threshold(),
        }
    }
}

/// `[cloud]` section — configures the Cloud ATC vertical slice: the remote
/// Fly-Machine executor, NATS output routing, and the Postgres registry.
///
/// When `enabled` is true, `atc run` selects `RemoteExecutor` + `PgRegistry`
/// instead of the local `ClaudeExecutor` + `SqliteRegistry`. All fields fall
/// back to environment variables so secrets need not live in the config file.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CloudConfig {
    /// Master switch. When false, the cloud path is never selected.
    #[serde(default)]
    pub enabled: bool,
    /// Fly app the ephemeral worker Machines are created under.
    #[serde(default)]
    pub fly_app: Option<String>,
    /// Container image the worker Machine boots (e.g. `registry.fly.io/atc-worker:latest`).
    #[serde(default)]
    pub fly_image: Option<String>,
    /// Fly region for the worker Machine (e.g. `iad`). None lets Fly choose.
    #[serde(default)]
    pub fly_region: Option<String>,
    /// Path to the `fly`/`flyctl` binary. Default: `fly` (found via $PATH).
    #[serde(default)]
    pub fly_bin: Option<PathBuf>,
    /// Name of the warm volume holding the bare mirror of the target repo.
    /// The worker Machine forks this volume at create time.
    #[serde(default)]
    pub worker_volume: Option<String>,
    /// NATS server URL the worker streams stream-json events to and the control
    /// plane consumes from. Falls back to `$NATS_URL`, then `nats://127.0.0.1:4222`.
    #[serde(default)]
    pub nats_url: Option<String>,
    /// Path to the `nats` CLI binary. Default: `nats` (found via $PATH).
    #[serde(default)]
    pub nats_bin: Option<PathBuf>,
    /// Subject prefix for per-dispatch event streams. Default: `atc.dispatch`.
    /// The full subject is `<prefix>.<dispatch_id>.events`.
    #[serde(default)]
    pub nats_subject_prefix: Option<String>,
    /// Postgres connection URL for the registry. Falls back to `$DATABASE_URL`.
    #[serde(default)]
    pub database_url: Option<String>,
    /// Git remote (e.g. `https://github.com/gitkb/atc.git`) the worker mirrors
    /// and opens a PR against.
    #[serde(default)]
    pub repo_remote: Option<String>,
    /// Cloud liveness TTL (seconds) for health Signal 1: a cloud dispatch whose
    /// re-materialized log has not advanced within this window is treated as
    /// exited. Default: 120.
    #[serde(default = "default_cloud_liveness_ttl_secs")]
    pub liveness_ttl_secs: u64,
}

fn default_cloud_liveness_ttl_secs() -> u64 {
    120
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fly_app: None,
            fly_image: None,
            fly_region: None,
            fly_bin: None,
            worker_volume: None,
            nats_url: None,
            nats_bin: None,
            nats_subject_prefix: None,
            database_url: None,
            repo_remote: None,
            liveness_ttl_secs: default_cloud_liveness_ttl_secs(),
        }
    }
}

impl CloudConfig {
    pub fn resolved_fly_bin(&self) -> PathBuf {
        self.fly_bin
            .as_ref()
            .map(|p| expand_tilde(p))
            .unwrap_or_else(|| PathBuf::from("fly"))
    }

    pub fn resolved_nats_bin(&self) -> PathBuf {
        self.nats_bin
            .as_ref()
            .map(|p| expand_tilde(p))
            .unwrap_or_else(|| PathBuf::from("nats"))
    }

    /// NATS URL: config value, then `$NATS_URL`, then the local default.
    pub fn resolved_nats_url(&self) -> String {
        if let Some(ref url) = self.nats_url {
            return url.clone();
        }
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".to_string())
    }

    /// Subject prefix for per-dispatch event streams.
    pub fn resolved_subject_prefix(&self) -> String {
        self.nats_subject_prefix
            .clone()
            .unwrap_or_else(|| "atc.dispatch".to_string())
    }

    /// Full NATS subject for a dispatch's stream-json events.
    pub fn subject_for(&self, dispatch_id: &str) -> String {
        format!("{}.{}.events", self.resolved_subject_prefix(), dispatch_id)
    }

    /// Postgres connection URL: config value, then `$DATABASE_URL`.
    pub fn resolved_database_url(&self) -> Option<String> {
        self.database_url
            .clone()
            .or_else(|| std::env::var("DATABASE_URL").ok())
    }

    pub fn liveness_ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.liveness_ttl_secs)
    }
}

/// `[batch]` section
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BatchConfig {
    /// Maximum concurrent dispatches. Default: 3.
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: usize,
}

fn default_max_concurrency() -> usize {
    3
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            max_concurrency: default_max_concurrency(),
        }
    }
}

/// Per-directive template override configuration.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DirectiveConfig {
    /// Path to a template file on disk. Supports `~` expansion.
    pub template_path: Option<String>,
    /// Inline template string. Ignored if `template_path` is also set.
    pub template_inline: Option<String>,
    /// Per-directive budget override (USD). Takes precedence over global dispatch.max_budget_usd.
    pub max_budget_usd: Option<f64>,
    /// Per-directive turns override. Takes precedence over global dispatch.max_turns.
    pub max_turns: Option<u32>,
    /// Ordered list of component names. Each name maps to `<components_dir>/<name>.md`.
    /// When set, the system prompt is assembled by concatenating these components.
    #[serde(default)]
    pub components: Option<Vec<String>>,
    /// Ordered list of context provider names to run before agent spawn.
    /// Valid names: "pr-context", "kb-context", "rebase".
    #[serde(default)]
    pub providers: Option<Vec<String>>,
}

/// `[prompt]` section — paths to prompt components, templates, and partials.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PromptConfig {
    /// Directory containing component `.md` files.
    /// Default: `.atc/components`
    #[serde(default = "default_components_dir")]
    pub components_dir: String,
    /// Directory containing template `.md` files.
    /// Default: `.atc/templates`
    #[serde(default = "default_templates_dir")]
    pub templates_dir: String,
    /// Directory containing partial `.md` files.
    /// Default: `.atc/partials`
    #[serde(default = "default_partials_dir")]
    pub partials_dir: String,
}

fn default_components_dir() -> String {
    ".atc/components".to_string()
}
fn default_templates_dir() -> String {
    ".atc/templates".to_string()
}
fn default_partials_dir() -> String {
    ".atc/partials".to_string()
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            components_dir: default_components_dir(),
            templates_dir: default_templates_dir(),
            partials_dir: default_partials_dir(),
        }
    }
}

/// `[notifications]` section
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NotificationsConfig {
    /// Enable macOS notifications. Default: true.
    #[serde(default = "default_true")]
    pub macos: bool,
    /// Webhook URL for POST notifications. Empty = disabled.
    pub webhook_url: Option<String>,
}

fn default_true() -> bool {
    true
}

/// `[watch]` section
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WatchConfig {
    /// Tmux session check interval in seconds. Default: 5.
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// Emit CostThreshold event at this USD level. Default: 10.0.
    #[serde(default = "default_cost_threshold")]
    pub cost_threshold: f64,
}

fn default_poll_interval_secs() -> u64 {
    5
}
fn default_cost_threshold() -> f64 {
    10.0
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_poll_interval_secs(),
            cost_threshold: default_cost_threshold(),
        }
    }
}

/// `[paths]` section — search path for `.atc/` directory resolution.
/// Project-local `.atc/` is always checked first; these paths provide fallbacks.
///
/// **Note:** `search_path` is parsed and persisted but not yet consumed at runtime.
/// It is reserved for a future multi-directory lookup feature.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PathsConfig {
    /// Additional directories to search for components, templates, and directives.
    /// Project-local `.atc/` takes priority, then these paths in order.
    /// Default: empty (project-local `.atc/` only).
    #[serde(default)]
    pub search_path: Vec<String>,
}

/// `[daemon]` section
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DaemonConfig {
    /// Queue drain interval in seconds. Default: 1.
    #[serde(default = "default_drain_interval_secs")]
    pub drain_interval_secs: u64,
    /// Maximum concurrent dispatches across all queues. Default: 5.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// Graceful shutdown timeout in seconds. Default: 300 (5 minutes).
    #[serde(default = "default_graceful_shutdown_timeout_secs")]
    pub graceful_shutdown_timeout_secs: u64,
    /// PID file path. Default: ~/.local/share/atc/daemon.pid.
    pub pid_file: Option<PathBuf>,
}

fn default_drain_interval_secs() -> u64 {
    1
}

fn default_max_concurrent() -> usize {
    5
}

fn default_graceful_shutdown_timeout_secs() -> u64 {
    300
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            drain_interval_secs: default_drain_interval_secs(),
            max_concurrent: default_max_concurrent(),
            graceful_shutdown_timeout_secs: default_graceful_shutdown_timeout_secs(),
            pid_file: None,
        }
    }
}

impl DaemonConfig {
    /// Resolve effective PID file path.
    pub fn resolved_pid_file(&self, config_dir: Option<&Path>) -> PathBuf {
        if let Some(ref p) = self.pid_file {
            let p = expand_tilde(p);
            return if p.is_absolute() {
                p
            } else if let Some(dir) = config_dir {
                dir.join(p)
            } else {
                p
            };
        }
        let root = std::env::var("ATC_ROOT")
            .map(|p| expand_tilde(Path::new(&p)))
            .unwrap_or_else(|_| home_dir().join(".local/share/atc"));
        root.join("daemon.pid")
    }
}

/// Returns the user's home directory, falling back to `/tmp` if `HOME` is unset
/// (e.g., in containers/CI) to avoid producing relative paths from an empty string.
fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

pub fn expand_tilde(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if s == "~" {
        home_dir()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        p.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let cfg = AtcConfig::default();
        assert_eq!(cfg.batch.max_concurrency, 3);
        assert!(cfg.registry.path.is_none());
        assert!(cfg.dispatch.log_dir.is_none());
        assert!(cfg.dispatch.claude_bin.is_none());
        assert!(!cfg.dispatch.sandbox);
        assert!(cfg.dispatch.repo.is_none());
        assert!(cfg.dispatch.worktree_base.is_none());
        assert!(cfg.dispatch.meta_workspace_root.is_none());
        assert_eq!(cfg.dispatch.max_turns, 10_000);
        assert_eq!(cfg.dispatch.max_budget_usd, 25.0);
    }

    #[test]
    fn test_parse_valid_toml() {
        let toml = r#"
[batch]
max_concurrency = 5

[registry]
path = "/tmp/test.db"

[dispatch]
repo = "core"
sandbox = true
max_turns = 5000
max_budget_usd = 10.0
"#;
        let cfg = AtcConfig::parse_and_validate(toml).unwrap();
        assert_eq!(cfg.batch.max_concurrency, 5);
        assert_eq!(
            cfg.registry.path.as_deref(),
            Some(Path::new("/tmp/test.db"))
        );
        assert!(cfg.dispatch.sandbox);
        assert_eq!(cfg.dispatch.repo.as_deref(), Some("core"));
        assert_eq!(cfg.dispatch.max_turns, 5000);
        assert_eq!(cfg.dispatch.max_budget_usd, 10.0);
    }

    #[test]
    fn test_parse_empty_toml_uses_defaults() {
        let cfg = AtcConfig::parse_and_validate("").unwrap();
        assert_eq!(cfg.batch.max_concurrency, 3);
        assert!(cfg.registry.path.is_none());
        assert_eq!(cfg.dispatch.max_turns, 10_000);
        assert_eq!(cfg.dispatch.max_budget_usd, 25.0);
    }

    #[test]
    fn test_health_config_defaults() {
        let cfg = AtcConfig::default();
        assert_eq!(cfg.health.signal_timeout_secs, 30);
    }

    #[test]
    fn test_health_config_from_toml() {
        let toml = "[health]\nsignal_timeout_secs = 60";
        let cfg = AtcConfig::parse_and_validate(toml).unwrap();
        assert_eq!(cfg.health.signal_timeout_secs, 60);
    }

    #[test]
    fn test_parse_rejects_zero_signal_timeout() {
        let toml = "[health]\nsignal_timeout_secs = 0";
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string().contains("signal_timeout_secs must be >= 1"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_rejects_zero_concurrency() {
        let toml = "[batch]\nmax_concurrency = 0";
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string().contains("max_concurrency must be >= 1"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_rejects_invalid_toml() {
        let err = AtcConfig::parse_and_validate("not valid toml {{{}").unwrap_err();
        assert!(
            err.to_string().contains("expected"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_load_explicit_path() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("test.toml");
        std::fs::write(&config_path, "[batch]\nmax_concurrency = 7").unwrap();
        let cfg = AtcConfig::load(Some(&config_path)).unwrap();
        assert_eq!(cfg.batch.max_concurrency, 7);
        assert_eq!(cfg.config_dir.as_deref(), Some(dir.path()));
    }

    #[test]
    fn test_load_missing_explicit_path_errors() {
        let err = AtcConfig::load(Some(Path::new("/tmp/nonexistent-atc-config.toml"))).unwrap_err();
        assert!(
            err.to_string().contains("No such file")
                || err.to_string().contains("not found")
                || err.to_string().contains("os error 2"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_load_no_config_returns_defaults() {
        let _atc_config_guard = std::env::var("ATC_CONFIG").ok();
        std::env::remove_var("ATC_CONFIG");

        let result = AtcConfig::load(None);
        assert!(result.is_ok());

        if let Some(val) = _atc_config_guard {
            std::env::set_var("ATC_CONFIG", val);
        }
    }

    #[test]
    fn test_expand_tilde_bare() {
        let result = expand_tilde(Path::new("~"));
        assert_eq!(result, home_dir());
    }

    #[test]
    fn test_expand_tilde_with_path() {
        let result = expand_tilde(Path::new("~/foo/bar"));
        assert_eq!(result, home_dir().join("foo/bar"));
    }

    #[test]
    fn test_expand_tilde_no_tilde() {
        let result = expand_tilde(Path::new("/absolute/path"));
        assert_eq!(result, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn test_expand_tilde_relative() {
        let result = expand_tilde(Path::new("relative/path"));
        assert_eq!(result, PathBuf::from("relative/path"));
    }

    #[test]
    fn test_resolved_path_default() {
        let cfg = RegistryConfig { path: None };
        let resolved = cfg.resolved_path();
        assert!(
            resolved.to_string_lossy().ends_with("registry.db"),
            "unexpected path: {resolved:?}"
        );
    }

    #[test]
    fn test_cloud_config_defaults_disabled() {
        let cfg = CloudConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.liveness_ttl_secs, 120);
        assert_eq!(cfg.resolved_fly_bin(), PathBuf::from("fly"));
        assert_eq!(cfg.resolved_nats_bin(), PathBuf::from("nats"));
        assert_eq!(cfg.resolved_subject_prefix(), "atc.dispatch");
        assert_eq!(
            cfg.subject_for("tasks--harmony-844@implement@1780727928109"),
            "atc.dispatch.tasks--harmony-844@implement@1780727928109.events"
        );
    }

    #[test]
    fn test_cloud_config_parses_from_toml() {
        let toml_src = r#"
            [cloud]
            enabled = true
            fly_app = "atc-workers"
            fly_image = "registry.fly.io/atc-workers:latest"
            worker_volume = "atc_mirror"
            nats_url = "nats://nats.internal:4222"
            database_url = "postgres://localhost/atc"
            repo_remote = "https://github.com/gitkb/atc.git"
            liveness_ttl_secs = 90
        "#;
        let cfg: AtcConfig = toml::from_str(toml_src).unwrap();
        assert!(cfg.cloud.enabled);
        assert_eq!(cfg.cloud.fly_app.as_deref(), Some("atc-workers"));
        assert_eq!(cfg.cloud.resolved_nats_url(), "nats://nats.internal:4222");
        assert_eq!(
            cfg.cloud.resolved_database_url().as_deref(),
            Some("postgres://localhost/atc")
        );
        assert_eq!(cfg.cloud.liveness_ttl_secs, 90);
    }

    #[test]
    fn test_cloud_config_absent_defaults_to_disabled() {
        let cfg: AtcConfig = toml::from_str("").unwrap();
        assert!(!cfg.cloud.enabled);
        assert_eq!(cfg.cloud.liveness_ttl_secs, 120);
    }

    #[test]
    fn test_parse_rejects_zero_liveness_ttl() {
        let toml = "[cloud]\nliveness_ttl_secs = 0";
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string()
                .contains("cloud.liveness_ttl_secs must be >= 1"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_resolved_path_explicit() {
        let cfg = RegistryConfig {
            path: Some(PathBuf::from("/custom/path.db")),
        };
        assert_eq!(cfg.resolved_path(), PathBuf::from("/custom/path.db"));
    }

    #[test]
    fn test_resolved_path_tilde() {
        let cfg = RegistryConfig {
            path: Some(PathBuf::from("~/my.db")),
        };
        assert_eq!(cfg.resolved_path(), home_dir().join("my.db"));
    }

    // --- DispatchConfig tests ---

    #[test]
    fn test_resolved_log_dir_default() {
        let cfg = DispatchConfig::default();
        let resolved = cfg.resolved_log_dir();
        assert!(
            resolved.to_string_lossy().ends_with("logs"),
            "unexpected path: {resolved:?}"
        );
    }

    #[test]
    fn test_resolved_log_dir_explicit() {
        let cfg = DispatchConfig {
            log_dir: Some(PathBuf::from("/custom/logs")),
            ..Default::default()
        };
        assert_eq!(cfg.resolved_log_dir(), PathBuf::from("/custom/logs"));
    }

    #[test]
    fn test_resolved_log_dir_tilde() {
        let cfg = DispatchConfig {
            log_dir: Some(PathBuf::from("~/atc-logs")),
            ..Default::default()
        };
        assert_eq!(cfg.resolved_log_dir(), home_dir().join("atc-logs"));
    }

    #[test]
    fn test_resolved_claude_bin_default() {
        let cfg = DispatchConfig::default();
        assert_eq!(cfg.resolved_claude_bin(), PathBuf::from("claude"));
    }

    #[test]
    fn test_resolved_claude_bin_explicit() {
        let cfg = DispatchConfig {
            claude_bin: Some(PathBuf::from("/usr/local/bin/claude")),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolved_claude_bin(),
            PathBuf::from("/usr/local/bin/claude")
        );
    }

    #[test]
    fn test_resolved_claude_bin_tilde() {
        let cfg = DispatchConfig {
            claude_bin: Some(PathBuf::from("~/bin/claude")),
            ..Default::default()
        };
        assert_eq!(cfg.resolved_claude_bin(), home_dir().join("bin/claude"));
    }

    #[test]
    fn test_resolved_worktree_base_default() {
        let cfg = DispatchConfig::default();
        assert_eq!(
            cfg.resolved_worktree_base(),
            PathBuf::from("/tmp/worktrees")
        );
    }

    #[test]
    fn test_resolved_worktree_base_explicit() {
        let cfg = DispatchConfig {
            worktree_base: Some(PathBuf::from("/custom/worktrees")),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolved_worktree_base(),
            PathBuf::from("/custom/worktrees")
        );
    }

    #[test]
    fn test_resolved_repo_missing() {
        let cfg = DispatchConfig::default();
        assert!(cfg.resolved_repo().is_none());
    }

    #[test]
    fn test_resolved_repo_present() {
        let cfg = DispatchConfig {
            repo: Some("core".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.resolved_repo(), Some("core"));
    }

    #[test]
    fn test_resolved_meta_workspace_root_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = DispatchConfig {
            meta_workspace_root: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let resolved = cfg.resolved_meta_workspace_root(None).unwrap();
        assert!(resolved.is_absolute());
    }

    #[test]
    fn test_resolved_meta_workspace_root_relative() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let cfg = DispatchConfig {
            meta_workspace_root: Some(PathBuf::from("sub")),
            ..Default::default()
        };
        let resolved = cfg.resolved_meta_workspace_root(Some(dir.path())).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(&sub).unwrap());
    }

    #[test]
    fn test_resolved_meta_workspace_root_default_dot() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = DispatchConfig::default();
        let resolved = cfg.resolved_meta_workspace_root(Some(dir.path())).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn test_legacy_no_sandbox_rejected() {
        let toml = "[dispatch]\nno_sandbox = true";
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string().contains("unknown field"),
            "expected deny_unknown_fields error, got: {err}"
        );
    }

    #[test]
    fn test_parse_directives_from_toml() {
        let toml = r#"
[directives.implement]
template_path = "/etc/atc/implement.md"

[directives.research]
template_inline = "Research prompt for {{slug}}"

[directives.review-fix]
template_path = "~/templates/review.md"
template_inline = "fallback (ignored)"
"#;
        let cfg = AtcConfig::parse_and_validate(toml).unwrap();
        assert_eq!(cfg.directives.len(), 3);

        let implement = cfg.directives.get("implement").unwrap();
        assert_eq!(
            implement.template_path.as_deref(),
            Some("/etc/atc/implement.md")
        );
        assert!(implement.template_inline.is_none());

        let research = cfg.directives.get("research").unwrap();
        assert!(research.template_path.is_none());
        assert_eq!(
            research.template_inline.as_deref(),
            Some("Research prompt for {{slug}}")
        );

        let review_fix = cfg.directives.get("review-fix").unwrap();
        assert!(review_fix.template_path.is_some());
        assert!(review_fix.template_inline.is_some());
    }

    #[test]
    fn test_parse_legacy_modes_from_toml() {
        let toml = r#"
[modes.implement]
template_inline = "Legacy prompt"
"#;
        let cfg = AtcConfig::parse_and_validate(toml).unwrap();
        assert_eq!(
            cfg.directives
                .get("implement")
                .and_then(|d| d.template_inline.as_deref()),
            Some("Legacy prompt")
        );
    }

    #[test]
    fn test_directive_config_per_directive_budget() {
        let toml = r#"
[directives.implement]
template_inline = "test"
max_budget_usd = 10.0
max_turns = 500
"#;
        let cfg = AtcConfig::parse_and_validate(toml).unwrap();
        let dcfg = cfg.directives.get("implement").unwrap();
        assert_eq!(dcfg.max_budget_usd, Some(10.0));
        assert_eq!(dcfg.max_turns, Some(500));
    }

    #[test]
    fn test_directive_config_rejects_unknown_fields() {
        let toml = r#"
[directives.implement]
template_paht = "typo"
"#;
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string().contains("unknown field"),
            "expected deny_unknown_fields error, got: {err}"
        );
    }

    #[test]
    fn test_unknown_directive_name_rejected() {
        let toml = r#"
[directives.implment]
template_inline = "typo in directive name"
"#;
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string().contains("unknown directive 'implment'"),
            "expected unknown directive error, got: {err}"
        );
    }

    #[test]
    fn test_config_parse_errors_escape_terminal_controls() {
        let cases = [
            r#"
[resolvers]
order = ["task\u001B[2J\u202Egpj"]
"#,
            r#"
[directives."impl\u001B[2J\u202Egpj"]
template_inline = "bad directive"
"#,
            r#"
[directives.implement]
providers = ["provider\u001B[2J\u202Egpj"]
"#,
            r#"
[directives.implement]
components = ["component\u001B[2J\u202Egpj/secret"]
"#,
            r#"
[sources."source\u001B[2J\u202Egpj"]
type = "ready"
poll_interval_secs = 0
"#,
        ];

        for toml in cases {
            let error = AtcConfig::parse_and_validate(toml).unwrap_err().to_string();

            assert!(error.contains("\\x1b[2J\\u{202e}gpj"));
            assert!(!error.contains('\x1b'));
            assert!(!error.contains('\u{202e}'));
        }
    }

    #[test]
    fn test_valid_directive_names_accepted() {
        let toml = r#"
[directives.implement]
template_inline = "a"

[directives.research]
template_inline = "b"

[directives.kb-update]
template_inline = "c"

[directives.review-fix]
template_inline = "d"

[directives.pr-comments]
template_inline = "e"

[directives.refine]
template_inline = "f"

[directives.create-task]
template_inline = "g"

[directives.close]
template_inline = "h"
"#;
        let cfg = AtcConfig::parse_and_validate(toml).unwrap();
        assert_eq!(cfg.directives.len(), 8);
    }

    #[test]
    fn test_resolved_meta_workspace_root_rejects_root() {
        let cfg = DispatchConfig {
            meta_workspace_root: Some(PathBuf::from("/")),
            ..Default::default()
        };
        let err = cfg.resolved_meta_workspace_root(None).unwrap_err();
        assert!(
            err.to_string().contains("must not be the filesystem root"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_dispatch_config_full_toml() {
        let toml = r#"
[dispatch]
repo = "core"
worktree_base = "/tmp/wt"
meta_workspace_root = "/some/path"
log_dir = "/tmp/logs"
claude_bin = "/usr/bin/claude"
sandbox = true
max_turns = 500
max_budget_usd = 5.0
max_retries = 5
"#;
        let cfg = AtcConfig::parse_and_validate(toml).unwrap();
        assert_eq!(cfg.dispatch.repo.as_deref(), Some("core"));
        assert_eq!(
            cfg.dispatch.worktree_base.as_deref(),
            Some(Path::new("/tmp/wt"))
        );
        assert_eq!(
            cfg.dispatch.meta_workspace_root.as_deref(),
            Some(Path::new("/some/path"))
        );
        assert!(cfg.dispatch.sandbox);
        assert_eq!(cfg.dispatch.max_turns, 500);
        assert_eq!(cfg.dispatch.max_budget_usd, 5.0);
        assert_eq!(cfg.dispatch.max_retries, 5);
    }

    #[test]
    fn test_max_retries_default() {
        let cfg = AtcConfig::default();
        assert_eq!(cfg.dispatch.max_retries, 3);
    }

    #[test]
    fn test_parse_rejects_zero_max_retries() {
        let toml = "[dispatch]\nmax_retries = 0";
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string().contains("max_retries must be >= 1"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_rejects_zero_max_turns() {
        let toml = "[dispatch]\nmax_turns = 0";
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string().contains("max_turns must be >= 1"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_rejects_negative_budget() {
        let toml = "[dispatch]\nmax_budget_usd = -5.0";
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string()
                .contains("max_budget_usd must be a positive finite number"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_rejects_zero_budget() {
        let toml = "[dispatch]\nmax_budget_usd = 0.0";
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string()
                .contains("max_budget_usd must be a positive finite number"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_rejects_nan_budget() {
        let toml = "[dispatch]\nmax_budget_usd = nan";
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string()
                .contains("max_budget_usd must be a positive finite number"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_rejects_infinity_budget() {
        let toml = "[dispatch]\nmax_budget_usd = inf";
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string()
                .contains("max_budget_usd must be a positive finite number"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_rejects_negative_infinity_budget() {
        let toml = "[dispatch]\nmax_budget_usd = -inf";
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string()
                .contains("max_budget_usd must be a positive finite number"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_per_directive_rejects_zero_budget() {
        let toml = "[directives.implement]\nmax_budget_usd = 0.0";
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string()
                .contains("directives.implement.max_budget_usd must be a positive finite number"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_per_directive_rejects_negative_budget() {
        let toml = "[directives.implement]\nmax_budget_usd = -5.0";
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string()
                .contains("directives.implement.max_budget_usd must be a positive finite number"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_per_directive_rejects_zero_turns() {
        let toml = "[directives.research]\nmax_turns = 0";
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string()
                .contains("directives.research.max_turns must be >= 1"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_rejects_zero_poll_interval() {
        let toml = "[watch]\npoll_interval_secs = 0";
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string()
                .contains("watch.poll_interval_secs must be >= 1"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_rejects_negative_cost_threshold() {
        let toml = "[watch]\ncost_threshold = -1.0";
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string()
                .contains("watch.cost_threshold must be a finite non-negative number"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_rejects_nan_cost_threshold() {
        let toml = "[watch]\ncost_threshold = nan";
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string()
                .contains("watch.cost_threshold must be a finite non-negative number"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_per_directive_valid_overrides_accepted() {
        let toml = r#"
[directives.implement]
max_budget_usd = 10.0
max_turns = 500
"#;
        let cfg = AtcConfig::parse_and_validate(toml).unwrap();
        let dcfg = cfg.directives.get("implement").unwrap();
        assert_eq!(dcfg.max_budget_usd, Some(10.0));
        assert_eq!(dcfg.max_turns, Some(500));
    }

    #[test]
    fn test_empty_components_list_rejected() {
        let toml = r#"
[directives.implement]
components = []
"#;
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string()
                .contains("must contain at least one component"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_blank_component_name_rejected() {
        let toml = r#"
[directives.implement]
components = ["base", "  "]
"#;
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string().contains("empty component name"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_valid_components_accepted() {
        let toml = r#"
[directives.implement]
components = ["base", "git"]
"#;
        let cfg = AtcConfig::parse_and_validate(toml).unwrap();
        let dcfg = cfg.directives.get("implement").unwrap();
        assert_eq!(
            dcfg.components,
            Some(vec!["base".to_string(), "git".to_string()])
        );
    }

    #[test]
    fn test_component_name_path_traversal_rejected() {
        let cases = [("../secret", ".."), ("foo/bar", "/"), ("foo\\\\bar", "\\")];
        for (name, reason) in cases {
            let toml = format!("[directives.implement]\ncomponents = [\"{name}\"]");
            let err = AtcConfig::parse_and_validate(&toml).unwrap_err();
            assert!(
                err.to_string().contains("invalid component name"),
                "component name '{name}' (contains {reason}) should be rejected, got: {err}"
            );
        }
    }

    // --- Upward traversal tests ---

    #[test]
    fn test_traversal_finds_config_in_start_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("atc.toml"), "[batch]\nmax_concurrency = 11").unwrap();
        let cfg = AtcConfig::find_config_upward(dir.path()).unwrap();
        assert_eq!(cfg.batch.max_concurrency, 11);
        assert_eq!(
            std::fs::canonicalize(cfg.config_dir.as_ref().unwrap()).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap(),
        );
    }

    #[test]
    fn test_traversal_finds_config_in_parent() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("atc.toml"),
            "[batch]\nmax_concurrency = 12",
        )
        .unwrap();
        let child = root.path().join("sub");
        std::fs::create_dir_all(&child).unwrap();
        let cfg = AtcConfig::find_config_upward(&child).unwrap();
        assert_eq!(cfg.batch.max_concurrency, 12);
        assert_eq!(
            std::fs::canonicalize(cfg.config_dir.as_ref().unwrap()).unwrap(),
            std::fs::canonicalize(root.path()).unwrap(),
        );
    }

    #[test]
    fn test_traversal_finds_config_in_grandparent() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("atc.toml"),
            "[batch]\nmax_concurrency = 13",
        )
        .unwrap();
        let deep = root.path().join("a/b/c/d/e");
        std::fs::create_dir_all(&deep).unwrap();
        let cfg = AtcConfig::find_config_upward(&deep).unwrap();
        assert_eq!(cfg.batch.max_concurrency, 13);
        assert_eq!(
            std::fs::canonicalize(cfg.config_dir.as_ref().unwrap()).unwrap(),
            std::fs::canonicalize(root.path()).unwrap(),
        );
    }

    #[test]
    fn test_traversal_stops_at_nearest_config() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("atc.toml"),
            "[batch]\nmax_concurrency = 20",
        )
        .unwrap();
        let sub = root.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("atc.toml"), "[batch]\nmax_concurrency = 21").unwrap();
        let deep = sub.join("deep");
        std::fs::create_dir_all(&deep).unwrap();
        let cfg = AtcConfig::find_config_upward(&deep).unwrap();
        assert_eq!(cfg.batch.max_concurrency, 21);
    }

    #[test]
    fn test_traversal_terminates_when_no_config() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        // No atc.toml anywhere under the tempdir; traversal eventually reaches
        // / where there is (almost certainly) no atc.toml either.
        let result = AtcConfig::find_config_upward(&deep);
        // May be None (no config found) or Some (if /tmp or / happens to have one).
        // The key property: it terminates without infinite loop.
        let _ = result;
    }

    #[test]
    fn test_traversal_skips_directory_named_atc_toml() {
        let root = tempfile::tempdir().unwrap();
        // Place a real config in the root
        std::fs::write(
            root.path().join("atc.toml"),
            "[batch]\nmax_concurrency = 50",
        )
        .unwrap();
        // Create a subdirectory with a *directory* named atc.toml
        let sub = root.path().join("child");
        std::fs::create_dir_all(sub.join("atc.toml")).unwrap();
        // Traversal should skip the directory and find the file in the root
        let cfg = AtcConfig::find_config_upward(&sub).unwrap();
        assert_eq!(cfg.batch.max_concurrency, 50);
    }

    #[test]
    fn test_explicit_config_overrides_traversal() {
        let root = tempfile::tempdir().unwrap();
        let sub = root.path().join("child");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            root.path().join("atc.toml"),
            "[batch]\nmax_concurrency = 30",
        )
        .unwrap();
        // Verify traversal would actually find the config from `sub`
        let traversed = AtcConfig::find_config_upward(&sub).unwrap();
        assert_eq!(traversed.batch.max_concurrency, 30);
        // Now verify explicit flag wins over that traversal result
        let explicit = tempfile::tempdir().unwrap();
        let explicit_path = explicit.path().join("explicit.toml");
        std::fs::write(&explicit_path, "[batch]\nmax_concurrency = 31").unwrap();
        let cfg = AtcConfig::load(Some(&explicit_path)).unwrap();
        assert_eq!(cfg.batch.max_concurrency, 31);
    }

    #[test]
    fn test_atc_config_env_overrides_traversal() {
        let root = tempfile::tempdir().unwrap();
        let sub = root.path().join("child");
        std::fs::create_dir_all(&sub).unwrap();
        // Place a traversable config that would be found from `sub`
        std::fs::write(
            root.path().join("atc.toml"),
            "[batch]\nmax_concurrency = 40",
        )
        .unwrap();
        // Verify traversal would find it
        let traversed = AtcConfig::find_config_upward(&sub).unwrap();
        assert_eq!(traversed.batch.max_concurrency, 40);
        // Create a different config and point ATC_CONFIG at it
        let env_dir = tempfile::tempdir().unwrap();
        let env_path = env_dir.path().join("env.toml");
        std::fs::write(&env_path, "[batch]\nmax_concurrency = 41").unwrap();
        std::env::set_var("ATC_CONFIG", env_path.to_str().unwrap());
        let cfg = AtcConfig::load(None).unwrap();
        std::env::remove_var("ATC_CONFIG");
        assert_eq!(cfg.batch.max_concurrency, 41);
    }

    #[test]
    fn test_traversal_config_dir_set_correctly() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("atc.toml"), "").unwrap();
        let sub = root.path().join("x/y/z");
        std::fs::create_dir_all(&sub).unwrap();
        let cfg = AtcConfig::find_config_upward(&sub).unwrap();
        assert_eq!(
            std::fs::canonicalize(cfg.config_dir.as_ref().unwrap()).unwrap(),
            std::fs::canonicalize(root.path()).unwrap(),
        );
    }

    #[test]
    fn test_traversal_skips_malformed_config() {
        let root = tempfile::tempdir().unwrap();
        // Valid config in root
        std::fs::write(root.path().join("atc.toml"), "").unwrap();
        // Malformed config in child dir (invalid TOML key)
        let mid = root.path().join("mid");
        std::fs::create_dir_all(&mid).unwrap();
        std::fs::write(mid.join("atc.toml"), "[invalid key!@#]").unwrap();
        let sub = mid.join("deep");
        std::fs::create_dir_all(&sub).unwrap();

        // Should skip the malformed mid/atc.toml and find root/atc.toml
        let result = AtcConfig::find_config_upward(&sub);
        assert!(result.is_some());
        assert_eq!(
            std::fs::canonicalize(result.unwrap().config_dir.unwrap()).unwrap(),
            std::fs::canonicalize(root.path()).unwrap(),
        );
    }

    // --- .atc/ directory convention tests ---

    #[test]
    fn test_traversal_finds_atc_dir_config() {
        let root = tempfile::tempdir().unwrap();
        let atc_dir = root.path().join(".atc");
        std::fs::create_dir_all(&atc_dir).unwrap();
        std::fs::write(atc_dir.join("config.toml"), "[batch]\nmax_concurrency = 42").unwrap();

        let cfg = AtcConfig::find_config_upward(root.path()).unwrap();
        assert_eq!(cfg.batch.max_concurrency, 42);
        assert!(cfg.atc_dir_mode);
        // config_dir should be the project root, not .atc/
        assert_eq!(
            std::fs::canonicalize(cfg.config_dir.as_ref().unwrap()).unwrap(),
            std::fs::canonicalize(root.path()).unwrap(),
        );
    }

    #[test]
    fn test_atc_dir_config_preferred_over_atc_toml() {
        let root = tempfile::tempdir().unwrap();
        // Both .atc/config.toml and atc.toml exist
        let atc_dir = root.path().join(".atc");
        std::fs::create_dir_all(&atc_dir).unwrap();
        std::fs::write(atc_dir.join("config.toml"), "[batch]\nmax_concurrency = 99").unwrap();
        std::fs::write(root.path().join("atc.toml"), "[batch]\nmax_concurrency = 1").unwrap();

        let cfg = AtcConfig::find_config_upward(root.path()).unwrap();
        assert_eq!(cfg.batch.max_concurrency, 99);
        assert!(cfg.atc_dir_mode);
    }

    #[test]
    fn test_atc_dir_config_sets_default_paths() {
        let root = tempfile::tempdir().unwrap();
        let atc_dir = root.path().join(".atc");
        std::fs::create_dir_all(&atc_dir).unwrap();
        std::fs::write(atc_dir.join("config.toml"), "").unwrap();

        let cfg = AtcConfig::find_config_upward(root.path()).unwrap();
        assert!(cfg.atc_dir_mode);
        assert_eq!(cfg.prompt.components_dir, ".atc/components");
        assert_eq!(cfg.prompt.templates_dir, ".atc/templates");
        assert_eq!(cfg.prompt.partials_dir, ".atc/partials");
    }

    #[test]
    fn test_atc_dir_explicit_prompt_paths_not_overridden() {
        let root = tempfile::tempdir().unwrap();
        let atc_dir = root.path().join(".atc");
        std::fs::create_dir_all(&atc_dir).unwrap();
        std::fs::write(
            atc_dir.join("config.toml"),
            "[prompt]\ncomponents_dir = \"custom/comps\"",
        )
        .unwrap();

        let cfg = AtcConfig::find_config_upward(root.path()).unwrap();
        assert_eq!(cfg.prompt.components_dir, "custom/comps");
    }

    #[test]
    fn test_atc_dir_loads_directive_files() {
        let root = tempfile::tempdir().unwrap();
        let atc_dir = root.path().join(".atc");
        std::fs::create_dir_all(atc_dir.join("directives")).unwrap();
        std::fs::write(atc_dir.join("config.toml"), "").unwrap();
        std::fs::write(
            atc_dir.join("directives/implement.toml"),
            r#"components = ["base", "git"]
max_budget_usd = 15.0
"#,
        )
        .unwrap();

        let cfg = AtcConfig::find_config_upward(root.path()).unwrap();
        let dcfg = cfg.directives.get("implement").unwrap();
        assert_eq!(
            dcfg.components,
            Some(vec!["base".to_string(), "git".to_string()])
        );
        assert_eq!(dcfg.max_budget_usd, Some(15.0));
    }

    #[test]
    fn test_config_toml_directives_override_files() {
        let root = tempfile::tempdir().unwrap();
        let atc_dir = root.path().join(".atc");
        std::fs::create_dir_all(atc_dir.join("directives")).unwrap();
        // File-based directive
        std::fs::write(
            atc_dir.join("directives/implement.toml"),
            "max_budget_usd = 10.0",
        )
        .unwrap();
        // Config-level override takes priority
        std::fs::write(
            atc_dir.join("config.toml"),
            "[directives.implement]\nmax_budget_usd = 50.0",
        )
        .unwrap();

        let cfg = AtcConfig::find_config_upward(root.path()).unwrap();
        let dcfg = cfg.directives.get("implement").unwrap();
        assert_eq!(dcfg.max_budget_usd, Some(50.0));
    }

    #[test]
    fn test_fallback_to_atc_toml_when_no_atc_dir() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("atc.toml"), "[batch]\nmax_concurrency = 7").unwrap();

        let cfg = AtcConfig::find_config_upward(root.path()).unwrap();
        assert_eq!(cfg.batch.max_concurrency, 7);
        assert!(!cfg.atc_dir_mode);
        // Default paths are always .atc/
        assert_eq!(cfg.prompt.components_dir, ".atc/components");
    }

    #[test]
    fn test_traversal_finds_atc_dir_in_parent() {
        let root = tempfile::tempdir().unwrap();
        let atc_dir = root.path().join(".atc");
        std::fs::create_dir_all(&atc_dir).unwrap();
        std::fs::write(atc_dir.join("config.toml"), "[batch]\nmax_concurrency = 33").unwrap();
        let child = root.path().join("sub/deep");
        std::fs::create_dir_all(&child).unwrap();

        let cfg = AtcConfig::find_config_upward(&child).unwrap();
        assert_eq!(cfg.batch.max_concurrency, 33);
        assert!(cfg.atc_dir_mode);
    }

    #[test]
    fn test_is_atc_dir_config_detection() {
        assert!(AtcConfig::is_atc_dir_config(Path::new(
            "/foo/.atc/config.toml"
        )));
        assert!(AtcConfig::is_atc_dir_config(Path::new(".atc/config.toml")));
        assert!(!AtcConfig::is_atc_dir_config(Path::new("/foo/atc.toml")));
        assert!(!AtcConfig::is_atc_dir_config(Path::new("/foo/config.toml")));
        assert!(!AtcConfig::is_atc_dir_config(Path::new(
            "/foo/.atc/other.toml"
        )));
    }

    #[test]
    fn test_paths_config_parsed() {
        let toml = r#"
[paths]
search_path = ["~/.config/atc", "/etc/atc"]
"#;
        let cfg = AtcConfig::parse_and_validate(toml).unwrap();
        assert_eq!(cfg.paths.search_path, vec!["~/.config/atc", "/etc/atc"]);
    }

    #[test]
    fn test_load_explicit_atc_dir_config() {
        let root = tempfile::tempdir().unwrap();
        let atc_dir = root.path().join(".atc");
        std::fs::create_dir_all(&atc_dir).unwrap();
        let config_path = atc_dir.join("config.toml");
        std::fs::write(&config_path, "[batch]\nmax_concurrency = 88").unwrap();

        let cfg = AtcConfig::load(Some(&config_path)).unwrap();
        assert_eq!(cfg.batch.max_concurrency, 88);
        assert!(cfg.atc_dir_mode);
        // config_dir should be the project root (parent of .atc/)
        assert_eq!(
            std::fs::canonicalize(cfg.config_dir.as_ref().unwrap()).unwrap(),
            std::fs::canonicalize(root.path()).unwrap(),
        );
    }

    #[test]
    fn test_directive_file_malformed_skipped() {
        let root = tempfile::tempdir().unwrap();
        let atc_dir = root.path().join(".atc");
        std::fs::create_dir_all(atc_dir.join("directives")).unwrap();
        std::fs::write(atc_dir.join("config.toml"), "").unwrap();
        // Valid directive file
        std::fs::write(
            atc_dir.join("directives/research.toml"),
            "max_budget_usd = 5.0",
        )
        .unwrap();
        // Malformed directive file — should be skipped
        std::fs::write(atc_dir.join("directives/implement.toml"), "[not valid!!!").unwrap();

        let cfg = AtcConfig::find_config_upward(root.path()).unwrap();
        // research should be loaded, implement should be skipped
        assert!(cfg.directives.contains_key("research"));
        assert!(!cfg.directives.contains_key("implement"));
    }

    #[test]
    fn test_directive_file_invalid_name_skipped() {
        let root = tempfile::tempdir().unwrap();
        let atc_dir = root.path().join(".atc");
        std::fs::create_dir_all(atc_dir.join("directives")).unwrap();
        std::fs::write(atc_dir.join("config.toml"), "").unwrap();
        // Unknown directive name — should be skipped with a warning
        std::fs::write(
            atc_dir.join("directives/unknown-action.toml"),
            "max_budget_usd = 5.0",
        )
        .unwrap();
        // Valid directive file
        std::fs::write(
            atc_dir.join("directives/research.toml"),
            "max_budget_usd = 5.0",
        )
        .unwrap();

        let cfg = AtcConfig::find_config_upward(root.path()).unwrap();
        assert!(cfg.directives.contains_key("research"));
        assert!(!cfg.directives.contains_key("unknown-action"));
    }

    #[test]
    fn test_directive_file_invalid_budget_skipped() {
        let root = tempfile::tempdir().unwrap();
        let atc_dir = root.path().join(".atc");
        std::fs::create_dir_all(atc_dir.join("directives")).unwrap();
        std::fs::write(atc_dir.join("config.toml"), "").unwrap();
        // Zero budget is invalid — should be skipped
        std::fs::write(
            atc_dir.join("directives/implement.toml"),
            "max_budget_usd = 0.0",
        )
        .unwrap();

        let cfg = AtcConfig::find_config_upward(root.path()).unwrap();
        assert!(!cfg.directives.contains_key("implement"));
    }

    #[test]
    fn test_directive_file_non_toml_ignored() {
        let root = tempfile::tempdir().unwrap();
        let atc_dir = root.path().join(".atc");
        std::fs::create_dir_all(atc_dir.join("directives")).unwrap();
        std::fs::write(atc_dir.join("config.toml"), "").unwrap();
        // .md file should be ignored
        std::fs::write(atc_dir.join("directives/README.md"), "# Directives").unwrap();

        let cfg = AtcConfig::find_config_upward(root.path()).unwrap();
        assert!(cfg.directives.is_empty());
    }

    // --- DaemonConfig tests ---

    #[test]
    fn test_daemon_config_defaults() {
        let cfg = DaemonConfig::default();
        assert_eq!(cfg.drain_interval_secs, 1);
        assert_eq!(cfg.max_concurrent, 5);
        assert_eq!(cfg.graceful_shutdown_timeout_secs, 300);
        assert!(cfg.pid_file.is_none());
    }

    #[test]
    fn test_daemon_config_from_toml() {
        let toml = r#"
[daemon]
drain_interval_secs = 10
max_concurrent = 3
graceful_shutdown_timeout_secs = 60
pid_file = "/tmp/atc.pid"
"#;
        let cfg = AtcConfig::parse_and_validate(toml).unwrap();
        assert_eq!(cfg.daemon.drain_interval_secs, 10);
        assert_eq!(cfg.daemon.max_concurrent, 3);
        assert_eq!(cfg.daemon.graceful_shutdown_timeout_secs, 60);
        assert_eq!(
            cfg.daemon.pid_file.as_deref(),
            Some(Path::new("/tmp/atc.pid"))
        );
    }

    #[test]
    fn test_parse_rejects_zero_drain_interval() {
        let toml = "[daemon]\ndrain_interval_secs = 0";
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string()
                .contains("daemon.drain_interval_secs must be >= 1"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_rejects_zero_max_concurrent() {
        let toml = "[daemon]\nmax_concurrent = 0";
        let err = AtcConfig::parse_and_validate(toml).unwrap_err();
        assert!(
            err.to_string()
                .contains("daemon.max_concurrent must be >= 1"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_resolved_pid_file_default() {
        let cfg = DaemonConfig::default();
        let resolved = cfg.resolved_pid_file(None);
        assert!(
            resolved.to_string_lossy().ends_with("daemon.pid"),
            "unexpected path: {resolved:?}"
        );
    }

    #[test]
    fn test_resolved_pid_file_explicit() {
        let cfg = DaemonConfig {
            pid_file: Some(PathBuf::from("/custom/daemon.pid")),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolved_pid_file(None),
            PathBuf::from("/custom/daemon.pid")
        );
    }

    #[test]
    fn test_resolved_pid_file_tilde() {
        let cfg = DaemonConfig {
            pid_file: Some(PathBuf::from("~/atc/daemon.pid")),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolved_pid_file(None),
            home_dir().join("atc/daemon.pid")
        );
    }

    #[test]
    fn test_resolved_pid_file_relative_with_config_dir() {
        let cfg = DaemonConfig {
            pid_file: Some(PathBuf::from("daemon.pid")),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolved_pid_file(Some(Path::new("/project/root"))),
            PathBuf::from("/project/root/daemon.pid")
        );
    }

    #[test]
    fn test_resolved_pid_file_relative_without_config_dir() {
        let cfg = DaemonConfig {
            pid_file: Some(PathBuf::from("daemon.pid")),
            ..Default::default()
        };
        // Without config_dir, relative path stays relative
        assert_eq!(cfg.resolved_pid_file(None), PathBuf::from("daemon.pid"));
    }
}
