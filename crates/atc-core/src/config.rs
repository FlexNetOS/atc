use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Top-level ATC configuration. Loaded from TOML file.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct AtcConfig {
    /// Directory containing the config file that was loaded.
    /// Used to resolve relative paths in DispatchConfig.
    #[serde(skip)]
    pub config_dir: Option<PathBuf>,

    #[serde(default)]
    pub registry: RegistryConfig,
    #[serde(default)]
    pub dispatch: DispatchConfig,
    #[serde(default)]
    pub batch: BatchConfig,
}

impl AtcConfig {
    fn parse_and_validate(contents: &str) -> anyhow::Result<Self> {
        let cfg: Self = toml::from_str(contents)?;
        anyhow::ensure!(
            cfg.batch.max_concurrency > 0,
            "batch.max_concurrency must be >= 1"
        );
        Ok(cfg)
    }

    /// Load config using resolution order:
    /// 1. `--config <path>` CLI flag (passed as argument)
    /// 2. `ATC_CONFIG` environment variable
    /// 3. `./atc.toml` (current working directory)
    /// 4. `~/.config/atc/config.toml` (XDG user config)
    ///
    /// Returns default config if no file is found.
    pub fn load(config_path: Option<&std::path::Path>) -> anyhow::Result<Self> {
        // 1. Explicit path from CLI flag
        if let Some(path) = config_path {
            let path = expand_tilde(path);
            let contents = std::fs::read_to_string(&path)?;
            let mut cfg = Self::parse_and_validate(&contents)?;
            cfg.config_dir = path.parent().map(|p| p.to_path_buf());
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
            cfg.config_dir = path.parent().map(|p| p.to_path_buf());
            return Ok(cfg);
        }

        // 3. ./atc.toml
        let local_path = PathBuf::from("./atc.toml");
        if local_path.exists() {
            let contents = std::fs::read_to_string(&local_path)?;
            let mut cfg = Self::parse_and_validate(&contents)?;
            cfg.config_dir = std::env::current_dir().ok();
            return Ok(cfg);
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
}

/// `[registry]` section
#[derive(Debug, Default, Deserialize, Serialize)]
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
#[derive(Debug, Deserialize, Serialize)]
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
}

fn default_max_turns() -> u32 {
    10_000
}
fn default_max_budget_usd() -> f64 {
    25.0
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

    /// Resolve the repo alias. Returns an error if not configured.
    pub fn resolved_repo(&self) -> anyhow::Result<&str> {
        self.repo
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("dispatch.repo is required in atc.toml"))
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

        std::fs::canonicalize(&absolute).map_err(|e| {
            anyhow::anyhow!(
                "cannot resolve meta_workspace_root '{}': {}",
                absolute.display(),
                e
            )
        })
    }
}

/// `[batch]` section
#[derive(Debug, Deserialize, Serialize)]
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
        let err = cfg.resolved_repo().unwrap_err();
        assert!(err.to_string().contains("dispatch.repo is required"));
    }

    #[test]
    fn test_resolved_repo_present() {
        let cfg = DispatchConfig {
            repo: Some("core".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.resolved_repo().unwrap(), "core");
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
    }
}
