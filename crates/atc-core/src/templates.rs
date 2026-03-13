use anyhow::{Context, Result};
use std::path::Path;

use crate::config::{expand_tilde, AtcConfig};
use crate::types::Mode;

/// Resolve and render the system prompt for a mode.
///
/// 1. Resolve base template (config `template_path` → `template_inline`)
/// 2. Replace `{{slug}}` and `{{directive}}` tokens
/// 3. Append directive tail block only when directive is non-empty AND the
///    template did not contain a `{{directive}}` placeholder (i.e., the
///    template didn't handle placement itself).
pub async fn render_prompt(
    mode: &Mode,
    slug: &str,
    config: &AtcConfig,
    directive: &str,
) -> Result<String> {
    let base = resolve_base_template(mode, config).await?;
    let template_owns_directive = base.contains("{{directive}}");
    let rendered = base
        .replace("{{slug}}", slug)
        .replace("{{directive}}", directive);
    if directive.is_empty() || template_owns_directive {
        Ok(rendered)
    } else {
        Ok(format!(
            "{rendered}\n\n---\nAdditional directive: {directive}"
        ))
    }
}

/// Resolve the base template string for a mode using the config override chain:
/// 1. `template_path` from config (file on disk, ~ expanded)
/// 2. `template_inline` from config (non-empty string)
/// 3. Error — no built-in defaults; each project must provide its own templates.
async fn resolve_base_template(mode: &Mode, config: &AtcConfig) -> Result<String> {
    let mode_key = mode.as_str();
    if let Some(mode_config) = config.modes.get(mode_key) {
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
            return Ok(content);
        }
        if let Some(ref inline) = mode_config.template_inline {
            if !inline.is_empty() {
                return Ok(inline.clone());
            }
            // Empty string falls through to error
        }
    }
    anyhow::bail!(
        "no template configured for mode '{}': set [modes.{}] template_path or template_inline in atc.toml",
        mode_key,
        mode_key,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModeConfig;
    use std::collections::HashMap;

    fn config_with_mode(mode_key: &str, mode_config: ModeConfig) -> AtcConfig {
        let mut modes = HashMap::new();
        modes.insert(mode_key.to_string(), mode_config);
        AtcConfig {
            modes,
            ..Default::default()
        }
    }

    fn config_with_inline(mode_key: &str, template: &str) -> AtcConfig {
        config_with_mode(
            mode_key,
            ModeConfig {
                template_path: None,
                template_inline: Some(template.to_string()),
            },
        )
    }

    // -- template_path override --

    #[tokio::test]
    async fn test_template_path_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.txt");
        std::fs::write(&path, "Custom prompt for {{slug}}.").unwrap();

        let cfg = config_with_mode(
            "implement",
            ModeConfig {
                template_path: Some(path.to_string_lossy().into_owned()),
                template_inline: None,
            },
        );
        let result = render_prompt(&Mode::Implement, "tasks/abc", &cfg, "")
            .await
            .unwrap();
        assert_eq!(result, "Custom prompt for tasks/abc.");
    }

    // -- template_inline override --

    #[tokio::test]
    async fn test_template_inline_override() {
        let cfg = config_with_inline("research", "Inline prompt for {{slug}}.");
        let result = render_prompt(&Mode::Research, "tasks/xyz", &cfg, "")
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

        let cfg = config_with_mode(
            "implement",
            ModeConfig {
                template_path: Some(path.to_string_lossy().into_owned()),
                template_inline: Some("From inline.".to_string()),
            },
        );
        let result = render_prompt(&Mode::Implement, "tasks/t", &cfg, "")
            .await
            .unwrap();
        assert_eq!(result, "From file.");
    }

    // -- {{slug}} replacement --

    #[tokio::test]
    async fn test_slug_replaced() {
        let cfg = config_with_inline("implement", "Working on {{slug}} now.");
        let result = render_prompt(&Mode::Implement, "tasks/gitkb-42", &cfg, "")
            .await
            .unwrap();
        assert_eq!(result, "Working on tasks/gitkb-42 now.");
        assert!(!result.contains("{{slug}}"));
    }

    // -- {{directive}} token replacement in template --

    #[tokio::test]
    async fn test_directive_token_replaced_no_tail_block() {
        let cfg = config_with_inline("implement", "Task {{slug}} directive: {{directive}}");
        let result = render_prompt(&Mode::Implement, "tasks/t", &cfg, "focus on tests")
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
        let result = render_prompt(&Mode::Implement, "tasks/t", &cfg, "focus on tests")
            .await
            .unwrap();
        assert!(result.contains("---\nAdditional directive: focus on tests"));
    }

    #[tokio::test]
    async fn test_directive_omitted_when_empty() {
        let cfg = config_with_inline("implement", "Prompt for {{slug}}.");
        let result = render_prompt(&Mode::Implement, "tasks/t", &cfg, "")
            .await
            .unwrap();
        assert!(!result.contains("Additional directive"));
        assert!(!result.contains("---\n"));
    }

    // -- Error when template_path not found --

    #[tokio::test]
    async fn test_error_when_template_path_missing() {
        let cfg = config_with_mode(
            "implement",
            ModeConfig {
                template_path: Some("/tmp/nonexistent-atc-template-268.txt".to_string()),
                template_inline: None,
            },
        );
        let err = render_prompt(&Mode::Implement, "tasks/t", &cfg, "")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("failed to read template file"),
            "unexpected error: {err}"
        );
    }

    // -- Error when no config for mode --

    #[tokio::test]
    async fn test_error_when_no_config() {
        let cfg = AtcConfig::default();
        let err = render_prompt(&Mode::Implement, "tasks/t", &cfg, "")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no template configured for mode"),
            "unexpected error: {err}"
        );
    }

    // -- Empty template_inline also errors --

    #[tokio::test]
    async fn test_empty_inline_errors() {
        let cfg = config_with_mode(
            "implement",
            ModeConfig {
                template_path: None,
                template_inline: Some(String::new()),
            },
        );
        let err = render_prompt(&Mode::Implement, "tasks/t", &cfg, "")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no template configured for mode"),
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

        let mut cfg = config_with_mode(
            "implement",
            ModeConfig {
                template_path: Some("templates/impl.txt".to_string()),
                template_inline: None,
            },
        );
        cfg.config_dir = Some(dir.path().to_path_buf());

        let result = render_prompt(&Mode::Implement, "tasks/t", &cfg, "").await.unwrap();
        assert_eq!(result, "Relative template for tasks/t.");
    }

    // -- All 7 modes resolve when configured --

    #[tokio::test]
    async fn test_all_modes_resolve_with_inline_config() {
        let modes = [
            ("implement", Mode::Implement),
            ("research", Mode::Research),
            ("kb-update", Mode::KbUpdate),
            ("review-fix", Mode::ReviewFix),
            ("pr-comments", Mode::PrComments),
            ("refine", Mode::Refine),
            ("create-task", Mode::CreateTask),
        ];
        for (key, mode) in &modes {
            let cfg = config_with_inline(key, &format!("Template for {{{{slug}}}} mode {key}."));
            let result = render_prompt(mode, "tasks/test-1", &cfg, "").await.unwrap();
            assert!(
                result.contains("tasks/test-1"),
                "mode {key} did not substitute slug: {result}"
            );
        }
    }

    // -- All 7 modes error without config --

    #[tokio::test]
    async fn test_all_modes_error_without_config() {
        let cfg = AtcConfig::default();
        let modes = [
            Mode::Implement,
            Mode::Research,
            Mode::KbUpdate,
            Mode::ReviewFix,
            Mode::PrComments,
            Mode::Refine,
            Mode::CreateTask,
        ];
        for mode in &modes {
            let err = render_prompt(mode, "tasks/t", &cfg, "").await.unwrap_err();
            assert!(
                err.to_string().contains("no template configured"),
                "mode {} should error without config: {err}",
                mode.as_str()
            );
        }
    }
}
