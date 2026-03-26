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
    println!("You can now remove atc.toml — ATC will use .atc/config.toml instead.");

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

    if config.batch.max_concurrency != 3 {
        if let Ok(s) = toml::to_string_pretty(&config.batch) {
            parts.push(format!("[batch]\n{s}"));
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
