use anyhow::{Context, Result};
use handlebars::Handlebars;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::{expand_tilde, AtcConfig};
use crate::types::Directive;

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
    directive: &Directive,
    slug: &str,
    directive_text: &str,
    config: &AtcConfig,
    worktree_path: Option<&Path>,
) -> Result<String> {
    let directive_key = directive.as_str();
    let directive_config = config
        .directives
        .get(directive_key)
        .with_context(|| format!("no directive config for '{directive_key}'"))?;

    let components = directive_config
        .components
        .as_ref()
        .with_context(|| format!("directive '{directive_key}' has no components list"))?;

    let components_dir = resolve_dir(&config.prompt.components_dir, config.config_dir.as_deref());

    let mut parts = Vec::with_capacity(components.len());
    for name in components {
        let path = components_dir.join(format!("{name}.md"));
        let content = tokio::fs::read_to_string(&path).await.with_context(|| {
            format!("failed to read component '{name}' at '{}'", path.display())
        })?;
        // Strip `# Agent: ...` header for consistency with partial rendering
        parts.push(strip_agent_header_line(&content));
    }

    let assembled = parts.join("\n\n");

    // Check if any component already embeds {{directive}} inline before
    // Handlebars expansion replaces it — if so, skip the trailing append.
    let template_owns_directive = assembled.contains("{{directive}}");

    // Expand partials in the assembled prompt
    let hbs = build_registry(config, worktree_path).await?;
    let mut rendered = hbs
        .render_template(
            &assembled,
            &serde_json::json!({ "slug": slug, "directive": directive_text }),
        )
        .with_context(|| {
            format!("failed to expand partials in assembled prompt for directive '{directive_key}'")
        })?;

    // Append the directive only if not already present — either inlined via
    // {{directive}} in a component or expanded through a partial.
    if !directive_text.trim().is_empty()
        && !template_owns_directive
        && !rendered.contains(directive_text)
    {
        rendered.push_str(&format!(
            "\n\n---\nAdditional directive: {}",
            directive_text
        ));
    }

    Ok(rendered)
}

/// Prefix used for deferred template variables. Provider-injected vars like
/// `{{prefetch}}` aren't available at template-render time; they're replaced
/// by the pipeline after providers run. This sentinel lets them pass through
/// Handlebars strict mode without error.
pub const DEFERRED_VAR_PREFIX: &str = "__ATC_DEFER_";
/// Suffix for deferred variable placeholders.
pub const DEFERRED_VAR_SUFFIX: &str = "__";

/// Build a deferred placeholder string for a variable name.
pub fn deferred_placeholder(var: &str) -> String {
    format!("{}{}{}", DEFERRED_VAR_PREFIX, var, DEFERRED_VAR_SUFFIX)
}

/// Render a Handlebars template file with YAML frontmatter.
///
/// This is a standalone entry point for callers that need full Handlebars
/// rendering (variables + partials) from a template file. It is **not** used
/// by the legacy `template_path` dispatch path, which intentionally uses
/// simple `{{slug}}`/`{{directive}}` token replacement for backward
/// compatibility. Use this function directly when you need rich templating.
///
/// `deferred_vars` lists variable names that will be injected by providers
/// after rendering. These are rendered as sentinel placeholders so Handlebars
/// strict mode doesn't reject them. The pipeline substitutes them later.
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
    render_template_with_deferred(template_path, params, &[], config, worktree_path).await
}

/// Like [`render_template`] but accepts a list of deferred variable names.
pub async fn render_template_with_deferred(
    template_path: &Path,
    params: &BTreeMap<String, String>,
    deferred_vars: &[&str],
    config: &AtcConfig,
    worktree_path: Option<&Path>,
) -> Result<TemplateOutput> {
    // Resolve relative template paths against `prompt.templates_dir`.
    let resolved = if template_path.is_relative() {
        resolve_dir(&config.prompt.templates_dir, config.config_dir.as_deref()).join(template_path)
    } else {
        template_path.to_path_buf()
    };
    let raw = tokio::fs::read_to_string(&resolved)
        .await
        .with_context(|| format!("failed to read template file '{}'", resolved.display()))?;

    let (frontmatter, body) = split_frontmatter(&raw)?;

    // Inject deferred placeholders for provider-supplied vars so Handlebars
    // strict mode doesn't reject them. The pipeline replaces these after
    // providers run.
    let mut effective_params = params.clone();
    for var in deferred_vars {
        if !effective_params.contains_key(*var) {
            effective_params.insert(var.to_string(), deferred_placeholder(var));
        }
    }

    let hbs = build_registry(config, worktree_path).await?;
    let rendered = hbs
        .render_template(body, &effective_params)
        .with_context(|| format!("failed to render template '{}'", template_path.display()))?;

    Ok(TemplateOutput {
        body: rendered,
        directives: frontmatter.directives,
    })
}

/// Top-level prompt rendering — replaces the old `templates::render_prompt`.
///
/// Resolution order:
/// 1. Directive has `components` → assemble from component files
/// 2. Directive has `template_path` → read file + simple token replacement (backward compat)
/// 3. Directive has `template_inline` → use inline string (backward compat)
/// 4. Error
pub async fn render_prompt(
    directive: &Directive,
    slug: &str,
    config: &AtcConfig,
    directive_text: &str,
    worktree_path: Option<&Path>,
) -> Result<String> {
    let directive_key = directive.as_str();

    if let Some(directive_config) = config.directives.get(directive_key) {
        // Path 1: Component assembly
        if directive_config.components.is_some() {
            let prompt =
                assemble_system_prompt(directive, slug, directive_text, config, worktree_path)
                    .await?;
            return Ok(prompt);
        }

        // Path 2: template_path (backward compat with simple token replacement)
        if let Some(ref path_str) = directive_config.template_path {
            if directive_config.template_inline.is_some() {
                tracing::warn!(
                    directive = directive_key,
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
                        "failed to read template file '{}' for directive '{}'",
                        expanded.display(),
                        directive_key,
                    )
                })?;
            return Ok(apply_legacy_tokens(&content, slug, directive_text));
        }

        // Path 3: template_inline (backward compat)
        if let Some(ref inline) = directive_config.template_inline {
            if !inline.trim().is_empty() {
                return Ok(apply_legacy_tokens(inline, slug, directive_text));
            }
        }
    }

    anyhow::bail!(
        "no template configured for directive '{}': set [directives.{}] components, template_path, or template_inline in atc.toml",
        directive_key,
        directive_key,
    )
}

/// Apply legacy `{{slug}}` and `{{directive}}` token replacement (backward compat).
fn apply_legacy_tokens(template: &str, slug: &str, directive: &str) -> String {
    let template_owns_directive = template.contains("{{directive}}");
    let rendered = template
        .replace("{{slug}}", slug)
        .replace("{{directive}}", directive);
    if directive.trim().is_empty() || template_owns_directive {
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
    // Strip optional UTF-8 BOM but do NOT trim whitespace — a file that starts
    // with blank lines followed by `---` is body content, not frontmatter.
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);

    // The opening fence must be exactly `---` followed by a newline (LF or CRLF).
    let after_open = if let Some(rest) = raw.strip_prefix("---\n") {
        rest
    } else if let Some(rest) = raw.strip_prefix("---\r\n") {
        rest
    } else {
        // No frontmatter — entire content is the body
        return Ok((Frontmatter::default(), raw));
    };

    // Scan for the closing fence: an unindented line containing only `---`
    // (with optional trailing whitespace). Uses `split_inclusive` to preserve
    // exact byte boundaries regardless of LF vs CRLF line endings.
    let mut yaml_end = None;
    let mut body_start = None;
    let mut offset = 0usize;
    for line in after_open.split_inclusive('\n') {
        let line_no_nl = line.strip_suffix('\n').unwrap_or(line);
        let logical_line = line_no_nl
            .strip_suffix('\r')
            .unwrap_or(line_no_nl)
            .trim_end_matches([' ', '\t']);
        if logical_line == "---" {
            yaml_end = Some(offset);
            body_start = Some(offset + line.len());
            break;
        }
        offset += line.len();
    }

    let yaml_end = yaml_end.with_context(|| "template has opening `---` but no closing `---`")?;
    let body_start = body_start.expect("closing fence sets body_start");
    let yaml_str = &after_open[..yaml_end];
    let body = after_open[body_start..].trim_start_matches(['\r', '\n']);

    // Parse YAML
    let yaml: serde_yml::Value =
        serde_yml::from_str(yaml_str).context("failed to parse template YAML frontmatter")?;

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

/// Build a Handlebars registry with partials from components, root, and
/// project directories.
///
/// **Performance note:** This rescans the filesystem on every call. If you are
/// rendering multiple prompts with the same configuration (e.g. batch dispatch),
/// consider calling this once and reusing the returned registry.
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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "cannot read partials directory");
            return;
        }
    };

    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "error iterating partials directory");
                continue;
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
            tracing::warn!(
                name = %name,
                path = %path.display(),
                error = %e,
                "failed to register partial; templates referencing {{{{> {name}}}}} will fail at render time",
            );
        }
    }
}

/// Strip the leading `# Agent: ...` header line from a component file.
fn strip_agent_header_line(content: &str) -> String {
    if content.starts_with("# Agent:") {
        // Skip the first line
        match content.find('\n') {
            Some(pos) => content[pos + 1..]
                .trim_start_matches(['\r', '\n'])
                .to_string(),
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
    use crate::config::{DirectiveConfig, PromptConfig};
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
    fn test_split_frontmatter_with_dashes_in_yaml_value() {
        let raw = "---\ndescription: |\n  --- this line starts with dashes\ndirectives: [code-read]\n---\nBody here.";
        let (fm, body) = split_frontmatter(raw).unwrap();
        assert_eq!(fm.directives, vec!["code-read"]);
        assert_eq!(body, "Body here.");
    }

    #[test]
    fn test_split_frontmatter_crlf_line_endings() {
        let raw =
            "---\r\ndescription: \"hello\"\r\ndirectives: [code-read]\r\n---\r\nBody with CRLF.";
        let (fm, body) = split_frontmatter(raw).unwrap();
        assert_eq!(fm.description.as_deref(), Some("hello"));
        assert_eq!(fm.directives, vec!["code-read"]);
        assert_eq!(body, "Body with CRLF.");
    }

    #[test]
    fn test_split_frontmatter_indented_dashes_not_closing_fence() {
        // Indented `---` inside a YAML block scalar must NOT be treated as the closing fence.
        let raw = "---\ndescription: |\n  ---\n  indented dashes\ndirectives: []\n---\nBody.";
        let (fm, body) = split_frontmatter(raw).unwrap();
        assert!(fm.description.is_some());
        assert_eq!(body, "Body.");
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

        let mut directives = HashMap::new();
        directives.insert(
            "implement".to_string(),
            DirectiveConfig {
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
            directives,
            ..Default::default()
        };

        let result = assemble_system_prompt(&Directive::Implement, "test-slug", "", &config, None)
            .await
            .unwrap();
        assert!(result.contains("Base content."));
        assert!(result.contains("Git content."));
        // Agent headers are stripped for consistency with partial rendering
        assert!(
            !result.contains("# Agent: Base"),
            "agent header should be stripped"
        );
        assert!(
            !result.contains("# Agent: Git"),
            "agent header should be stripped"
        );
    }

    #[tokio::test]
    async fn test_assemble_missing_component_errors() {
        let dir = tempfile::tempdir().unwrap();
        let comp_dir = dir.path().join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();

        let partials_dir = dir.path().join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();

        let mut directives = HashMap::new();
        directives.insert(
            "implement".to_string(),
            DirectiveConfig {
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
            directives,
            ..Default::default()
        };

        let err = assemble_system_prompt(&Directive::Implement, "test-slug", "", &config, None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("failed to read component"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_assemble_components_expands_slug() {
        let dir = tempfile::tempdir().unwrap();

        let comp_dir = dir.path().join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();
        std::fs::write(comp_dir.join("base.md"), "Task slug: {{slug}}").unwrap();

        let partials_dir = dir.path().join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();

        let mut directives = HashMap::new();
        directives.insert(
            "implement".to_string(),
            DirectiveConfig {
                components: Some(vec!["base".to_string()]),
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
            directives,
            ..Default::default()
        };

        let result = assemble_system_prompt(&Directive::Implement, "my-task", "", &config, None)
            .await
            .unwrap();
        assert!(
            result.contains("Task slug: my-task"),
            "slug should be expanded in component output, got: {result}"
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

    #[tokio::test]
    async fn test_render_template_deferred_vars_pass_through_strict_mode() {
        let dir = tempfile::tempdir().unwrap();

        let partials_dir = dir.path().join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();
        let comp_dir = dir.path().join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();

        // Template references both a user-supplied var and a deferred provider var
        let template = "---\ndirectives: [review-fix]\n---\nPR: {{pr_url}}\nContext: {{prefetch}}";
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
        params.insert(
            "pr_url".to_string(),
            "https://github.com/org/repo/pull/1".to_string(),
        );

        // Without deferred_vars, this should fail (strict mode)
        let err = render_template(&tmpl_path, &params, &config, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("render"), "got: {err}");

        // With deferred_vars, it should succeed with a placeholder
        let output =
            render_template_with_deferred(&tmpl_path, &params, &["prefetch"], &config, None)
                .await
                .unwrap();
        assert!(output.body.contains("https://github.com/org/repo/pull/1"));
        assert!(output.body.contains(&deferred_placeholder("prefetch")));
        // User-supplied var should be resolved, not deferred
        assert!(!output.body.contains("{{pr_url}}"));
    }

    #[test]
    fn test_deferred_placeholder_format() {
        assert_eq!(deferred_placeholder("prefetch"), "__ATC_DEFER_prefetch__");
    }

    #[tokio::test]
    async fn test_deferred_var_not_overridden_when_user_supplies_it() {
        // If the user explicitly provides a value for a "deferred" var,
        // the user value should win.
        let dir = tempfile::tempdir().unwrap();
        let partials_dir = dir.path().join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();
        let comp_dir = dir.path().join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();

        let template = "---\ndirectives: []\n---\nData: {{prefetch}}";
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
        params.insert("prefetch".to_string(), "user-provided-value".to_string());

        let output =
            render_template_with_deferred(&tmpl_path, &params, &["prefetch"], &config, None)
                .await
                .unwrap();
        assert!(output.body.contains("user-provided-value"));
        assert!(!output.body.contains("__ATC_DEFER_"));
    }

    // --- render_prompt backward compatibility tests ---

    #[tokio::test]
    async fn test_render_prompt_inline_backward_compat() {
        let mut directives = HashMap::new();
        directives.insert(
            "implement".to_string(),
            DirectiveConfig {
                template_inline: Some("Task: {{slug}}".to_string()),
                ..Default::default()
            },
        );
        let config = AtcConfig {
            directives,
            ..Default::default()
        };
        let result = render_prompt(&Directive::Implement, "tasks/abc", &config, "", None)
            .await
            .unwrap();
        assert_eq!(result, "Task: tasks/abc");
    }

    #[tokio::test]
    async fn test_render_prompt_file_backward_compat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.txt");
        std::fs::write(&path, "File: {{slug}}.").unwrap();

        let mut directives = HashMap::new();
        directives.insert(
            "implement".to_string(),
            DirectiveConfig {
                template_path: Some(path.to_string_lossy().into_owned()),
                ..Default::default()
            },
        );
        let config = AtcConfig {
            directives,
            ..Default::default()
        };
        let result = render_prompt(&Directive::Implement, "tasks/t", &config, "", None)
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

        let mut directives = HashMap::new();
        directives.insert(
            "implement".to_string(),
            DirectiveConfig {
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
            directives,
            ..Default::default()
        };
        let result = render_prompt(&Directive::Implement, "tasks/t", &config, "", None)
            .await
            .unwrap();
        assert!(result.contains("Component content."));
        assert!(!result.contains("Inline fallback"));
    }

    #[tokio::test]
    async fn test_render_prompt_components_with_directive() {
        let dir = tempfile::tempdir().unwrap();
        let comp_dir = dir.path().join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();
        std::fs::write(comp_dir.join("base.md"), "Component content.").unwrap();

        let partials_dir = dir.path().join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();

        let mut directives = HashMap::new();
        directives.insert(
            "implement".to_string(),
            DirectiveConfig {
                components: Some(vec!["base".to_string()]),
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
            directives,
            ..Default::default()
        };
        let result = render_prompt(
            &Directive::Implement,
            "tasks/t",
            &config,
            "focus on tests",
            None,
        )
        .await
        .unwrap();
        assert!(result.contains("Component content."));
        assert!(result.contains("Additional directive: focus on tests"));
    }

    #[tokio::test]
    async fn test_render_prompt_components_empty_directive_not_appended() {
        let dir = tempfile::tempdir().unwrap();
        let comp_dir = dir.path().join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();
        std::fs::write(comp_dir.join("base.md"), "Component content.").unwrap();

        let partials_dir = dir.path().join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();

        let mut directives = HashMap::new();
        directives.insert(
            "implement".to_string(),
            DirectiveConfig {
                components: Some(vec!["base".to_string()]),
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
            directives,
            ..Default::default()
        };
        let result = render_prompt(&Directive::Implement, "tasks/t", &config, "", None)
            .await
            .unwrap();
        assert!(result.contains("Component content."));
        assert!(!result.contains("Additional directive"));
    }

    #[tokio::test]
    async fn test_assemble_directive_available_in_handlebars_context() {
        let dir = tempfile::tempdir().unwrap();
        let comp_dir = dir.path().join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();
        std::fs::write(
            comp_dir.join("base.md"),
            "Directive: {{directive}}, Slug: {{slug}}",
        )
        .unwrap();

        let partials_dir = dir.path().join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();

        let mut directives = HashMap::new();
        directives.insert(
            "implement".to_string(),
            DirectiveConfig {
                components: Some(vec!["base".to_string()]),
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
            directives,
            ..Default::default()
        };

        let result = assemble_system_prompt(
            &Directive::Implement,
            "my-task",
            "focus on tests",
            &config,
            None,
        )
        .await
        .unwrap();
        assert!(
            result.contains("Directive: focus on tests"),
            "directive should be available in Handlebars context, got: {result}"
        );
        assert!(
            result.contains("Slug: my-task"),
            "slug should be available in Handlebars context, got: {result}"
        );
    }

    #[tokio::test]
    async fn test_render_prompt_components_directive_not_duplicated_when_embedded() {
        let dir = tempfile::tempdir().unwrap();
        let comp_dir = dir.path().join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();
        // Component embeds {{directive}} inline — should NOT get appended again
        std::fs::write(comp_dir.join("base.md"), "Inline directive: {{directive}}").unwrap();

        let partials_dir = dir.path().join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();

        let mut directives = HashMap::new();
        directives.insert(
            "implement".to_string(),
            DirectiveConfig {
                components: Some(vec!["base".to_string()]),
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
            directives,
            ..Default::default()
        };
        let result = render_prompt(
            &Directive::Implement,
            "tasks/t",
            &config,
            "focus on tests",
            None,
        )
        .await
        .unwrap();
        assert!(
            result.contains("Inline directive: focus on tests"),
            "directive should be expanded inline, got: {result}"
        );
        assert!(
            !result.contains("Additional directive"),
            "directive should NOT be appended when template owns it, got: {result}"
        );
    }

    #[tokio::test]
    async fn test_directive_not_duplicated_when_partial_embeds_it() {
        // A partial expands {{directive}} — the append check should detect
        // the directive text in the rendered output and skip the trailing append.
        let dir = tempfile::tempdir().unwrap();
        let comp_dir = dir.path().join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();
        // Component invokes a partial; no inline {{directive}} here.
        std::fs::write(comp_dir.join("base.md"), "Preamble\n\n{{> instructions}}").unwrap();

        let partials_dir = dir.path().join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();
        // The partial itself embeds {{directive}}.
        std::fs::write(partials_dir.join("instructions.md"), "Focus: {{directive}}").unwrap();

        let mut directives = HashMap::new();
        directives.insert(
            "implement".to_string(),
            DirectiveConfig {
                components: Some(vec!["base".to_string()]),
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
            directives,
            ..Default::default()
        };
        let result = render_prompt(
            &Directive::Implement,
            "tasks/t",
            &config,
            "write tests",
            None,
        )
        .await
        .unwrap();
        assert!(
            result.contains("Focus: write tests"),
            "partial should expand directive, got: {result}"
        );
        assert!(
            !result.contains("Additional directive"),
            "directive should NOT be appended when partial already contains it, got: {result}"
        );
    }

    #[test]
    fn test_split_frontmatter_bom_stripped() {
        let raw = "\u{feff}---\ndescription: \"bom\"\n---\nBody after BOM.";
        let (fm, body) = split_frontmatter(raw).unwrap();
        assert_eq!(fm.description.as_deref(), Some("bom"));
        assert_eq!(body, "Body after BOM.");
    }

    #[test]
    fn test_split_frontmatter_leading_whitespace_is_body() {
        // Leading whitespace followed by --- should NOT be treated as frontmatter.
        let raw = "\n\n---\nThis is body content.";
        let (fm, body) = split_frontmatter(raw).unwrap();
        assert!(fm.description.is_none());
        assert!(fm.directives.is_empty());
        assert_eq!(body, raw);
    }

    #[tokio::test]
    async fn test_assemble_strips_agent_headers() {
        let dir = tempfile::tempdir().unwrap();
        let comp_dir = dir.path().join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();
        std::fs::write(comp_dir.join("base.md"), "# Agent: Base\n\nBase body.").unwrap();
        std::fs::write(comp_dir.join("git.md"), "# Agent: Git\n\nGit body.").unwrap();

        let partials_dir = dir.path().join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();

        let mut directives = HashMap::new();
        directives.insert(
            "implement".to_string(),
            DirectiveConfig {
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
            directives,
            ..Default::default()
        };

        let result = assemble_system_prompt(&Directive::Implement, "s", "", &config, None)
            .await
            .unwrap();
        assert!(
            !result.contains("# Agent:"),
            "agent headers should be stripped in assembled output, got: {result}"
        );
        assert!(result.contains("Base body."));
        assert!(result.contains("Git body."));
    }

    #[tokio::test]
    async fn test_render_prompt_no_config_errors() {
        let config = AtcConfig::default();
        let err = render_prompt(&Directive::Implement, "tasks/t", &config, "", None)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("no template configured for directive"),
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
