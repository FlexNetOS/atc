use anyhow::Result;
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use tracing::{debug, info};

use atc_core::config::AtcConfig;
use atc_core::prompt_engine;
use atc_core::resolver::{InputResolver, ResolvedInput};
use atc_core::types::{DispatchRecord, Mode, RunOpts};

use crate::dispatch::build_dispatch_id;

/// Per-process monotonic counter for template branch uniqueness.
static TPL_SEQ: AtomicU32 = AtomicU32::new(0);

/// Check whether `s` contains only characters safe for use in a Git refname.
/// Rejects spaces and characters forbidden by `git check-ref-format`:
/// control chars, space, `~`, `^`, `:`, `?`, `*`, `[`, `\`.
fn is_ref_safe(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && !s.ends_with('.')
        && !s.contains("..")
        && s.chars().all(|c| {
            !c.is_control()
                && c != ' '
                && c != '~'
                && c != '^'
                && c != ':'
                && c != '?'
                && c != '*'
                && c != '['
                && c != '\\'
        })
}

/// Resolver for template-based dispatches.
///
/// Templates are `.md` files in the configured templates directory. The template
/// name is the file stem (e.g. "review" matches "review.md"). Templates can
/// include YAML frontmatter with `directives:` to specify components/mode.
pub struct TemplateResolver;

impl TemplateResolver {
    /// Resolve the templates directory path.
    fn templates_dir(config: &AtcConfig) -> std::path::PathBuf {
        let dir = &config.prompt.templates_dir;
        let p = atc_core::config::expand_tilde(Path::new(dir));
        if p.is_absolute() {
            p
        } else if let Some(ref base) = config.config_dir {
            base.join(p)
        } else {
            p
        }
    }

    /// List available template names (file stems of .md files in templates_dir).
    pub fn list_templates(config: &AtcConfig) -> Vec<String> {
        let dir = Self::templates_dir(config);
        let mut names = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        names.push(stem.to_string());
                    }
                }
            }
        }
        names.sort();
        names
    }
}

#[async_trait]
impl InputResolver for TemplateResolver {
    fn name(&self) -> &str {
        "template"
    }

    async fn can_resolve(&self, input: &str, config: &AtcConfig) -> bool {
        // Reject path traversal attempts at the input level
        if input.contains("..") || input.contains('/') || input.contains('\\') {
            return false;
        }
        // Reject characters that are invalid in Git refnames (spaces, *, :, ?, [, ^, ~, etc.)
        // so we fail fast instead of waiting for validate_branch_name() later.
        if !is_ref_safe(input) {
            return false;
        }
        let dir = Self::templates_dir(config);
        let template_path = dir.join(format!("{input}.md"));
        template_path.exists()
    }

    async fn resolve(
        &self,
        input: &str,
        opts: &RunOpts,
        config: &AtcConfig,
    ) -> Result<ResolvedInput> {
        // Reject path traversal attempts at the input level
        anyhow::ensure!(
            !input.contains("..") && !input.contains('/') && !input.contains('\\'),
            "template name must not contain path separators or '..': {:?}",
            input
        );
        // Reject characters that are invalid in Git refnames so the user gets
        // immediate feedback instead of a cryptic validate_branch_name error.
        anyhow::ensure!(
            is_ref_safe(input),
            "template name contains characters invalid in Git refnames: {:?}",
            input
        );
        let template_name = format!("{input}.md");
        let template_path = Self::templates_dir(config).join(&template_name);

        debug!(template = %template_path.display(), "rendering template");

        // Build params from CLI --param key=value pairs
        let mut params = BTreeMap::new();
        for (k, v) in &opts.params {
            params.insert(k.clone(), v.clone());
        }

        // Render template — pass just the filename so render_template resolves
        // it against templates_dir internally, avoiding double-resolution when
        // templates_dir is relative and config_dir is None.
        let output =
            prompt_engine::render_template(Path::new(&template_name), &params, config, None)
                .await?;

        // Resolve mode: CLI override > frontmatter directives > default implement
        let mode = if let Some(ref m) = opts.mode {
            m.clone()
        } else if let Some(first_directive) = output.directives.first() {
            first_directive.parse::<Mode>().unwrap_or_else(|_| {
                debug!(directive = %first_directive, "unrecognized mode directive, defaulting to implement");
                Mode::Implement
            })
        } else {
            Mode::Implement
        };

        info!(template = input, mode = %mode.as_str(), "template resolved");

        // Use template name with a resolver-specific prefix to avoid collisions
        // with task-derived branches (derive_branch is bijective only for valid
        // GitKB slugs that contain '/').
        // Include a timestamp so concurrent dispatches of the same template
        // get distinct branches (similar to PromptResolver).
        let ts = chrono::Utc::now().timestamp_millis();
        let seq = TPL_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let branch = format!("tpl--{}-{}-{}-{}", input, ts, pid, seq);
        let dispatch_id = build_dispatch_id(&branch, &mode);

        Ok(ResolvedInput {
            system_prompt: output.body,
            mode,
            task_slug: None,
            branch,
            dispatch_id,
            env_overrides: std::collections::HashMap::new(),
            kb_root: None,
        })
    }

    async fn on_cleanup(
        &self,
        _record: &DispatchRecord,
        _config: &AtcConfig,
        _registry: Option<&dyn atc_core::registry::Registry>,
    ) {
        // Templates have no external state to clean up
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atc_core::config::PromptConfig;

    #[test]
    fn test_template_resolver_name() {
        let resolver = TemplateResolver;
        assert_eq!(resolver.name(), "template");
    }

    #[test]
    fn test_list_templates_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config = AtcConfig {
            config_dir: Some(dir.path().to_path_buf()),
            prompt: PromptConfig {
                templates_dir: "templates".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };

        // Create the templates dir (empty)
        std::fs::create_dir_all(dir.path().join("templates")).unwrap();

        let names = TemplateResolver::list_templates(&config);
        assert!(names.is_empty());
    }

    #[test]
    fn test_list_templates_finds_md_files() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl_dir = dir.path().join("templates");
        std::fs::create_dir_all(&tmpl_dir).unwrap();
        std::fs::write(tmpl_dir.join("review.md"), "---\n---\nReview template.").unwrap();
        std::fs::write(tmpl_dir.join("deploy.md"), "---\n---\nDeploy template.").unwrap();
        std::fs::write(tmpl_dir.join("notes.txt"), "not a template").unwrap();

        let config = AtcConfig {
            config_dir: Some(dir.path().to_path_buf()),
            prompt: PromptConfig {
                templates_dir: "templates".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };

        let names = TemplateResolver::list_templates(&config);
        assert_eq!(names, vec!["deploy", "review"]);
    }

    #[tokio::test]
    async fn test_can_resolve_rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl_dir = dir.path().join("templates");
        std::fs::create_dir_all(&tmpl_dir).unwrap();
        // Create a file that traversal would reach
        std::fs::write(dir.path().join("secret.md"), "secret").unwrap();

        let config = AtcConfig {
            config_dir: Some(dir.path().to_path_buf()),
            prompt: PromptConfig {
                templates_dir: "templates".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };

        let resolver = TemplateResolver;
        assert!(!resolver.can_resolve("../secret", &config).await);
        assert!(!resolver.can_resolve("../../etc/passwd", &config).await);
        assert!(!resolver.can_resolve("sub/dir", &config).await);
        assert!(!resolver.can_resolve("sub\\dir", &config).await);
        assert!(!resolver.can_resolve("..", &config).await);
    }

    #[tokio::test]
    async fn test_can_resolve_rejects_invalid_refname_chars() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl_dir = dir.path().join("templates");
        std::fs::create_dir_all(&tmpl_dir).unwrap();
        // Create files whose stems contain git-refname-invalid characters
        std::fs::write(tmpl_dir.join("my template.md"), "body").unwrap();
        std::fs::write(tmpl_dir.join("star*.md"), "body").unwrap();
        std::fs::write(tmpl_dir.join("colon:.md"), "body").unwrap();

        let config = AtcConfig {
            config_dir: Some(dir.path().to_path_buf()),
            prompt: PromptConfig {
                templates_dir: "templates".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };

        let resolver = TemplateResolver;
        assert!(!resolver.can_resolve("my template", &config).await);
        assert!(!resolver.can_resolve("star*", &config).await);
        assert!(!resolver.can_resolve("colon:", &config).await);
        assert!(!resolver.can_resolve("q?mark", &config).await);
        assert!(!resolver.can_resolve("tilde~1", &config).await);
    }

    #[test]
    fn test_is_ref_safe() {
        assert!(is_ref_safe("review"));
        assert!(is_ref_safe("my-template"));
        assert!(is_ref_safe("v1.0"));
        assert!(!is_ref_safe("has space"));
        assert!(!is_ref_safe("star*"));
        assert!(!is_ref_safe("colon:x"));
        assert!(!is_ref_safe("q?mark"));
        assert!(!is_ref_safe("caret^"));
        assert!(!is_ref_safe("tilde~"));
        assert!(!is_ref_safe("bracket[0]"));
        assert!(!is_ref_safe(""));
        assert!(!is_ref_safe(".hidden"));
        assert!(!is_ref_safe("trail."));
        assert!(!is_ref_safe("dbl..dot"));
    }

    #[tokio::test]
    async fn test_resolve_rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl_dir = dir.path().join("templates");
        std::fs::create_dir_all(&tmpl_dir).unwrap();

        let config = AtcConfig {
            config_dir: Some(dir.path().to_path_buf()),
            prompt: PromptConfig {
                templates_dir: "templates".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };

        let opts = RunOpts {
            input: "../secret".to_string(),
            mode: None,
            params: std::collections::HashMap::new(),
            pr_url: None,
            inline: true,
            force: false,
            dry_run: false,
            directives: None,
            no_worktree: false,
            max_budget_usd: None,
            max_turns: None,
            retries: 0,
            list: false,
        };

        let resolver = TemplateResolver;
        let result = resolver.resolve("../secret", &opts, &config).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("path separators"));
    }

    #[tokio::test]
    async fn test_can_resolve_existing_template() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl_dir = dir.path().join("templates");
        std::fs::create_dir_all(&tmpl_dir).unwrap();
        std::fs::write(tmpl_dir.join("review.md"), "Template body.").unwrap();

        let config = AtcConfig {
            config_dir: Some(dir.path().to_path_buf()),
            prompt: PromptConfig {
                templates_dir: "templates".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };

        let resolver = TemplateResolver;
        assert!(resolver.can_resolve("review", &config).await);
        assert!(!resolver.can_resolve("nonexistent", &config).await);
    }

    #[tokio::test]
    async fn test_resolve_template() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl_dir = dir.path().join("templates");
        std::fs::create_dir_all(&tmpl_dir).unwrap();

        let partials_dir = dir.path().join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();
        let comp_dir = dir.path().join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();

        std::fs::write(
            tmpl_dir.join("review.md"),
            "---\ndirectives: [review-fix]\n---\nReview PR: {{pr_url}}",
        )
        .unwrap();

        let config = AtcConfig {
            config_dir: Some(dir.path().to_path_buf()),
            prompt: PromptConfig {
                templates_dir: "templates".to_string(),
                partials_dir: "partials".to_string(),
                components_dir: "components".to_string(),
            },
            ..Default::default()
        };

        let mut params = std::collections::HashMap::new();
        params.insert(
            "pr_url".to_string(),
            "https://github.com/org/repo/pull/1".to_string(),
        );

        let opts = RunOpts {
            input: "review".to_string(),
            mode: None,
            params,
            pr_url: None,
            inline: true,
            force: false,
            dry_run: false,
            directives: None,
            no_worktree: false,
            max_budget_usd: None,
            max_turns: None,
            retries: 0,
            list: false,
        };

        let resolver = TemplateResolver;
        let result = resolver.resolve("review", &opts, &config).await.unwrap();

        assert_eq!(result.mode, Mode::ReviewFix);
        assert!(result
            .system_prompt
            .contains("https://github.com/org/repo/pull/1"));
        assert!(result.task_slug.is_none());
        assert!(
            result.branch.starts_with("tpl--review-"),
            "branch should start with 'tpl--review-', got: {}",
            result.branch
        );
    }
}
