use anyhow::{Context, Result};
use std::path::Path;

use crate::config::{expand_tilde, AtcConfig};
use crate::terminal_text::display_text;
use crate::types::Directive;

/// Resolve and render the system prompt for a directive.
///
/// 1. Resolve base template (config `template_path` → `template_inline`)
/// 2. Replace `{{slug}}` and `{{directive}}` tokens
/// 3. Append directive tail block only when directive is non-empty AND the
///    template did not contain a `{{directive}}` placeholder (i.e., the
///    template didn't handle placement itself).
pub async fn render_prompt(
    directive: &Directive,
    slug: &str,
    config: &AtcConfig,
    directive_text: &str,
) -> Result<String> {
    let base = resolve_base_template(directive, config).await?;
    let template_owns_directive = base.contains("{{directive}}");
    let rendered = base
        .replace("{{slug}}", slug)
        .replace("{{directive}}", directive_text);
    if directive_text.is_empty() || template_owns_directive {
        Ok(rendered)
    } else {
        Ok(format!(
            "{rendered}\n\n---\nAdditional directive: {directive_text}"
        ))
    }
}

/// Resolve the base template string for a directive using the config override chain:
/// 1. `template_path` from config (file on disk, ~ expanded)
/// 2. `template_inline` from config (non-empty string)
/// 3. Error — no built-in defaults; each project must provide its own templates.
async fn resolve_base_template(directive: &Directive, config: &AtcConfig) -> Result<String> {
    let directive_key = directive.as_str();
    if let Some(directive_config) = config.directives.get(directive_key) {
        if let Some(ref path_str) = directive_config.template_path {
            if directive_config.template_inline.is_some() {
                tracing::warn!(
                    directive = %display_text(directive_key),
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
                        display_text(&expanded.display().to_string()),
                        display_text(directive_key),
                    )
                })?;
            return Ok(content);
        }
        if let Some(ref inline) = directive_config.template_inline {
            if !inline.trim().is_empty() {
                return Ok(inline.clone());
            }
            // Empty string falls through to error
        }
    }
    anyhow::bail!(
        "no template configured for directive '{}': set [directives.{}] template_path or template_inline in atc.toml",
        display_text(directive_key),
        display_text(directive_key),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DirectiveConfig;
    use std::collections::HashMap;

    fn config_with_directive(directive_key: &str, directive_config: DirectiveConfig) -> AtcConfig {
        let mut directives = HashMap::new();
        directives.insert(directive_key.to_string(), directive_config);
        AtcConfig {
            directives,
            ..Default::default()
        }
    }

    fn config_with_inline(directive_key: &str, template: &str) -> AtcConfig {
        config_with_directive(
            directive_key,
            DirectiveConfig {
                template_path: None,
                template_inline: Some(template.to_string()),
                ..Default::default()
            },
        )
    }

    // -- template_path override --

    #[tokio::test]
    async fn test_template_path_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.txt");
        std::fs::write(&path, "Custom prompt for {{slug}}.").unwrap();

        let cfg = config_with_directive(
            "implement",
            DirectiveConfig {
                template_path: Some(path.to_string_lossy().into_owned()),
                template_inline: None,
                ..Default::default()
            },
        );
        let result = render_prompt(&Directive::Implement, "tasks/abc", &cfg, "")
            .await
            .unwrap();
        assert_eq!(result, "Custom prompt for tasks/abc.");
    }

    // -- template_inline override --

    #[tokio::test]
    async fn test_template_inline_override() {
        let cfg = config_with_inline("research", "Inline prompt for {{slug}}.");
        let result = render_prompt(&Directive::Research, "tasks/xyz", &cfg, "")
            .await
            .unwrap();
        assert_eq!(result, "Inline prompt for tasks/xyz.");
    }

    // -- template_path beats template_inline --

    #[tokio::test]
    async fn test_template_path_beats_inline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("winner.txt");
        std::fs::write(&path, "From file.").unwrap();

        let cfg = config_with_directive(
            "implement",
            DirectiveConfig {
                template_path: Some(path.to_string_lossy().into_owned()),
                template_inline: Some("From inline.".to_string()),
                ..Default::default()
            },
        );
        let result = render_prompt(&Directive::Implement, "tasks/t", &cfg, "")
            .await
            .unwrap();
        assert_eq!(result, "From file.");
    }

    // -- {{slug}} replacement --

    #[tokio::test]
    async fn test_slug_replaced() {
        let cfg = config_with_inline("implement", "Working on {{slug}} now.");
        let result = render_prompt(&Directive::Implement, "tasks/gitkb-42", &cfg, "")
            .await
            .unwrap();
        assert_eq!(result, "Working on tasks/gitkb-42 now.");
        assert!(!result.contains("{{slug}}"));
    }

    // -- {{directive}} token replacement in template --

    #[tokio::test]
    async fn test_directive_token_replaced_no_tail_block() {
        let cfg = config_with_inline("implement", "Task {{slug}} directive: {{directive}}");
        let result = render_prompt(&Directive::Implement, "tasks/t", &cfg, "focus on tests")
            .await
            .unwrap();
        // Directive replaced in-place
        assert!(result.contains("directive: focus on tests"));
        // Tail block NOT appended because template owned the directive placement
        assert!(
            !result.contains("---\nAdditional directive:"),
            "tail block should not appear when template contains {{{{directive}}}}: {result}"
        );
    }

    // -- Directive tail block when template lacks {{directive}} --

    #[tokio::test]
    async fn test_directive_appended_when_nonempty_no_placeholder() {
        let cfg = config_with_inline("implement", "Prompt for {{slug}}.");
        let result = render_prompt(&Directive::Implement, "tasks/t", &cfg, "focus on tests")
            .await
            .unwrap();
        assert!(result.contains("---\nAdditional directive: focus on tests"));
    }

    #[tokio::test]
    async fn test_directive_omitted_when_empty() {
        let cfg = config_with_inline("implement", "Prompt for {{slug}}.");
        let result = render_prompt(&Directive::Implement, "tasks/t", &cfg, "")
            .await
            .unwrap();
        assert!(!result.contains("Additional directive"));
        assert!(!result.contains("---\n"));
    }

    // -- Error when template_path not found --

    #[tokio::test]
    async fn test_error_when_template_path_missing() {
        let cfg = config_with_directive(
            "implement",
            DirectiveConfig {
                template_path: Some(
                    "/tmp/nonexistent-atc-template-\x1b[2J\u{202e}gpj.txt".to_string(),
                ),
                template_inline: None,
                ..Default::default()
            },
        );
        let err = render_prompt(&Directive::Implement, "tasks/t", &cfg, "")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("failed to read template file"),
            "unexpected error: {err}"
        );
        assert!(err.contains("\\x1b[2J\\u{202e}gpj"), "got: {err}");
        assert!(!err.contains('\x1b'), "got: {err}");
        assert!(!err.contains('\u{202e}'), "got: {err}");
    }

    // -- Error when no config for directive --

    #[tokio::test]
    async fn test_error_when_no_config() {
        let cfg = AtcConfig::default();
        let err = render_prompt(&Directive::Implement, "tasks/t", &cfg, "")
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("no template configured for directive"),
            "unexpected error: {err}"
        );
    }

    // -- Empty template_inline also errors --

    #[tokio::test]
    async fn test_empty_inline_errors() {
        let cfg = config_with_directive(
            "implement",
            DirectiveConfig {
                template_path: None,
                template_inline: Some(String::new()),
                ..Default::default()
            },
        );
        let err = render_prompt(&Directive::Implement, "tasks/t", &cfg, "")
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("no template configured for directive"),
            "unexpected error: {err}"
        );
    }

    // -- Whitespace-only template_inline also errors --

    #[tokio::test]
    async fn test_whitespace_only_inline_errors() {
        let cfg = config_with_directive(
            "implement",
            DirectiveConfig {
                template_path: None,
                template_inline: Some("   \n\t  ".to_string()),
                ..Default::default()
            },
        );
        let err = render_prompt(&Directive::Implement, "tasks/t", &cfg, "")
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("no template configured for directive"),
            "unexpected error: {err}"
        );
    }

    // -- Relative template_path resolved against config_dir --

    #[tokio::test]
    async fn test_relative_template_path_resolved_against_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("templates");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("impl.txt"), "Relative template for {{slug}}.").unwrap();

        let mut cfg = config_with_directive(
            "implement",
            DirectiveConfig {
                template_path: Some("templates/impl.txt".to_string()),
                template_inline: None,
                ..Default::default()
            },
        );
        cfg.config_dir = Some(dir.path().to_path_buf());

        let result = render_prompt(&Directive::Implement, "tasks/t", &cfg, "")
            .await
            .unwrap();
        assert_eq!(result, "Relative template for tasks/t.");
    }

    // -- All 7 directives resolve when configured --

    #[tokio::test]
    async fn test_all_directives_resolve_with_inline_config() {
        let all_directives = [
            ("implement", Directive::Implement),
            ("research", Directive::Research),
            ("kb-update", Directive::KbUpdate),
            ("review-fix", Directive::ReviewFix),
            ("pr-comments", Directive::PrComments),
            ("refine", Directive::Refine),
            ("create-task", Directive::CreateTask),
            ("close", Directive::Close),
        ];
        for (key, d) in &all_directives {
            let cfg =
                config_with_inline(key, &format!("Template for {{{{slug}}}} directive {key}."));
            let result = render_prompt(d, "tasks/test-1", &cfg, "").await.unwrap();
            assert!(
                result.contains("tasks/test-1"),
                "directive {key} did not substitute slug: {result}"
            );
        }
    }

    // -- All 7 directives error without config --

    #[tokio::test]
    async fn test_all_directives_error_without_config() {
        let cfg = AtcConfig::default();
        let all_directives = [
            Directive::Implement,
            Directive::Research,
            Directive::KbUpdate,
            Directive::ReviewFix,
            Directive::PrComments,
            Directive::Refine,
            Directive::CreateTask,
            Directive::Close,
        ];
        for d in &all_directives {
            let err = render_prompt(d, "tasks/t", &cfg, "").await.unwrap_err();
            assert!(
                err.to_string().contains("no template configured"),
                "directive {} should error without config: {err}",
                d.as_str()
            );
        }
    }
}
