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
///
/// **Re-init behavior:**
/// - Without `--force`: create files that don't exist, skip files that do.
/// - With `--force`: overwrite everything.
pub async fn run_init(config: &AtcConfig, force: bool) -> Result<()> {
    let base = config
        .config_dir
        .as_deref()
        .unwrap_or_else(|| Path::new("."));
    let atc_dir = base.join(".atc");

    let is_reinit = atc_dir.exists();

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
    let config_path = atc_dir.join("config.toml");
    let config_toml = build_config_toml(config);
    write_file(&config_path, &config_toml, ".atc/config.toml", force)?;

    // Write directive files from [directives.*] sections
    for (name, dcfg) in &config.directives {
        let directive_toml = toml::to_string_pretty(dcfg)
            .with_context(|| format!("failed to serialize directive config '{name}'"))?;
        let path = atc_dir.join("directives").join(format!("{name}.toml"));
        let label = format!(".atc/directives/{name}.toml");
        write_file(&path, &directive_toml, &label, force)?;
    }

    // Copy components
    let components_dir = resolve_dir(&config.prompt.components_dir, config.config_dir.as_deref());
    copy_md_files(
        &components_dir,
        &atc_dir.join("components"),
        "components",
        force,
    )?;

    // Copy templates
    let templates_dir = resolve_dir(&config.prompt.templates_dir, config.config_dir.as_deref());
    copy_md_files(
        &templates_dir,
        &atc_dir.join("templates"),
        "templates",
        force,
    )?;

    if is_reinit && !force {
        println!(
            "\nRe-initialized .atc/ at {} (skipped existing files)",
            atc_dir.display()
        );
    } else if is_reinit {
        println!(
            "\nRe-initialized .atc/ at {} (force overwrote existing files)",
            atc_dir.display()
        );
    } else {
        println!("\nInitialized .atc/ at {}", atc_dir.display());
    }
    if !config.atc_dir_mode && base.join("atc.toml").exists() {
        println!("You can now remove atc.toml — ATC will use .atc/config.toml instead.");
    }

    Ok(())
}

/// Write a file, respecting the `force` flag.
/// If the file exists and `force` is false, print a skip message and return.
fn write_file(path: &Path, content: &str, label: &str, force: bool) -> Result<()> {
    if path.exists() && !force {
        println!("  Skipped (exists): {label}");
        return Ok(());
    }
    std::fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))?;
    println!("  Created {label}");
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

    // Include daemon if non-default
    let default_daemon = atc_core::config::DaemonConfig::default();
    if config.daemon.drain_interval_secs != default_daemon.drain_interval_secs
        || config.daemon.max_concurrent != default_daemon.max_concurrent
        || config.daemon.graceful_shutdown_timeout_secs
            != default_daemon.graceful_shutdown_timeout_secs
        || config.daemon.pid_file.is_some()
    {
        if let Ok(s) = toml::to_string_pretty(&config.daemon) {
            parts.push(format!("[daemon]\n{s}"));
        }
    }

    // Include sources if any are configured.
    // Use a wrapper struct so entries serialize as [sources.<name>] tables.
    if !config.sources.is_empty() {
        #[derive(serde::Serialize)]
        struct Wrapper<'a> {
            sources: &'a std::collections::HashMap<String, atc_core::source::SourceConfig>,
        }
        if let Ok(s) = toml::to_string_pretty(&Wrapper {
            sources: &config.sources,
        }) {
            parts.push(s);
        }
    }

    if parts.is_empty() {
        "# ATC configuration\n# Directives are in .atc/directives/*.toml\n# Components are in .atc/components/*.md\n# Templates are in .atc/templates/*.md\n".to_string()
    } else {
        parts.join("\n")
    }
}

/// Copy all `.md` files from `src` to `dst`, respecting the `force` flag.
/// Skips when `src` and `dst` resolve to the same directory (e.g. during re-init).
fn copy_md_files(src: &Path, dst: &Path, label: &str, force: bool) -> Result<()> {
    // Guard: if src and dst are the same directory, copying is a no-op (or harmful).
    if let (Ok(canon_src), Ok(canon_dst)) = (src.canonicalize(), dst.canonicalize()) {
        if canon_src == canon_dst {
            return Ok(());
        }
    }

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

    for entry in entries {
        let entry = entry.with_context(|| {
            format!(
                "failed to read entry in {label} directory {}",
                src.display()
            )
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            if let Some(name) = path.file_name() {
                let dest = dst.join(name);
                let name_str = name.to_string_lossy();
                if dest.exists() && !force {
                    println!("  Skipped (exists): .atc/{label}/{name_str}");
                } else {
                    std::fs::copy(&path, &dest).with_context(|| {
                        format!("failed to copy {} to {}", path.display(), dest.display())
                    })?;
                    println!("  Created .atc/{label}/{name_str}");
                }
            }
        }
    }
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

    #[test]
    fn test_build_config_toml_includes_daemon_when_non_default() {
        let mut cfg = AtcConfig::default();
        cfg.daemon.max_concurrent = 10;
        let toml = build_config_toml(&cfg);
        assert!(toml.contains("[daemon]"), "missing [daemon] section");
        assert!(
            toml.contains("max_concurrent = 10"),
            "missing max_concurrent value"
        );
    }

    #[test]
    fn test_build_config_toml_includes_sources_when_present() {
        let mut cfg = AtcConfig::default();
        cfg.sources.insert(
            "backlog".to_string(),
            atc_core::source::SourceConfig::Ready(atc_core::source::ReadySourceConfig {
                poll_interval_secs: 60,
                limit: 5,
                queue: "default".to_string(),
            }),
        );
        let toml = build_config_toml(&cfg);
        assert!(
            toml.contains("[sources.backlog]"),
            "missing [sources.backlog] section, got:\n{toml}"
        );
        assert!(
            toml.contains("poll_interval_secs = 60"),
            "missing poll_interval_secs"
        );
    }

    #[test]
    fn test_build_config_toml_omits_daemon_when_default() {
        let cfg = AtcConfig::default();
        let toml = build_config_toml(&cfg);
        assert!(
            !toml.contains("[daemon]"),
            "default config should not include [daemon]"
        );
    }

    #[test]
    fn test_build_config_toml_omits_sources_when_empty() {
        let cfg = AtcConfig::default();
        let toml = build_config_toml(&cfg);
        assert!(
            !toml.contains("[sources"),
            "default config should not include [sources]"
        );
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
    async fn test_run_init_skips_existing_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let atc_dir = dir.path().join(".atc");
        std::fs::create_dir_all(&atc_dir).unwrap();
        // Write a pre-existing config.toml with custom content
        std::fs::write(atc_dir.join("config.toml"), "# custom").unwrap();
        let mut cfg = AtcConfig::default();
        cfg.config_dir = Some(dir.path().to_path_buf());
        // Re-init without force should succeed (skip existing files)
        run_init(&cfg, false).await.unwrap();
        // Pre-existing file should be preserved
        let contents = std::fs::read_to_string(atc_dir.join("config.toml")).unwrap();
        assert_eq!(
            contents, "# custom",
            "existing config.toml should be preserved"
        );
    }

    #[tokio::test]
    async fn test_run_init_force_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let atc_dir = dir.path().join(".atc");
        std::fs::create_dir_all(&atc_dir).unwrap();
        // Write a pre-existing config.toml with custom content
        std::fs::write(atc_dir.join("config.toml"), "# custom").unwrap();
        let mut cfg = AtcConfig::default();
        cfg.config_dir = Some(dir.path().to_path_buf());
        run_init(&cfg, true).await.unwrap();
        assert!(dir.path().join(".atc/config.toml").exists());
        // Force should have overwritten the custom content
        let contents = std::fs::read_to_string(atc_dir.join("config.toml")).unwrap();
        assert_ne!(
            contents, "# custom",
            "config.toml should be overwritten with --force"
        );
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

    #[tokio::test]
    async fn test_run_init_reinit_adds_new_skips_existing() {
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
        // First init
        run_init(&cfg, false).await.unwrap();
        let impl_path = dir.path().join(".atc/directives/implement.toml");
        assert!(impl_path.exists());
        // Modify the file so we can verify it's preserved
        std::fs::write(&impl_path, "# customized").unwrap();

        // Add a new directive and re-init without force
        cfg.directives.insert(
            "research".to_string(),
            atc_core::config::DirectiveConfig {
                max_budget_usd: Some(7.0),
                ..Default::default()
            },
        );
        run_init(&cfg, false).await.unwrap();

        // Old file should be preserved
        let impl_contents = std::fs::read_to_string(&impl_path).unwrap();
        assert_eq!(
            impl_contents, "# customized",
            "existing directive should be preserved"
        );
        // New file should be created
        let research_path = dir.path().join(".atc/directives/research.toml");
        assert!(research_path.exists(), "new directive should be created");
        let research_contents = std::fs::read_to_string(&research_path).unwrap();
        assert!(research_contents.contains("7.0"));
    }

    /// Regression test: force re-init when templates_dir already points to `.atc/templates`
    /// should not corrupt files by copying them onto themselves.
    #[tokio::test]
    async fn test_force_reinit_same_src_dst_does_not_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = AtcConfig::default();
        cfg.config_dir = Some(dir.path().to_path_buf());

        // First init to create .atc/ scaffolding
        run_init(&cfg, false).await.unwrap();

        // Write a template directly into .atc/templates (simulates existing content)
        let template_path = dir.path().join(".atc/templates/task.md");
        let original = "---\ndirective: implement\n---\nDo the thing";
        std::fs::write(&template_path, original).unwrap();

        // Now set templates_dir to .atc/templates — same as destination
        cfg.prompt.templates_dir = ".atc/templates".to_string();

        // Force re-init: should not corrupt or error
        run_init(&cfg, true).await.unwrap();

        // File should still be intact
        let contents = std::fs::read_to_string(&template_path).unwrap();
        assert_eq!(
            contents, original,
            "template should not be corrupted by src==dst copy"
        );
    }

    #[tokio::test]
    async fn test_run_init_copies_templates_with_skip() {
        let dir = tempfile::tempdir().unwrap();
        // Create source templates dir with one template
        let src_templates = dir.path().join("templates");
        std::fs::create_dir_all(&src_templates).unwrap();
        std::fs::write(
            src_templates.join("pr-review.md"),
            "---\ndirective: review-fix\nrequired_params: [pr]\n---\nReview {{pr}}",
        )
        .unwrap();
        std::fs::write(
            src_templates.join("swot.md"),
            "---\ndirective: research\nrequired_params: [competitor, name]\n---\nSWOT {{name}}",
        )
        .unwrap();

        let mut cfg = AtcConfig::default();
        cfg.config_dir = Some(dir.path().to_path_buf());
        cfg.prompt.templates_dir = "templates".to_string();
        // First init
        run_init(&cfg, false).await.unwrap();
        assert!(dir.path().join(".atc/templates/pr-review.md").exists());
        assert!(dir.path().join(".atc/templates/swot.md").exists());

        // Modify pr-review and re-init
        std::fs::write(
            dir.path().join(".atc/templates/pr-review.md"),
            "# customized",
        )
        .unwrap();
        run_init(&cfg, false).await.unwrap();
        // Existing template should be preserved
        let contents =
            std::fs::read_to_string(dir.path().join(".atc/templates/pr-review.md")).unwrap();
        assert_eq!(contents, "# customized");
    }
}
