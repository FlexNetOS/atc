use anyhow::{Context, Result};
use handlebars::Handlebars;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::{expand_tilde, AtcConfig};
use crate::types::Mode;

/// Result of rendering a template: the rendered body and a list of directive/component names.
#[derive(Debug, Clone)]
pub struct TemplateOutput {
    pub body: String,
    pub directives: Vec<String>,
}

/// Assemble a system prompt from component `.md` files listed in the mode config.
///
/// Each component name maps to `<components_dir>/<name>.md`. Files are concatenated
/// in order with `\n\n` separators. After assembly, any `{{> partial}}` Handlebars
/// partial tags are expanded via the 3-level partial resolver.
pub async fn assemble_system_prompt(
    mode: &Mode,
    config: &AtcConfig,
    worktree_path: Option<&Path>,
) -> Result<String> {
    let mode_key = mode.as_str();
    let mode_config = config
        .modes
        .get(mode_key)
        .with_context(|| format!("no mode config for '{mode_key}'"))?;

    let components = mode_config
        .components
        .as_ref()
        .with_context(|| format!("mode '{mode_key}' has no components list"))?;

    let components_dir = resolve_dir(&config.prompt.components_dir, config.config_dir.as_deref());

    let mut parts = Vec::with_capacity(components.len());
    for name in components {
        let path = components_dir.join(format!("{name}.md"));
        let content = tokio::fs::read_to_string(&path).await.with_context(|| {
            format!("failed to read component '{name}' at '{}'", path.display())
        })?;
        parts.push(content);
    }

    let assembled = parts.join("\n\n");

    // Expand partials in the assembled prompt
    let hbs = build_registry(config, worktree_path).await?;
    let rendered = hbs
        .render_template(&assembled, &serde_json::json!({}))
        .with_context(|| {
            format!("failed to expand partials in assembled prompt for mode '{mode_key}'")
        })?;

    Ok(rendered)
}

/// Render a Handlebars template file with YAML frontmatter.
///
/// This is a standalone entry point for callers that need full Handlebars
/// rendering (variables + partials) from a template file. It is **not** used
/// by the legacy `template_path` dispatch path, which intentionally uses
/// simple `{{slug}}`/`{{directive}}` token replacement for backward
/// compatibility. Use this function directly when you need rich templating.
///
/// Returns the rendered body and the list of directives from the frontmatter.
/// Template files have the format:
/// ```text
/// ---
/// description: "..."
/// directives: [component1, component2]
/// ---
/// Template body with {{variable}} and {{> partial}} tags
/// ```
pub async fn render_template(
    template_path: &Path,
    params: &BTreeMap<String, String>,
    config: &AtcConfig,
    worktree_path: Option<&Path>,
) -> Result<TemplateOutput> {
    let raw = tokio::fs::read_to_string(template_path)
        .await
        .with_context(|| format!("failed to read template file '{}'", template_path.display()))?;

    let (frontmatter, body) = split_frontmatter(&raw)?;

    let hbs = build_registry(config, worktree_path).await?;
    let rendered = hbs
        .render_template(body, params)
        .with_context(|| format!("failed to render template '{}'", template_path.display()))?;

    Ok(TemplateOutput {
        body: rendered,
        directives: frontmatter.directives,
    })
}

/// Top-level prompt rendering — replaces the old `templates::render_prompt`.
///
/// Resolution order:
/// 1. Mode has `components` → assemble from component files
/// 2. Mode has `template_path` → read file + simple token replacement (backward compat)
/// 3. Mode has `template_inline` → use inline string (backward compat)
/// 4. Error
pub async fn render_prompt(
    mode: &Mode,
    slug: &str,
    config: &AtcConfig,
    directive: &str,
    worktree_path: Option<&Path>,
) -> Result<String> {
    let mode_key = mode.as_str();

    if let Some(mode_config) = config.modes.get(mode_key) {
        // Path 1: Component assembly
        if mode_config.components.is_some() {
            let mut prompt = assemble_system_prompt(mode, config, worktree_path).await?;
            if !directive.trim().is_empty() {
                prompt.push_str(&format!(
                    "\n\n---\nAdditional directive: {}",
                    directive
                ));
            }
            return Ok(prompt);
        }

        // Path 2: template_path (backward compat with simple token replacement)
        if let Some(ref path_str) = mode_config.template_path {
            if mode_config.template_inline.is_some() {
                tracing::warn!(
                    mode = mode_key,
                    "both template_path and template_inline set; using template_path"
                );
            }
            let raw = expand_tilde(Path::new(path_str));
            let expanded = if raw.is_relative() {
                if let Some(ref dir) = config.config_dir {
                    dir.join(&raw)
                } else {
                    raw
                }
            } else {
                raw
            };
            let content = tokio::fs::read_to_string(&expanded)
                .await
                .with_context(|| {
                    format!(
                        "failed to read template file '{}' for mode '{}'",
                        expanded.display(),
                        mode_key,
                    )
                })?;
            return Ok(apply_legacy_tokens(&content, slug, directive));
        }

        // Path 3: template_inline (backward compat)
        if let Some(ref inline) = mode_config.template_inline {
            if !inline.trim().is_empty() {
                return Ok(apply_legacy_tokens(inline, slug, directive));
            }
        }
    }

    anyhow::bail!(
        "no template configured for mode '{}': set [modes.{}] components, template_path, or template_inline in atc.toml",
        mode_key,
        mode_key,
    )
}

/// Apply legacy `{{slug}}` and `{{directive}}` token replacement (backward compat).
fn apply_legacy_tokens(template: &str, slug: &str, directive: &str) -> String {
    let template_owns_directive = template.contains("{{directive}}");
    let rendered = template
        .replace("{{slug}}", slug)
        .replace("{{directive}}", directive);
    if directive.is_empty() || template_owns_directive {
        rendered
    } else {
        format!("{rendered}\n\n---\nAdditional directive: {directive}")
    }
}

// --- Frontmatter parsing ---

#[derive(Debug, Default)]
struct Frontmatter {
    #[allow(dead_code)]
    description: Option<String>,
    directives: Vec<String>,
}

fn split_frontmatter(raw: &str) -> Result<(Frontmatter, &str)> {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("---") {
        // No frontmatter — entire content is the body
        return Ok((Frontmatter::default(), raw));
    }

    // Find the closing `---` delimiter: must be a line containing only `---`
    // (with optional trailing whitespace). This avoids false matches on YAML
    // values that contain `---` within block scalars.
    let after_first = &trimmed[3..];
    let closing = after_first
        .lines()
        .enumerate()
        .find(|(_, line)| line.trim() == "---")
        .map(|(i, _)| {
            // Calculate byte offset: sum of preceding lines + newlines
            after_first
                .lines()
                .take(i)
                .map(|l| l.len() + 1) // +1 for the '\n'
                .sum::<usize>()
        })
        .with_context(|| "template has opening `---` but no closing `---`")?;

    let yaml_str = &after_first[..closing];
    let body_start = 3 + closing + 4; // skip "\n---"
    let body = trimmed[body_start..].trim_start_matches('\n');

    // Parse YAML
    let yaml: serde_yaml::Value =
        serde_yaml::from_str(yaml_str).context("failed to parse template YAML frontmatter")?;

    let description = yaml
        .get("description")
        .and_then(|v| v.as_str())
        .map(String::from);

    let directives = yaml
        .get("directives")
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    Ok((
        Frontmatter {
            description,
            directives,
        },
        body,
    ))
}

// --- Partial resolution ---

/// Build a Handlebars registry with partials registered from the 3-level priority chain.
async fn build_registry(
    config: &AtcConfig,
    worktree_path: Option<&Path>,
) -> Result<Handlebars<'static>> {
    let mut hbs = Handlebars::new();
    hbs.set_strict_mode(true);
    // We produce markdown, not HTML — disable escaping
    hbs.register_escape_fn(handlebars::no_escape);

    let components_dir = resolve_dir(&config.prompt.components_dir, config.config_dir.as_deref());
    let partials_dir = resolve_dir(&config.prompt.partials_dir, config.config_dir.as_deref());

    // Collect all partial names from all 3 levels. Lower-priority sources are registered
    // first so higher-priority sources overwrite them.
    // Level 3 (lowest): components dir (strip `# Agent: ...` header)
    register_partials_from_dir(&mut hbs, &components_dir, true).await;
    // Level 2: root partials dir
    register_partials_from_dir(&mut hbs, &partials_dir, false).await;
    // Level 1 (highest): project-specific `.dispatch/partials/`
    if let Some(wt) = worktree_path {
        let project_partials = wt.join(".dispatch/partials");
        register_partials_from_dir(&mut hbs, &project_partials, false).await;
    }

    Ok(hbs)
}

/// Register all `.md` files in a directory as Handlebars partials.
/// If `strip_agent_header` is true, removes the leading `# Agent: ...` line.
async fn register_partials_from_dir(
    hbs: &mut Handlebars<'static>,
    dir: &Path,
    strip_agent_header: bool,
) {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(_) => return, // Directory doesn't exist — skip silently
    };

    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "error iterating partials directory");
                break;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to read partial");
                continue;
            }
        };
        let content = if strip_agent_header {
            strip_agent_header_line(&content)
        } else {
            content
        };
        if let Err(e) = hbs.register_partial(&name, &content) {
            tracing::warn!(name = %name, error = %e, "failed to register partial");
        }
    }
}

/// Strip the leading `# Agent: ...` header line from a component file.
fn strip_agent_header_line(content: &str) -> String {
    if content.starts_with("# Agent:") {
        // Skip the first line
        match content.find('\n') {
            Some(pos) => content[pos + 1..].trim_start_matches('\n').to_string(),
            None => String::new(),
        }
    } else {
        content.to_string()
    }
}

/// Resolve a directory path relative to config_dir if it's relative.
fn resolve_dir(dir: &str, config_dir: Option<&Path>) -> PathBuf {
    let p = expand_tilde(Path::new(dir));
    if p.is_absolute() {
        p
    } else if let Some(base) = config_dir {
        base.join(p)
    } else {
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModeConfig, PromptConfig};
    use std::collections::HashMap;

    // --- split_frontmatter tests ---

    #[test]
    fn test_split_frontmatter_with_yaml() {
        let raw = r#"---
description: "Test template"
directives: [code-read, code-write]
---
Body content here."#;
        let (fm, body) = split_frontmatter(raw).unwrap();
        assert_eq!(fm.description.as_deref(), Some("Test template"));
        assert_eq!(fm.directives, vec!["code-read", "code-write"]);
        assert_eq!(body, "Body content here.");
    }

    #[test]
    fn test_split_frontmatter_no_frontmatter() {
        let raw = "Just body content, no frontmatter.";
        let (fm, body) = split_frontmatter(raw).unwrap();
        assert!(fm.description.is_none());
        assert!(fm.directives.is_empty());
        assert_eq!(body, raw);
    }

    #[test]
    fn test_split_frontmatter_empty_directives() {
        let raw = "---\ndescription: \"hello\"\n---\nBody.";
        let (fm, body) = split_frontmatter(raw).unwrap();
        assert_eq!(fm.description.as_deref(), Some("hello"));
        assert!(fm.directives.is_empty());
        assert_eq!(body, "Body.");
    }

    // --- strip_agent_header_line tests ---

    #[test]
    fn test_strip_agent_header() {
        let content = "# Agent: Base\n\nYou are an agent.";
        assert_eq!(strip_agent_header_line(content), "You are an agent.");
    }

    #[test]
    fn test_strip_agent_header_no_header() {
        let content = "Just some content.";
        assert_eq!(strip_agent_header_line(content), "Just some content.");
    }

    // --- apply_legacy_tokens tests ---

    #[test]
    fn test_legacy_tokens_slug_only() {
        let result = apply_legacy_tokens("Task: {{slug}}", "tasks/t-1", "");
        assert_eq!(result, "Task: tasks/t-1");
    }

    #[test]
    fn test_legacy_tokens_directive_in_template() {
        let result = apply_legacy_tokens("Task: {{slug}} dir: {{directive}}", "tasks/t-1", "focus");
        assert_eq!(result, "Task: tasks/t-1 dir: focus");
        assert!(!result.contains("Additional directive"));
    }

    #[test]
    fn test_legacy_tokens_directive_appended() {
        let result = apply_legacy_tokens("Task: {{slug}}", "tasks/t-1", "focus");
        assert!(result.contains("Additional directive: focus"));
    }

    // --- assemble_system_prompt tests ---

    #[tokio::test]
    async fn test_assemble_components() {
        let dir = tempfile::tempdir().unwrap();
        let comp_dir = dir.path().join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();
        std::fs::write(comp_dir.join("base.md"), "# Agent: Base\n\nBase content.").unwrap();
        std::fs::write(comp_dir.join("git.md"), "# Agent: Git\n\nGit content.").unwrap();

        // Also create partials dir (empty is fine)
        let partials_dir = dir.path().join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();

        let mut modes = HashMap::new();
        modes.insert(
            "implement".to_string(),
            ModeConfig {
                components: Some(vec!["base".to_string(), "git".to_string()]),
                ..Default::default()
            },
        );

        let config = AtcConfig {
            config_dir: Some(dir.path().to_path_buf()),
            prompt: PromptConfig {
                components_dir: "components".to_string(),
                templates_dir: "templates".to_string(),
                partials_dir: "partials".to_string(),
            },
            modes,
            ..Default::default()
        };

        let result = assemble_system_prompt(&Mode::Implement, &config, None)
            .await
            .unwrap();
        assert!(result.contains("Base content."));
        assert!(result.contains("Git content."));
        // Components are concatenated with \n\n
        assert!(result.contains("Base content.\n\n# Agent: Git"));
    }

    #[tokio::test]
    async fn test_assemble_missing_component_errors() {
        let dir = tempfile::tempdir().unwrap();
        let comp_dir = dir.path().join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();

        let partials_dir = dir.path().join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();

        let mut modes = HashMap::new();
        modes.insert(
            "implement".to_string(),
            ModeConfig {
                components: Some(vec!["nonexistent".to_string()]),
                ..Default::default()
            },
        );

        let config = AtcConfig {
            config_dir: Some(dir.path().to_path_buf()),
            prompt: PromptConfig {
                components_dir: "components".to_string(),
                templates_dir: "templates".to_string(),
                partials_dir: "partials".to_string(),
            },
            modes,
            ..Default::default()
        };

        let err = assemble_system_prompt(&Mode::Implement, &config, None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("failed to read component"),
            "unexpected error: {err}"
        );
    }

    // --- render_template tests ---

    #[tokio::test]
    async fn test_render_template_with_frontmatter() {
        let dir = tempfile::tempdir().unwrap();

        // Create partials and components dirs
        let partials_dir = dir.path().join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();
        std::fs::write(partials_dir.join("verify.md"), "Run tests.").unwrap();

        let comp_dir = dir.path().join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();

        let template = r#"---
description: "Test template"
directives: [code-read, git]
---
Working on PR: {{pr}}
{{> verify}}"#;

        let tmpl_path = dir.path().join("test.md");
        std::fs::write(&tmpl_path, template).unwrap();

        let mut params = BTreeMap::new();
        params.insert(
            "pr".to_string(),
            "https://github.com/foo/bar/pull/42".to_string(),
        );

        let config = AtcConfig {
            config_dir: Some(dir.path().to_path_buf()),
            prompt: PromptConfig {
                components_dir: "components".to_string(),
                templates_dir: "templates".to_string(),
                partials_dir: "partials".to_string(),
            },
            ..Default::default()
        };

        let output = render_template(&tmpl_path, &params, &config, None)
            .await
            .unwrap();
        assert!(output.body.contains("https://github.com/foo/bar/pull/42"));
        assert!(output.body.contains("Run tests."));
        assert_eq!(output.directives, vec!["code-read", "git"]);
    }

    #[tokio::test]
    async fn test_render_template_strict_mode_catches_missing_vars() {
        let dir = tempfile::tempdir().unwrap();

        let partials_dir = dir.path().join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();
        let comp_dir = dir.path().join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();

        let template = "---\ndirectives: []\n---\nHello {{missing_var}}";
        let tmpl_path = dir.path().join("test.md");
        std::fs::write(&tmpl_path, template).unwrap();

        let config = AtcConfig {
            config_dir: Some(dir.path().to_path_buf()),
            prompt: PromptConfig {
                components_dir: "components".to_string(),
                templates_dir: "templates".to_string(),
                partials_dir: "partials".to_string(),
            },
            ..Default::default()
        };

        let params = BTreeMap::new();
        let err = render_template(&tmpl_path, &params, &config, None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("render"),
            "expected render error for missing var, got: {err}"
        );
    }

    // --- render_prompt backward compatibility tests ---

    #[tokio::test]
    async fn test_render_prompt_inline_backward_compat() {
        let mut modes = HashMap::new();
        modes.insert(
            "implement".to_string(),
            ModeConfig {
                template_inline: Some("Task: {{slug}}".to_string()),
                ..Default::default()
            },
        );
        let config = AtcConfig {
            modes,
            ..Default::default()
        };
        let result = render_prompt(&Mode::Implement, "tasks/abc", &config, "", None)
            .await
            .unwrap();
        assert_eq!(result, "Task: tasks/abc");
    }

    #[tokio::test]
    async fn test_render_prompt_file_backward_compat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.txt");
        std::fs::write(&path, "File: {{slug}}.").unwrap();

        let mut modes = HashMap::new();
        modes.insert(
            "implement".to_string(),
            ModeConfig {
                template_path: Some(path.to_string_lossy().into_owned()),
                ..Default::default()
            },
        );
        let config = AtcConfig {
            modes,
            ..Default::default()
        };
        let result = render_prompt(&Mode::Implement, "tasks/t", &config, "", None)
            .await
            .unwrap();
        assert_eq!(result, "File: tasks/t.");
    }

    #[tokio::test]
    async fn test_render_prompt_components_takes_priority() {
        let dir = tempfile::tempdir().unwrap();
        let comp_dir = dir.path().join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();
        std::fs::write(comp_dir.join("base.md"), "Component content.").unwrap();

        let partials_dir = dir.path().join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();

        let mut modes = HashMap::new();
        modes.insert(
            "implement".to_string(),
            ModeConfig {
                components: Some(vec!["base".to_string()]),
                template_inline: Some("Inline fallback".to_string()),
                ..Default::default()
            },
        );
        let config = AtcConfig {
            config_dir: Some(dir.path().to_path_buf()),
            prompt: PromptConfig {
                components_dir: "components".to_string(),
                templates_dir: "templates".to_string(),
                partials_dir: "partials".to_string(),
            },
            modes,
            ..Default::default()
        };
        let result = render_prompt(&Mode::Implement, "tasks/t", &config, "", None)
            .await
            .unwrap();
        assert!(result.contains("Component content."));
        assert!(!result.contains("Inline fallback"));
    }

    #[tokio::test]
    async fn test_render_prompt_no_config_errors() {
        let config = AtcConfig::default();
        let err = render_prompt(&Mode::Implement, "tasks/t", &config, "", None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no template configured for mode"),
            "unexpected error: {err}"
        );
    }

    // --- Partial resolution priority tests ---

    #[tokio::test]
    async fn test_partial_resolution_3_levels() {
        let dir = tempfile::tempdir().unwrap();

        // Components (level 3)
        let comp_dir = dir.path().join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();
        std::fs::write(
            comp_dir.join("verify.md"),
            "# Agent: Verify\n\nComponent verify.",
        )
        .unwrap();
        std::fs::write(
            comp_dir.join("only-comp.md"),
            "# Agent: OnlyComp\n\nOnly in components.",
        )
        .unwrap();

        // Root partials (level 2) — overrides component
        let partials_dir = dir.path().join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();
        std::fs::write(partials_dir.join("verify.md"), "Root partial verify.").unwrap();

        // Project partials (level 1) — overrides root
        let wt = dir.path().join("worktree");
        let project_partials = wt.join(".dispatch/partials");
        std::fs::create_dir_all(&project_partials).unwrap();
        std::fs::write(project_partials.join("verify.md"), "Project verify.").unwrap();

        let config = AtcConfig {
            config_dir: Some(dir.path().to_path_buf()),
            prompt: PromptConfig {
                components_dir: "components".to_string(),
                templates_dir: "templates".to_string(),
                partials_dir: "partials".to_string(),
            },
            ..Default::default()
        };

        // With worktree — project partial wins
        let hbs = build_registry(&config, Some(&wt)).await.unwrap();
        let result = hbs
            .render_template("{{> verify}}", &serde_json::json!({}))
            .unwrap();
        assert_eq!(result, "Project verify.");

        // Component fallback works (strips header)
        let result = hbs
            .render_template("{{> only-comp}}", &serde_json::json!({}))
            .unwrap();
        assert_eq!(result, "Only in components.");
    }

    #[tokio::test]
    async fn test_partial_resolution_without_worktree() {
        let dir = tempfile::tempdir().unwrap();

        let comp_dir = dir.path().join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();
        std::fs::write(
            comp_dir.join("verify.md"),
            "# Agent: Verify\n\nComponent verify.",
        )
        .unwrap();

        let partials_dir = dir.path().join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();
        std::fs::write(partials_dir.join("verify.md"), "Root partial verify.").unwrap();

        let config = AtcConfig {
            config_dir: Some(dir.path().to_path_buf()),
            prompt: PromptConfig {
                components_dir: "components".to_string(),
                templates_dir: "templates".to_string(),
                partials_dir: "partials".to_string(),
            },
            ..Default::default()
        };

        let hbs = build_registry(&config, None).await.unwrap();
        let result = hbs
            .render_template("{{> verify}}", &serde_json::json!({}))
            .unwrap();
        // Root partial wins over component
        assert_eq!(result, "Root partial verify.");
    }

    // --- HTML escaping disabled test ---

    #[tokio::test]
    async fn test_no_html_escaping() {
        let dir = tempfile::tempdir().unwrap();
        let partials_dir = dir.path().join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();
        let comp_dir = dir.path().join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();

        let template = "---\ndirectives: []\n---\nCode: {{code}}";
        let tmpl_path = dir.path().join("test.md");
        std::fs::write(&tmpl_path, template).unwrap();

        let config = AtcConfig {
            config_dir: Some(dir.path().to_path_buf()),
            prompt: PromptConfig {
                components_dir: "components".to_string(),
                templates_dir: "templates".to_string(),
                partials_dir: "partials".to_string(),
            },
            ..Default::default()
        };

        let mut params = BTreeMap::new();
        params.insert("code".to_string(), "<div>Hello & World</div>".to_string());

        let output = render_template(&tmpl_path, &params, &config, None)
            .await
            .unwrap();
        // Should NOT escape HTML
        assert_eq!(output.body, "Code: <div>Hello & World</div>");
    }
}
