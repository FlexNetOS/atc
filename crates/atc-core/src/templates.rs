use anyhow::{Context, Result};
use std::path::Path;

use crate::config::{expand_tilde, AtcConfig};
use crate::types::Mode;

// Built-in default templates, one per mode.
pub const IMPLEMENT: &str = include_str!("templates/implement.txt");
pub const RESEARCH: &str = include_str!("templates/research.txt");
pub const KB_UPDATE: &str = include_str!("templates/kb_update.txt");
pub const REVIEW_FIX: &str = include_str!("templates/review_fix.txt");
pub const PR_COMMENTS: &str = include_str!("templates/pr_comments.txt");
pub const REFINE: &str = include_str!("templates/refine.txt");
pub const CREATE_TASK: &str = include_str!("templates/create_task.txt");

/// Resolve and render the system prompt for a mode.
///
/// 1. Resolve base template (config override → inline override → built-in default)
/// 2. Replace `{{slug}}` and `{{directive}}` tokens
/// 3. Append directive tail block when directive is non-empty
pub fn render_prompt(
    mode: &Mode,
    slug: &str,
    config: &AtcConfig,
    directive: &str,
) -> Result<String> {
    let base = resolve_base_template(mode, config)?;
    let rendered = base
        .replace("{{slug}}", slug)
        .replace("{{directive}}", directive);
    if directive.is_empty() {
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
/// 3. Built-in default via `Mode::default_template()`
fn resolve_base_template(mode: &Mode, config: &AtcConfig) -> Result<String> {
    let mode_key = mode.config_key();
    if let Some(mode_config) = config.modes.get(mode_key) {
        if let Some(ref path_str) = mode_config.template_path {
            if mode_config.template_inline.is_some() {
                tracing::warn!(
                    mode = mode_key,
                    "both template_path and template_inline set; using template_path"
                );
            }
            let expanded = expand_tilde(Path::new(path_str));
            return std::fs::read_to_string(&expanded).with_context(|| {
                format!(
                    "failed to read template file '{}' for mode '{}'",
                    expanded.display(),
                    mode_key,
                )
            });
        }
        if let Some(ref inline) = mode_config.template_inline {
            if !inline.is_empty() {
                return Ok(inline.clone());
            }
            // Empty string falls through to default
        }
    }
    Ok(mode.default_template().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModeConfig;
    use std::collections::HashMap;

    fn default_config() -> AtcConfig {
        AtcConfig::default()
    }

    fn config_with_mode(mode_key: &str, mode_config: ModeConfig) -> AtcConfig {
        let mut modes = HashMap::new();
        modes.insert(mode_key.to_string(), mode_config);
        AtcConfig {
            modes,
            ..Default::default()
        }
    }

    // -- Default template rendering for each mode --

    #[test]
    fn test_default_template_implement() {
        let cfg = default_config();
        let result = render_prompt(&Mode::Implement, "tasks/test-1", &cfg, "").unwrap();
        assert!(result.contains("implementing task tasks/test-1"));
    }

    #[test]
    fn test_default_template_research() {
        let cfg = default_config();
        let result = render_prompt(&Mode::Research, "tasks/test-1", &cfg, "").unwrap();
        assert!(result.contains("researching task tasks/test-1"));
    }

    #[test]
    fn test_default_template_kb_update() {
        let cfg = default_config();
        let result = render_prompt(&Mode::KbUpdate, "tasks/test-1", &cfg, "").unwrap();
        assert!(result.contains("updating the knowledge base for task tasks/test-1"));
    }

    #[test]
    fn test_default_template_review_fix() {
        let cfg = default_config();
        let result = render_prompt(&Mode::ReviewFix, "tasks/test-1", &cfg, "").unwrap();
        assert!(result.contains("addressing review feedback for task tasks/test-1"));
    }

    #[test]
    fn test_default_template_pr_comments() {
        let cfg = default_config();
        let result = render_prompt(&Mode::PrComments, "tasks/test-1", &cfg, "").unwrap();
        assert!(result.contains("addressing pull request comments for task tasks/test-1"));
    }

    #[test]
    fn test_default_template_refine() {
        let cfg = default_config();
        let result = render_prompt(&Mode::Refine, "tasks/test-1", &cfg, "").unwrap();
        assert!(result.contains("refining a KB document for task tasks/test-1"));
    }

    #[test]
    fn test_default_template_create_task() {
        let cfg = default_config();
        let result = render_prompt(&Mode::CreateTask, "tasks/test-1", &cfg, "").unwrap();
        assert!(result.contains("creating a task document for tasks/test-1"));
    }

    // -- template_path override --

    #[test]
    fn test_template_path_override() {
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
        let result = render_prompt(&Mode::Implement, "tasks/abc", &cfg, "").unwrap();
        assert_eq!(result, "Custom prompt for tasks/abc.");
    }

    // -- template_inline override --

    #[test]
    fn test_template_inline_override() {
        let cfg = config_with_mode(
            "research",
            ModeConfig {
                template_path: None,
                template_inline: Some("Inline prompt for {{slug}}.".to_string()),
            },
        );
        let result = render_prompt(&Mode::Research, "tasks/xyz", &cfg, "").unwrap();
        assert_eq!(result, "Inline prompt for tasks/xyz.");
    }

    // -- template_path beats template_inline --

    #[test]
    fn test_template_path_beats_inline() {
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
        let result = render_prompt(&Mode::Implement, "tasks/t", &cfg, "").unwrap();
        assert_eq!(result, "From file.");
    }

    // -- {{slug}} replacement --

    #[test]
    fn test_slug_replaced() {
        let cfg = default_config();
        let result = render_prompt(&Mode::Implement, "tasks/gitkb-42", &cfg, "").unwrap();
        assert!(result.contains("tasks/gitkb-42"));
        assert!(!result.contains("{{slug}}"));
    }

    // -- Directive tail block --

    #[test]
    fn test_directive_appended_when_nonempty() {
        let cfg = default_config();
        let result = render_prompt(&Mode::Implement, "tasks/t", &cfg, "focus on tests").unwrap();
        assert!(result.contains("---\nAdditional directive: focus on tests"));
    }

    #[test]
    fn test_directive_omitted_when_empty() {
        let cfg = default_config();
        let result = render_prompt(&Mode::Implement, "tasks/t", &cfg, "").unwrap();
        assert!(!result.contains("Additional directive"));
        assert!(!result.contains("---\n"));
    }

    // -- Error when template_path not found --

    #[test]
    fn test_error_when_template_path_missing() {
        let cfg = config_with_mode(
            "implement",
            ModeConfig {
                template_path: Some("/tmp/nonexistent-atc-template-268.txt".to_string()),
                template_inline: None,
            },
        );
        let err = render_prompt(&Mode::Implement, "tasks/t", &cfg, "").unwrap_err();
        assert!(
            err.to_string().contains("failed to read template file"),
            "unexpected error: {err}"
        );
    }

    // -- No error when config is absent (all defaults) --

    #[test]
    fn test_no_error_when_config_absent() {
        let cfg = default_config();
        let result = render_prompt(&Mode::Implement, "tasks/t", &cfg, "");
        assert!(result.is_ok());
    }

    // -- Empty template_inline falls through to default --

    #[test]
    fn test_empty_inline_falls_through() {
        let cfg = config_with_mode(
            "implement",
            ModeConfig {
                template_path: None,
                template_inline: Some(String::new()),
            },
        );
        let result = render_prompt(&Mode::Implement, "tasks/t", &cfg, "").unwrap();
        // Should contain default template content, not empty string
        assert!(result.contains("implementing task"));
    }
}
