use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use tracing::{debug, info};

use atc_core::config::AtcConfig;
use atc_core::prompt_engine;
use atc_core::resolver::{InputResolver, ResolvedInput};
use atc_core::types::{Directive, DispatchRecord, RunOpts};

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
/// name is the file stem (e.g. "review" matches "review.md"). Templates use
/// YAML frontmatter with `directive:` (singular) to specify the execution
/// directive, and `required_params:` to validate params before rendering.
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

        // Ensure optional params used in {{#if}}/{{#unless}} conditionals
        // have a default empty value so Handlebars strict mode doesn't reject them.
        params
            .entry("comment".to_string())
            .or_insert_with(String::new);

        // Render template — pass just the filename so render_template resolves
        // it against templates_dir internally, avoiding double-resolution when
        // templates_dir is relative and config_dir is None.
        //
        // Provider-injected template vars (e.g. {{prefetch}} from pr-context)
        // aren't available yet — they're substituted by the pipeline after
        // providers run. We query providers for their declared var names so
        // Handlebars strict mode doesn't reject them.
        let mut deferred_owned = atc_core::providers::all_deferred_template_vars();
        // {{worktree}} is populated by the pipeline after worktree creation, not by a provider.
        if !deferred_owned.contains(&"worktree".to_string()) {
            deferred_owned.push("worktree".to_string());
        }
        let deferred_vars: Vec<&str> = deferred_owned.iter().map(|s| s.as_str()).collect();

        // Pre-render: read frontmatter to validate required_params before rendering.
        // We do a lightweight parse of the raw file first.
        let raw_content = tokio::fs::read_to_string(&template_path)
            .await
            .with_context(|| {
                format!("failed to read template file '{}'", template_path.display())
            })?;
        // Quick frontmatter parse just to check required_params
        let quick_fm = prompt_engine::parse_template_frontmatter(&raw_content)?;
        if let Some(ref required) = quick_fm.required_params {
            for param in required {
                if params.get(param.as_str()).is_none_or(|v| v.is_empty()) {
                    anyhow::bail!("template '{}' requires --param {}=<value>", input, param);
                }
            }
        }

        let output = prompt_engine::render_template_with_deferred(
            Path::new(&template_name),
            &params,
            &deferred_vars,
            config,
            None,
        )
        .await?;

        // Resolve directive: CLI override > frontmatter `directive:` (singular) >
        // legacy `directives:` list > default implement
        let resolved_directive = if let Some(ref m) = opts.directive {
            m.clone()
        } else if let Some(ref d) = output.directive {
            d.parse::<Directive>()
                .with_context(|| format!("unknown directive '{}' in template '{}'", d, input))?
        } else if let Some(first_directive) = output.directives.first() {
            first_directive.parse::<Directive>().unwrap_or_else(|_| {
                debug!(directive = %first_directive, "unrecognized directive, defaulting to implement");
                Directive::Implement
            })
        } else {
            Directive::Implement
        };

        info!(template = input, directive = %resolved_directive.as_str(), "template resolved");

        // Branch resolution priority:
        // 1. PR URL (--pr-url or --param pr=<url>) → use PR's actual head branch
        // 2. Explicit `branch` param
        // 3. Current git branch (if not main/master)
        // 4. Synthetic fallback: tpl--<name>-<ts>-<pid>-<seq>
        let pr_for_branch = opts
            .pr_url
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                params
                    .get("pr")
                    .map(String::as_str)
                    .filter(|s| !s.is_empty())
            });

        let branch = if let Some(pr_url) = pr_for_branch {
            // Extract head branch from PR (calls `gh pr view`)
            match crate::dispatch::extract_pr_head_branch(pr_url).await {
                Ok(b) => {
                    info!(pr_url, branch = %b, "using PR head branch");
                    b
                }
                Err(e) => {
                    debug!(pr_url, error = %e, "failed to extract PR head branch, falling back to synthetic");
                    let ts = chrono::Utc::now().timestamp_millis();
                    let seq = TPL_SEQ.fetch_add(1, Ordering::Relaxed);
                    let pid = std::process::id();
                    format!("tpl--{}-{}-{}-{}", input, ts, pid, seq)
                }
            }
        } else if let Some(b) = params.get("branch").filter(|s| !s.is_empty()) {
            b.clone()
        } else {
            // Check current git branch
            let current_branch = tokio::process::Command::new("git")
                .args(["branch", "--show-current"])
                .output()
                .await
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        let b = String::from_utf8_lossy(&o.stdout).trim().to_string();
                        if !b.is_empty() && b != "main" && b != "master" {
                            Some(b)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                });

            match current_branch {
                Some(b) => {
                    info!(branch = %b, "using current git branch for template dispatch");
                    b
                }
                None => {
                    // Synthetic fallback
                    let ts = chrono::Utc::now().timestamp_millis();
                    let seq = TPL_SEQ.fetch_add(1, Ordering::Relaxed);
                    let pid = std::process::id();
                    format!("tpl--{}-{}-{}-{}", input, ts, pid, seq)
                }
            }
        };
        let dispatch_id = build_dispatch_id(&branch, &resolved_directive);

        Ok(ResolvedInput {
            system_prompt: String::new(), // pipeline assembles from directive components
            directive: resolved_directive,
            task_slug: None,
            branch,
            dispatch_id,
            env_overrides: std::collections::HashMap::new(),
            kb_root: None,
            is_template: true,
            template_body: Some(output.body),
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
            directive: None,
            params: std::collections::HashMap::new(),
            pr_url: None,
            repos: vec![],
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
            "---\ndirective: review-fix\n---\nReview PR: {{pr_url}}",
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
            directive: None,
            params,
            pr_url: None,
            repos: vec![],
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

        assert_eq!(result.directive, Directive::ReviewFix);
        assert!(result.is_template);
        let body = result.template_body.as_deref().unwrap();
        assert!(
            body.contains("https://github.com/org/repo/pull/1"),
            "expected PR URL in template_body, got: {}",
            body
        );
        assert!(
            result.system_prompt.is_empty(),
            "system_prompt should be empty for template dispatch"
        );
        assert!(result.task_slug.is_none());
        // Branch is either the current git branch (if not main/master) or
        // a synthetic tpl--review-* branch.
        assert!(
            result.branch.starts_with("tpl--review-") || !result.branch.is_empty(),
            "branch should be non-empty, got: {}",
            result.branch
        );
    }

    /// Template with {{prefetch}} (a provider-injected var) should render
    /// successfully with a deferred placeholder instead of failing strict mode.
    #[tokio::test]
    async fn test_resolve_template_with_deferred_provider_var() {
        let dir = tempfile::tempdir().unwrap();
        let tmpl_dir = dir.path().join("templates");
        std::fs::create_dir_all(&tmpl_dir).unwrap();

        let partials_dir = dir.path().join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();
        let comp_dir = dir.path().join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();

        // Template that references both a user param and a provider-injected var
        std::fs::write(
            tmpl_dir.join("pr-review.md"),
            "---\ndirective: review-fix\nrequired_params: [pr]\n---\nPR: {{pr}}\n\n{{prefetch}}",
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
            "pr".to_string(),
            "https://github.com/org/repo/pull/42".to_string(),
        );

        let opts = RunOpts {
            input: "pr-review".to_string(),
            directive: None,
            params,
            pr_url: None,
            repos: vec![],
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
        let result = resolver.resolve("pr-review", &opts, &config).await.unwrap();

        assert!(result.is_template);
        let body = result.template_body.as_deref().unwrap();
        // User-supplied param should be resolved
        assert!(
            body.contains("https://github.com/org/repo/pull/42"),
            "expected PR URL in template_body, got: {}",
            body
        );
        // Provider var should be a deferred placeholder, not rejected
        assert!(
            body.contains("__ATC_DEFER_prefetch__"),
            "expected deferred placeholder for prefetch, got: {}",
            body
        );
        assert_eq!(result.directive, Directive::ReviewFix);
    }

    /// Helper to create a test config with templates, partials, and components dirs.
    fn test_config(dir: &std::path::Path) -> AtcConfig {
        let tmpl_dir = dir.join("templates");
        std::fs::create_dir_all(&tmpl_dir).unwrap();
        let partials_dir = dir.join("partials");
        std::fs::create_dir_all(&partials_dir).unwrap();
        let comp_dir = dir.join("components");
        std::fs::create_dir_all(&comp_dir).unwrap();

        AtcConfig {
            config_dir: Some(dir.to_path_buf()),
            prompt: PromptConfig {
                templates_dir: "templates".to_string(),
                partials_dir: "partials".to_string(),
                components_dir: "components".to_string(),
            },
            ..Default::default()
        }
    }

    fn test_opts(input: &str, params: std::collections::HashMap<String, String>) -> RunOpts {
        RunOpts {
            input: input.to_string(),
            directive: None,
            params,
            pr_url: None,
            repos: vec![],
            inline: true,
            force: false,
            dry_run: false,
            directives: None,
            no_worktree: false,
            max_budget_usd: None,
            max_turns: None,
            retries: 0,
            list: false,
        }
    }

    /// Test `directive:` (singular) frontmatter field resolves the directive.
    #[tokio::test]
    async fn test_resolve_template_directive_singular() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());

        std::fs::write(
            dir.path().join("templates/close.md"),
            "---\ndirective: close\n---\nClose this task.",
        )
        .unwrap();

        let opts = test_opts("close", std::collections::HashMap::new());
        let resolver = TemplateResolver;
        let result = resolver.resolve("close", &opts, &config).await.unwrap();

        assert_eq!(result.directive, Directive::Close);
        assert!(result.is_template);
        assert_eq!(result.template_body.as_deref(), Some("Close this task."));
    }

    /// Test required_params validation rejects missing params.
    #[tokio::test]
    async fn test_resolve_template_required_params_missing() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());

        std::fs::write(
            dir.path().join("templates/pr-review.md"),
            "---\ndirective: review-fix\nrequired_params: [pr]\n---\nReview PR: {{pr}}",
        )
        .unwrap();

        let opts = test_opts("pr-review", std::collections::HashMap::new());
        let resolver = TemplateResolver;
        let result = resolver.resolve("pr-review", &opts, &config).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("requires --param pr="),
            "expected required_params error, got: {}",
            err
        );
    }

    /// Test required_params validation passes when params provided.
    #[tokio::test]
    async fn test_resolve_template_required_params_provided() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());

        std::fs::write(
            dir.path().join("templates/pr-review.md"),
            "---\ndirective: review-fix\nrequired_params: [pr]\n---\nReview PR: {{pr}}\n\n{{prefetch}}",
        )
        .unwrap();

        let mut params = std::collections::HashMap::new();
        params.insert(
            "pr".to_string(),
            "https://github.com/org/repo/pull/1".to_string(),
        );

        let opts = test_opts("pr-review", params);
        let resolver = TemplateResolver;
        let result = resolver.resolve("pr-review", &opts, &config).await.unwrap();

        assert_eq!(result.directive, Directive::ReviewFix);
        assert!(result.is_template);
        let body = result.template_body.as_deref().unwrap();
        assert!(body.contains("https://github.com/org/repo/pull/1"));
    }

    /// Test each of the 6 templates resolves to the correct directive.
    #[tokio::test]
    async fn test_resolve_all_six_templates() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let tmpl_dir = dir.path().join("templates");

        // Create all 6 templates
        std::fs::write(
            tmpl_dir.join("pr-review.md"),
            "---\ndirective: review-fix\nrequired_params: [pr]\n---\nReview {{pr}}\n\n{{prefetch}}",
        )
        .unwrap();
        std::fs::write(
            tmpl_dir.join("pr-comment.md"),
            "---\ndirective: pr-comments\nrequired_params: [pr]\n---\nComment on {{pr}}\n\n{{prefetch}}",
        )
        .unwrap();
        std::fs::write(
            tmpl_dir.join("branch-review.md"),
            "---\ndirective: review-fix\n---\nReview branch.",
        )
        .unwrap();
        std::fs::write(
            tmpl_dir.join("close.md"),
            "---\ndirective: close\n---\nClose task.",
        )
        .unwrap();
        std::fs::write(
            tmpl_dir.join("push-branch.md"),
            "---\ndirective: implement\n---\nPush branch.",
        )
        .unwrap();
        std::fs::write(
            tmpl_dir.join("swot.md"),
            "---\ndirective: research\n---\nSWOT analysis.",
        )
        .unwrap();

        let resolver = TemplateResolver;

        // pr-review → review-fix
        let mut params = std::collections::HashMap::new();
        params.insert("pr".to_string(), "https://example.com/pull/1".to_string());
        let opts = test_opts("pr-review", params);
        let r = resolver.resolve("pr-review", &opts, &config).await.unwrap();
        assert_eq!(r.directive, Directive::ReviewFix);
        assert!(r.is_template);

        // pr-comment → pr-comments
        let mut params = std::collections::HashMap::new();
        params.insert("pr".to_string(), "https://example.com/pull/2".to_string());
        let opts = test_opts("pr-comment", params);
        let r = resolver
            .resolve("pr-comment", &opts, &config)
            .await
            .unwrap();
        assert_eq!(r.directive, Directive::PrComments);

        // branch-review → review-fix
        let opts = test_opts("branch-review", std::collections::HashMap::new());
        let r = resolver
            .resolve("branch-review", &opts, &config)
            .await
            .unwrap();
        assert_eq!(r.directive, Directive::ReviewFix);

        // close → close
        let opts = test_opts("close", std::collections::HashMap::new());
        let r = resolver.resolve("close", &opts, &config).await.unwrap();
        assert_eq!(r.directive, Directive::Close);

        // push-branch → implement
        let opts = test_opts("push-branch", std::collections::HashMap::new());
        let r = resolver
            .resolve("push-branch", &opts, &config)
            .await
            .unwrap();
        assert_eq!(r.directive, Directive::Implement);

        // swot → research
        let opts = test_opts("swot", std::collections::HashMap::new());
        let r = resolver.resolve("swot", &opts, &config).await.unwrap();
        assert_eq!(r.directive, Directive::Research);
    }

    /// Test that CLI --directive override takes precedence over template frontmatter.
    #[tokio::test]
    async fn test_resolve_template_cli_directive_override() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());

        std::fs::write(
            dir.path().join("templates/swot.md"),
            "---\ndirective: research\n---\nSWOT analysis.",
        )
        .unwrap();

        let opts = RunOpts {
            input: "swot".to_string(),
            directive: Some(Directive::Implement),
            params: std::collections::HashMap::new(),
            pr_url: None,
            repos: vec![],
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
        let result = resolver.resolve("swot", &opts, &config).await.unwrap();
        assert_eq!(result.directive, Directive::Implement);
    }

    /// Test unknown directive in template frontmatter produces a clear error.
    #[tokio::test]
    async fn test_resolve_template_unknown_directive_errors() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());

        std::fs::write(
            dir.path().join("templates/bad.md"),
            "---\ndirective: nonexistent-directive\n---\nBody.",
        )
        .unwrap();

        let opts = test_opts("bad", std::collections::HashMap::new());
        let resolver = TemplateResolver;
        let result = resolver.resolve("bad", &opts, &config).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unknown directive"),
            "expected unknown directive error, got: {}",
            err
        );
    }
}
