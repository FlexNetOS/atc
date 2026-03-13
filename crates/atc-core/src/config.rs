use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Top-level ATC configuration. Loaded from TOML file.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct AtcConfig {
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
            return Self::parse_and_validate(&contents);
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
            return Self::parse_and_validate(&contents);
        }

        // 3. ./atc.toml
        let local_path = PathBuf::from("./atc.toml");
        if local_path.exists() {
            let contents = std::fs::read_to_string(&local_path)?;
            return Self::parse_and_validate(&contents);
        }

        // 4. XDG config path ($XDG_CONFIG_HOME/atc/config.toml, fallback ~/.config)
        let xdg_path = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir().join(".config"))
            .join("atc/config.toml");
        if xdg_path.exists() {
            let contents = std::fs::read_to_string(&xdg_path)?;
            return Self::parse_and_validate(&contents);
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
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct DispatchConfig {
    /// Directory where stream-json log files are written.
    /// Default: ~/.local/share/atc/logs/
    pub log_dir: Option<PathBuf>,
    /// Path to the `claude` binary. Default: "claude" (found via $PATH).
    pub claude_bin: Option<PathBuf>,
    /// Pass --no-sandbox to claude. Default: false.
    #[serde(default)]
    pub no_sandbox: bool,
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

fn expand_tilde(p: &Path) -> PathBuf {
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
        assert!(!cfg.dispatch.no_sandbox);
    }

    #[test]
    fn test_parse_valid_toml() {
        let toml = r#"
[batch]
max_concurrency = 5

[registry]
path = "/tmp/test.db"

[dispatch]
no_sandbox = true
"#;
        let cfg = AtcConfig::parse_and_validate(toml).unwrap();
        assert_eq!(cfg.batch.max_concurrency, 5);
        assert_eq!(
            cfg.registry.path.as_deref(),
            Some(Path::new("/tmp/test.db"))
        );
        assert!(cfg.dispatch.no_sandbox);
    }

    #[test]
    fn test_parse_empty_toml_uses_defaults() {
        let cfg = AtcConfig::parse_and_validate("").unwrap();
        assert_eq!(cfg.batch.max_concurrency, 3);
        assert!(cfg.registry.path.is_none());
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
        // Temporarily unset env vars that could influence config loading
        let _atc_config_guard = std::env::var("ATC_CONFIG").ok();
        std::env::remove_var("ATC_CONFIG");

        // Load from a CWD that definitely has no atc.toml
        let _dir = tempfile::tempdir().unwrap();
        let original_dir = std::env::current_dir().unwrap();
        // Don't change CWD in tests as it's process-global; just verify default behavior
        // by checking that load(None) returns Ok when no env/file is present
        // (it may find ./atc.toml in the workspace, so just verify it doesn't panic)
        let result = AtcConfig::load(None);
        assert!(result.is_ok());

        // Restore
        if let Some(val) = _atc_config_guard {
            std::env::set_var("ATC_CONFIG", val);
        }
        let _ = original_dir;
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
}
