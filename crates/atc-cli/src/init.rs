use anyhow::{Context, Result};
use atc_core::config::AtcConfig;
use atc_core::prompt_engine::resolve_dir;
use std::path::Path;

/// Scaffold a `.atc/` directory from the current configuration.
///
/// Creates:
/// - `.atc/config.toml` — base config (registry, dispatch, batch, etc.)
/// - `.atc/directives/<name>.toml` — one file per `[directives.*]` entry
/// - `.atc/components/` — copies component `.md` files from current components_dir
/// - `.atc/templates/` — copies template `.md` files from current templates_dir
pub async fn run_init(config: &AtcConfig, force: bool) -> Result<()> {
    let base = config
        .config_dir
        .as_deref()
        .unwrap_or_else(|| Path::new("."));
    let atc_dir = base.join(".atc");

    if atc_dir.exists() && !force {
        anyhow::bail!(
            ".atc/ directory already exists at {}. Use --force to overwrite.",
            atc_dir.display()
        );
    }

    // Create directory structure
    let dirs = [
        atc_dir.clone(),
        atc_dir.join("directives"),
        atc_dir.join("templates"),
        atc_dir.join("components"),
    ];
    for dir in &dirs {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create directory {}", dir.display()))?;
    }

    // Write config.toml (without directives — those go in separate files)
    let config_toml = build_config_toml(config);
    std::fs::write(atc_dir.join("config.toml"), config_toml)
        .context("failed to write .atc/config.toml")?;
    println!("  Created .atc/config.toml");

    // Write directive files from [directives.*] sections
    for (name, dcfg) in &config.directives {
        let directive_toml = toml::to_string_pretty(dcfg)
            .with_context(|| format!("failed to serialize directive config '{name}'"))?;
        let path = atc_dir.join("directives").join(format!("{name}.toml"));
        std::fs::write(&path, directive_toml)
            .with_context(|| format!("failed to write {}", path.display()))?;
        println!("  Created .atc/directives/{name}.toml");
    }

    // Copy components
    let components_dir = resolve_dir(&config.prompt.components_dir, config.config_dir.as_deref());
    copy_md_files(&components_dir, &atc_dir.join("components"), "components")?;

    // Copy templates
    let templates_dir = resolve_dir(&config.prompt.templates_dir, config.config_dir.as_deref());
    copy_md_files(&templates_dir, &atc_dir.join("templates"), "templates")?;

    println!("\nInitialized .atc/ at {}", atc_dir.display());
    if !config.atc_dir_mode && base.join("atc.toml").exists() {
        println!("You can now remove atc.toml — ATC will use .atc/config.toml instead.");
    }

    Ok(())
}

/// Build a config.toml string from the current config, excluding `[directives.*]`
/// (those are split into separate files under `.atc/directives/`).
fn build_config_toml(config: &AtcConfig) -> String {
    let mut parts = Vec::new();

    // Only write non-default sections
    if config.registry.path.is_some() {
        if let Ok(s) = toml::to_string_pretty(&config.registry) {
            parts.push(format!("[registry]\n{s}"));
        }
    }

    // Always write dispatch if it has non-default values
    if let Ok(s) = toml::to_string_pretty(&config.dispatch) {
        if !s.trim().is_empty() {
            parts.push(format!("[dispatch]\n{s}"));
        }
    }

    if config.batch.max_concurrency != atc_core::config::BatchConfig::default().max_concurrency {
        if let Ok(s) = toml::to_string_pretty(&config.batch) {
            parts.push(format!("[batch]\n{s}"));
        }
    }

    // Include health if non-default
    let default_health = atc_core::config::HealthConfig::default();
    if config.health.signal_timeout_secs != default_health.signal_timeout_secs
        || config.health.auto_review != default_health.auto_review
        || config.health.cost_warning_threshold != default_health.cost_warning_threshold
    {
        if let Ok(s) = toml::to_string_pretty(&config.health) {
            parts.push(format!("[health]\n{s}"));
        }
    }

    // Include watch if non-default
    let default_watch = atc_core::config::WatchConfig::default();
    if config.watch.poll_interval_secs != default_watch.poll_interval_secs
        || config.watch.cost_threshold != default_watch.cost_threshold
    {
        if let Ok(s) = toml::to_string_pretty(&config.watch) {
            parts.push(format!("[watch]\n{s}"));
        }
    }

    // Include resolvers if non-default.
    // Use a wrapper struct so nested sub-tables serialize as [resolvers.task] etc.
    let default_resolvers = atc_core::config::ResolversConfig::default();
    if config.resolvers.order != default_resolvers.order
        || config.resolvers.task.enabled != default_resolvers.task.enabled
        || config.resolvers.template.enabled != default_resolvers.template.enabled
        || config.resolvers.prompt.enabled != default_resolvers.prompt.enabled
    {
        #[derive(serde::Serialize)]
        struct Wrapper<'a> {
            resolvers: &'a atc_core::config::ResolversConfig,
        }
        if let Ok(s) = toml::to_string_pretty(&Wrapper {
            resolvers: &config.resolvers,
        }) {
            parts.push(s);
        }
    }

    // Include prompt if non-default
    let default_prompt = atc_core::config::PromptConfig::default();
    if config.prompt.components_dir != default_prompt.components_dir
        || config.prompt.templates_dir != default_prompt.templates_dir
        || config.prompt.partials_dir != default_prompt.partials_dir
    {
        if let Ok(s) = toml::to_string_pretty(&config.prompt) {
            parts.push(format!("[prompt]\n{s}"));
        }
    }

    // Include paths if non-default
    if !config.paths.search_path.is_empty() {
        if let Ok(s) = toml::to_string_pretty(&config.paths) {
            parts.push(format!("[paths]\n{s}"));
        }
    }

    if let Some(ref notif) = config.notifications {
        if let Ok(s) = toml::to_string_pretty(notif) {
            parts.push(format!("[notifications]\n{s}"));
        }
    }

    if parts.is_empty() {
        "# ATC configuration\n# Directives are in .atc/directives/*.toml\n# Components are in .atc/components/*.md\n# Templates are in .atc/templates/*.md\n".to_string()
    } else {
        parts.join("\n")
    }
}

/// Copy all `.md` files from `src` to `dst`.
fn copy_md_files(src: &Path, dst: &Path, label: &str) -> Result<()> {
    let entries = match std::fs::read_dir(src) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "  Skipping {label} (source dir not found: {})",
                src.display()
            );
            return Ok(());
        }
        Err(e) => {
            return Err(e)
                .with_context(|| format!("failed to read {label} directory {}", src.display()));
        }
    };

    let mut count = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            if let Some(name) = path.file_name() {
                let dest = dst.join(name);
                std::fs::copy(&path, &dest).with_context(|| {
                    format!("failed to copy {} to {}", path.display(), dest.display())
                })?;
                count += 1;
            }
        }
    }
    println!("  Copied {count} {label}");
    Ok(())
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use atc_core::config::AtcConfig;

    #[test]
    fn test_build_config_toml_default_includes_dispatch() {
        let cfg = AtcConfig::default();
        let toml = build_config_toml(&cfg);
        // Default config includes [dispatch] because it has non-Option fields with defaults
        assert!(
            toml.contains("[dispatch]"),
            "default config should include [dispatch] section, got: {toml}"
        );
        // But should NOT include sections that are only written when non-default
        assert!(!toml.contains("[registry]"));
        assert!(!toml.contains("[resolvers]"));
        assert!(!toml.contains("[prompt]"));
    }

    #[test]
    fn test_build_config_toml_includes_registry_when_set() {
        let mut cfg = AtcConfig::default();
        cfg.registry.path = Some("/tmp/test.db".into());
        let toml = build_config_toml(&cfg);
        assert!(toml.contains("[registry]"), "missing [registry] section");
        assert!(toml.contains("test.db"), "missing registry path");
    }

    #[test]
    fn test_build_config_toml_includes_notifications_when_set() {
        let mut cfg = AtcConfig::default();
        cfg.notifications = Some(atc_core::config::NotificationsConfig {
            macos: true,
            webhook_url: Some("https://example.com/hook".into()),
        });
        let toml = build_config_toml(&cfg);
        assert!(
            toml.contains("[notifications]"),
            "missing [notifications] section"
        );
        assert!(toml.contains("example.com/hook"), "missing webhook URL");
    }

    #[test]
    fn test_build_config_toml_resolvers_nested_tables_correct() {
        let mut cfg = AtcConfig::default();
        cfg.resolvers.task.enabled = false;
        let toml = build_config_toml(&cfg);
        assert!(
            toml.contains("[resolvers.task]"),
            "resolvers sub-tables must use dotted keys, got:\n{toml}"
        );
        assert!(
            !toml.contains("\n[task]\n"),
            "resolvers sub-tables must not appear as top-level tables"
        );
    }

    #[test]
    fn test_build_config_toml_includes_prompt_when_non_default() {
        let mut cfg = AtcConfig::default();
        cfg.prompt.components_dir = "custom/components".to_string();
        let toml = build_config_toml(&cfg);
        assert!(toml.contains("[prompt]"), "missing [prompt] section");
        assert!(
            toml.contains("custom/components"),
            "missing custom components_dir"
        );
    }

    #[test]
    fn test_build_config_toml_includes_paths_when_non_empty() {
        let mut cfg = AtcConfig::default();
        cfg.paths.search_path = vec!["~/.config/atc".into()];
        let toml = build_config_toml(&cfg);
        assert!(toml.contains("[paths]"), "missing [paths] section");
    }

    #[test]
    fn test_build_config_toml_roundtrip_resolvers() {
        let mut cfg = AtcConfig::default();
        cfg.resolvers.task.enabled = false;
        let toml_str = build_config_toml(&cfg);
        // The generated TOML should be parseable back
        let reparsed: AtcConfig = toml::from_str(&toml_str).expect("generated TOML must be valid");
        assert!(!reparsed.resolvers.task.enabled);
        assert!(reparsed.resolvers.template.enabled);
    }

    #[tokio::test]
    async fn test_run_init_creates_atc_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = AtcConfig::default();
        cfg.config_dir = Some(dir.path().to_path_buf());
        run_init(&cfg, false).await.unwrap();
        assert!(dir.path().join(".atc/config.toml").exists());
        assert!(dir.path().join(".atc/directives").is_dir());
        assert!(dir.path().join(".atc/templates").is_dir());
        assert!(dir.path().join(".atc/components").is_dir());
    }

    #[tokio::test]
    async fn test_run_init_fails_if_exists_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let atc_dir = dir.path().join(".atc");
        std::fs::create_dir_all(&atc_dir).unwrap();
        let mut cfg = AtcConfig::default();
        cfg.config_dir = Some(dir.path().to_path_buf());
        let err = run_init(&cfg, false).await.unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn test_run_init_force_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let atc_dir = dir.path().join(".atc");
        std::fs::create_dir_all(&atc_dir).unwrap();
        let mut cfg = AtcConfig::default();
        cfg.config_dir = Some(dir.path().to_path_buf());
        run_init(&cfg, true).await.unwrap();
        assert!(dir.path().join(".atc/config.toml").exists());
    }

    #[tokio::test]
    async fn test_run_init_writes_directive_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = AtcConfig::default();
        cfg.config_dir = Some(dir.path().to_path_buf());
        cfg.directives.insert(
            "implement".to_string(),
            atc_core::config::DirectiveConfig {
                max_budget_usd: Some(15.0),
                ..Default::default()
            },
        );
        run_init(&cfg, false).await.unwrap();
        let directive_path = dir.path().join(".atc/directives/implement.toml");
        assert!(directive_path.exists());
        let contents = std::fs::read_to_string(&directive_path).unwrap();
        assert!(contents.contains("15.0"));
    }
}
