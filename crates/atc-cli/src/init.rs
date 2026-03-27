use anyhow::{Context, Result};
use atc_core::config::AtcConfig;
use std::path::Path;

// --- Embedded default directive files ---
const DEFAULT_DIRECTIVES: &[(&str, &str)] = &[
    (
        "implement",
        include_str!("../defaults/directives/implement.toml"),
    ),
    (
        "review-fix",
        include_str!("../defaults/directives/review-fix.toml"),
    ),
    (
        "pr-comments",
        include_str!("../defaults/directives/pr-comments.toml"),
    ),
    (
        "research",
        include_str!("../defaults/directives/research.toml"),
    ),
    ("close", include_str!("../defaults/directives/close.toml")),
    ("refine", include_str!("../defaults/directives/refine.toml")),
    (
        "create-task",
        include_str!("../defaults/directives/create-task.toml"),
    ),
];

// --- Embedded default template files ---
const DEFAULT_TEMPLATES: &[(&str, &str)] = &[
    (
        "pr-review.md",
        include_str!("../defaults/templates/pr-review.md"),
    ),
    (
        "pr-comment.md",
        include_str!("../defaults/templates/pr-comment.md"),
    ),
    (
        "branch-review.md",
        include_str!("../defaults/templates/branch-review.md"),
    ),
    ("close.md", include_str!("../defaults/templates/close.md")),
    (
        "push-branch.md",
        include_str!("../defaults/templates/push-branch.md"),
    ),
    ("swot.md", include_str!("../defaults/templates/swot.md")),
];

// --- Embedded default component files ---
const DEFAULT_COMPONENTS: &[(&str, &str)] = &[
    ("base.md", include_str!("../defaults/components/base.md")),
    (
        "constraints.md",
        include_str!("../defaults/components/constraints.md"),
    ),
    (
        "code-read.md",
        include_str!("../defaults/components/code-read.md"),
    ),
    (
        "code-write.md",
        include_str!("../defaults/components/code-write.md"),
    ),
    ("git.md", include_str!("../defaults/components/git.md")),
    (
        "github.md",
        include_str!("../defaults/components/github.md"),
    ),
    (
        "review.md",
        include_str!("../defaults/components/review.md"),
    ),
    (
        "kb-read.md",
        include_str!("../defaults/components/kb-read.md"),
    ),
    (
        "kb-write.md",
        include_str!("../defaults/components/kb-write.md"),
    ),
    (
        "refine.md",
        include_str!("../defaults/components/refine.md"),
    ),
    (
        "create-task.md",
        include_str!("../defaults/components/create-task.md"),
    ),
    ("web.md", include_str!("../defaults/components/web.md")),
];

/// Scaffold a `.atc/` directory with embedded default content.
///
/// Creates:
/// - `.atc/config.toml` — base config (registry, dispatch, batch, etc.)
/// - `.atc/directives/<name>.toml` — default directive files embedded in the binary
/// - `.atc/components/<name>.md` — default component files embedded in the binary
/// - `.atc/templates/<name>.md` — default template files embedded in the binary
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

    // Write embedded default directive files
    for (name, content) in DEFAULT_DIRECTIVES {
        let path = atc_dir.join("directives").join(format!("{name}.toml"));
        let label = format!(".atc/directives/{name}.toml");
        write_file(&path, content, &label, force)?;
    }

    // Also write any extra directives from [directives.*] config sections
    // that don't have a matching embedded default
    for (name, dcfg) in &config.directives {
        let is_default = DEFAULT_DIRECTIVES.iter().any(|(n, _)| *n == name.as_str());
        if !is_default {
            let path = atc_dir.join("directives").join(format!("{name}.toml"));
            let directive_toml = toml::to_string_pretty(dcfg)
                .with_context(|| format!("failed to serialize directive config '{name}'"))?;
            let label = format!(".atc/directives/{name}.toml");
            write_file(&path, &directive_toml, &label, force)?;
        }
    }

    // Write embedded default template files
    for (name, content) in DEFAULT_TEMPLATES {
        let path = atc_dir.join("templates").join(name);
        let label = format!(".atc/templates/{name}");
        write_file(&path, content, &label, force)?;
    }

    // Write embedded default component files
    for (name, content) in DEFAULT_COMPONENTS {
        let path = atc_dir.join("components").join(name);
        let label = format!(".atc/components/{name}");
        write_file(&path, content, &label, force)?;
    }

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
    async fn test_run_init_writes_embedded_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = AtcConfig::default();
        cfg.config_dir = Some(dir.path().to_path_buf());
        run_init(&cfg, false).await.unwrap();

        // Verify embedded directive files are written with content
        let impl_path = dir.path().join(".atc/directives/implement.toml");
        assert!(impl_path.exists(), "implement.toml should exist");
        let contents = std::fs::read_to_string(&impl_path).unwrap();
        assert!(
            contents.contains("max_budget_usd"),
            "directive should have budget"
        );
        assert!(
            contents.contains("components"),
            "directive should have components"
        );

        // Verify embedded template files are written with valid frontmatter
        let pr_review = dir.path().join(".atc/templates/pr-review.md");
        assert!(pr_review.exists(), "pr-review.md should exist");
        let contents = std::fs::read_to_string(&pr_review).unwrap();
        let fm = atc_core::prompt_engine::parse_template_frontmatter(&contents)
            .expect("pr-review.md should have valid frontmatter");
        assert_eq!(
            fm.directive.as_deref(),
            Some("review-fix"),
            "template should have directive frontmatter"
        );
        assert_eq!(
            fm.required_params.as_deref(),
            Some(vec!["pr".to_string()].as_slice()),
            "template should have required_params"
        );
        assert!(
            contents.contains("{{pr}}"),
            "template should have body content"
        );

        // Verify embedded component files are written with content
        let base_comp = dir.path().join(".atc/components/base.md");
        assert!(base_comp.exists(), "base.md should exist");
        let contents = std::fs::read_to_string(&base_comp).unwrap();
        assert!(!contents.is_empty(), "component should not be empty");
        assert!(
            contents.contains("Agent"),
            "component should have agent content"
        );
    }

    #[tokio::test]
    async fn test_run_init_all_templates_have_correct_frontmatter() {
        use atc_core::prompt_engine::parse_template_frontmatter;

        let dir = tempfile::tempdir().unwrap();
        let mut cfg = AtcConfig::default();
        cfg.config_dir = Some(dir.path().to_path_buf());
        run_init(&cfg, false).await.unwrap();

        // pr-review.md → directive: review-fix, required_params: [pr]
        let c = std::fs::read_to_string(dir.path().join(".atc/templates/pr-review.md")).unwrap();
        let fm = parse_template_frontmatter(&c).expect("valid frontmatter");
        assert_eq!(fm.directive.as_deref(), Some("review-fix"));
        assert_eq!(fm.required_params, Some(vec!["pr".to_string()]));

        // pr-comment.md → directive: pr-comments, required_params: [pr]
        let c = std::fs::read_to_string(dir.path().join(".atc/templates/pr-comment.md")).unwrap();
        let fm = parse_template_frontmatter(&c).expect("valid frontmatter");
        assert_eq!(fm.directive.as_deref(), Some("pr-comments"));
        assert_eq!(fm.required_params, Some(vec!["pr".to_string()]));

        // branch-review.md → directive: review-fix, no required_params
        let c =
            std::fs::read_to_string(dir.path().join(".atc/templates/branch-review.md")).unwrap();
        let fm = parse_template_frontmatter(&c).expect("valid frontmatter");
        assert_eq!(fm.directive.as_deref(), Some("review-fix"));
        assert_eq!(fm.required_params, None);

        // close.md → directive: close, required_params: [task]
        let c = std::fs::read_to_string(dir.path().join(".atc/templates/close.md")).unwrap();
        let fm = parse_template_frontmatter(&c).expect("valid frontmatter");
        assert_eq!(fm.directive.as_deref(), Some("close"));
        assert_eq!(fm.required_params, Some(vec!["task".to_string()]));

        // push-branch.md → directive: implement, no required_params
        let c = std::fs::read_to_string(dir.path().join(".atc/templates/push-branch.md")).unwrap();
        let fm = parse_template_frontmatter(&c).expect("valid frontmatter");
        assert_eq!(fm.directive.as_deref(), Some("implement"));
        assert_eq!(fm.required_params, None);

        // swot.md → directive: research, required_params: [competitor, name]
        let c = std::fs::read_to_string(dir.path().join(".atc/templates/swot.md")).unwrap();
        let fm = parse_template_frontmatter(&c).expect("valid frontmatter");
        assert_eq!(fm.directive.as_deref(), Some("research"));
        assert_eq!(
            fm.required_params,
            Some(vec!["competitor".to_string(), "name".to_string()])
        );
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
        run_init(&cfg, false).await.unwrap();
        // Default embedded directives should exist
        let directive_path = dir.path().join(".atc/directives/implement.toml");
        assert!(directive_path.exists());
        let contents = std::fs::read_to_string(&directive_path).unwrap();
        assert!(contents.contains("max_budget_usd"));
    }

    #[tokio::test]
    async fn test_run_init_reinit_adds_new_skips_existing() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = AtcConfig::default();
        cfg.config_dir = Some(dir.path().to_path_buf());
        // First init
        run_init(&cfg, false).await.unwrap();
        let impl_path = dir.path().join(".atc/directives/implement.toml");
        assert!(impl_path.exists());
        // Modify the file so we can verify it's preserved
        std::fs::write(&impl_path, "# customized").unwrap();

        // Re-init without force
        run_init(&cfg, false).await.unwrap();

        // Old file should be preserved
        let impl_contents = std::fs::read_to_string(&impl_path).unwrap();
        assert_eq!(
            impl_contents, "# customized",
            "existing directive should be preserved"
        );
    }

    #[tokio::test]
    async fn test_run_init_force_overwrites_templates() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = AtcConfig::default();
        cfg.config_dir = Some(dir.path().to_path_buf());

        // First init
        run_init(&cfg, false).await.unwrap();

        // Customize a template
        let template_path = dir.path().join(".atc/templates/pr-review.md");
        std::fs::write(&template_path, "# customized").unwrap();

        // Force re-init should overwrite
        run_init(&cfg, true).await.unwrap();
        let contents = std::fs::read_to_string(&template_path).unwrap();
        let fm = atc_core::prompt_engine::parse_template_frontmatter(&contents)
            .expect("overwritten template should have valid frontmatter");
        assert_eq!(
            fm.directive.as_deref(),
            Some("review-fix"),
            "force should have overwritten with embedded content"
        );
    }

    #[tokio::test]
    async fn test_run_init_templates_not_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = AtcConfig::default();
        cfg.config_dir = Some(dir.path().to_path_buf());
        run_init(&cfg, false).await.unwrap();

        // Every template file should have non-empty content with valid frontmatter
        for (name, _) in DEFAULT_TEMPLATES {
            let path = dir.path().join(".atc/templates").join(name);
            let contents = std::fs::read_to_string(&path)
                .unwrap_or_else(|_| panic!("template {name} should exist"));
            assert!(!contents.is_empty(), "template {name} should not be empty");
            let fm = atc_core::prompt_engine::parse_template_frontmatter(&contents)
                .unwrap_or_else(|e| panic!("template {name} should have valid frontmatter: {e}"));
            assert!(
                fm.directive.is_some(),
                "template {name} should have a directive"
            );
        }
    }

    #[tokio::test]
    async fn test_run_init_components_not_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = AtcConfig::default();
        cfg.config_dir = Some(dir.path().to_path_buf());
        run_init(&cfg, false).await.unwrap();

        // Every component file should have non-empty content
        for (name, _) in DEFAULT_COMPONENTS {
            let path = dir.path().join(".atc/components").join(name);
            let contents = std::fs::read_to_string(&path)
                .unwrap_or_else(|_| panic!("component {name} should exist"));
            assert!(!contents.is_empty(), "component {name} should not be empty");
        }
    }

    #[tokio::test]
    async fn test_run_init_extra_config_directives_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = AtcConfig::default();
        cfg.config_dir = Some(dir.path().to_path_buf());
        // Add a custom directive not in the embedded defaults
        cfg.directives.insert(
            "custom-workflow".to_string(),
            atc_core::config::DirectiveConfig {
                max_budget_usd: Some(42.0),
                ..Default::default()
            },
        );
        run_init(&cfg, false).await.unwrap();
        let custom_path = dir.path().join(".atc/directives/custom-workflow.toml");
        assert!(
            custom_path.exists(),
            "custom directive from config should be written"
        );
        let contents = std::fs::read_to_string(&custom_path).unwrap();
        assert!(contents.contains("42.0"));
    }

    #[tokio::test]
    async fn test_run_init_force_overwrites_extra_config_directives() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = AtcConfig::default();
        cfg.config_dir = Some(dir.path().to_path_buf());
        cfg.directives.insert(
            "custom-workflow".to_string(),
            atc_core::config::DirectiveConfig {
                max_budget_usd: Some(42.0),
                ..Default::default()
            },
        );
        // First init writes the custom directive
        run_init(&cfg, false).await.unwrap();
        let custom_path = dir.path().join(".atc/directives/custom-workflow.toml");
        assert!(custom_path.exists());

        // Modify it manually
        std::fs::write(&custom_path, "# customized").unwrap();

        // Re-init with --force should overwrite the custom directive
        run_init(&cfg, true).await.unwrap();
        let contents = std::fs::read_to_string(&custom_path).unwrap();
        assert!(
            contents.contains("42.0"),
            "force should have overwritten custom directive"
        );
    }
}
