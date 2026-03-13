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
            return Ok(toml::from_str(&contents)?);
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
            return Ok(toml::from_str(&contents)?);
        }

        // 3. ./atc.toml
        let local_path = PathBuf::from("./atc.toml");
        if local_path.exists() {
            let contents = std::fs::read_to_string(&local_path)?;
            return Ok(toml::from_str(&contents)?);
        }

        // 4. XDG config path ($XDG_CONFIG_HOME/atc/config.toml, fallback ~/.config)
        let xdg_path = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir().join(".config"))
            .join("atc/config.toml");
        if xdg_path.exists() {
            let contents = std::fs::read_to_string(&xdg_path)?;
            return Ok(toml::from_str(&contents)?);
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
            .map(PathBuf::from)
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
