use anyhow::Result;
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::Path;
use tracing::{debug, info};

use atc_core::config::AtcConfig;
use atc_core::prompt_engine;
use atc_core::resolver::{InputResolver, ResolvedInput};
use atc_core::types::{DispatchRecord, Mode, RunOpts};

use crate::dispatch::{build_dispatch_id, derive_branch};

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
        let dir = Self::templates_dir(config);
        let template_path = dir.join(format!("{input}.md"));

        debug!(template = %template_path.display(), "rendering template");

        // Build params from CLI --param key=value pairs
        let mut params = BTreeMap::new();
        for (k, v) in &opts.params {
            params.insert(k.clone(), v.clone());
        }

        // Render template
        let output = prompt_engine::render_template(&template_path, &params, config, None).await?;

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

        // Use template name as branch basis
        let branch = derive_branch(input);
        let dispatch_id = build_dispatch_id(&branch, &mode);

        Ok(ResolvedInput {
            system_prompt: output.body,
            mode,
            task_slug: None,
            branch,
            dispatch_id,
            env_overrides: std::collections::HashMap::new(),
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
        assert_eq!(result.branch, "review");
    }
}
