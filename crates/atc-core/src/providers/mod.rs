pub mod kb_context;
pub mod pr_context;
pub mod rebase;

use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::AtcConfig;
use crate::types::Directive;

/// Input context available to providers during dispatch preparation.
pub struct DispatchContext {
    pub dispatch_id: String,
    pub task_slug: Option<String>,
    pub branch: String,
    pub worktree_path: PathBuf,
    pub directive: Directive,
    pub pr_url: Option<String>,
    /// Key=value pairs from `--param` flags.
    pub params: HashMap<String, String>,
    pub kb_root: PathBuf,
    pub log_dir: PathBuf,
    pub config: Arc<AtcConfig>,
    /// Comment ID for targeted review (extracted from `--param comment=<url>`).
    pub comment_id: Option<String>,
    /// Comment type: "issue", "review_comment", or "review".
    pub comment_type: Option<String>,
    /// Policy-derived KB workspace branch. Providers should prefer this over
    /// `branch` when setting GITKB_WORKSPACE, falling back to `branch` when None.
    pub kb_workspace: Option<String>,
}

/// Output from a provider — merged into dispatch.
#[derive(Debug, Clone, Default)]
pub struct ContextOutput {
    /// Markdown sections prepended to stdin (before task doc).
    pub preamble_sections: Vec<String>,
    /// Files written to worktree (relative path, content).
    pub files: Vec<(PathBuf, String)>,
    /// Additional env vars for agent process.
    pub env: HashMap<String, String>,
    /// Template variable replacements (e.g., `{{prefetch}}` → content).
    pub template_vars: HashMap<String, String>,
}

/// Pluggable unit that fetches data and assembles prompt context before agent dispatch.
#[async_trait]
pub trait ContextProvider: Send + Sync {
    /// Provider name for logging and config reference.
    fn name(&self) -> &str;

    /// Template variable names this provider may inject via `ContextOutput::template_vars`.
    ///
    /// The template resolver uses this to register deferred placeholders so
    /// Handlebars strict mode doesn't reject them before providers run.
    /// Override this when your provider adds entries to `template_vars`.
    fn declared_template_vars(&self) -> &[&str] {
        &[]
    }

    /// Prepare context before agent dispatch.
    /// Called after prompt assembly, before agent spawn.
    async fn prepare(&self, ctx: &DispatchContext) -> anyhow::Result<ContextOutput>;
}

/// Canonical list of known provider names — used for both config validation and instantiation.
pub const KNOWN_PROVIDERS: &[&str] = &["pr-context", "kb-context", "rebase"];

/// Instantiate a provider by name.
pub fn make_provider(name: &str) -> Option<Box<dyn ContextProvider>> {
    match name {
        "pr-context" => Some(Box::new(pr_context::PrContextProvider::new())),
        "kb-context" => Some(Box::new(kb_context::KbContextProvider::new())),
        "rebase" => Some(Box::new(rebase::RebaseProvider::new())),
        _ => None,
    }
}

/// Instantiate all providers for a given directive from config.
pub fn providers_for_directive(
    config: &AtcConfig,
    directive: &Directive,
) -> Vec<Box<dyn ContextProvider>> {
    let directive_key = directive.as_str();
    let provider_names = match config.directives.get(directive_key) {
        Some(directive_cfg) => directive_cfg.providers.clone().unwrap_or_default(),
        None => Vec::new(),
    };

    provider_names
        .iter()
        .filter_map(|name| {
            let provider = make_provider(name);
            if provider.is_none() {
                tracing::warn!(provider = %name, "unknown provider in directive config, skipping");
            }
            provider
        })
        .collect()
}

/// Collect all declared template variable names from every known provider.
///
/// Used by the template resolver to register deferred placeholders before
/// providers run, so Handlebars strict mode doesn't reject provider-injected vars.
/// We query all providers (not just those for the current directive) because the directive
/// may not be known until after template rendering.
pub fn all_deferred_template_vars() -> Vec<String> {
    let mut vars = Vec::new();
    for name in KNOWN_PROVIDERS {
        if let Some(p) = make_provider(name) {
            for v in p.declared_template_vars() {
                let s = (*v).to_string();
                if !vars.contains(&s) {
                    vars.push(s);
                }
            }
        }
    }
    vars
}

/// Run all providers concurrently and merge their outputs.
/// Provider errors are non-fatal: logged as warnings, dispatch continues.
pub async fn run_providers(
    providers: &[Box<dyn ContextProvider>],
    ctx: &DispatchContext,
) -> ContextOutput {
    use futures::future::join_all;

    let futures: Vec<_> = providers.iter().map(|p| p.prepare(ctx)).collect();
    let results = join_all(futures).await;

    let mut merged = ContextOutput::default();
    for (i, result) in results.into_iter().enumerate() {
        let provider_name = providers[i].name();
        match result {
            Ok(output) => {
                merged.preamble_sections.extend(output.preamble_sections);
                merged.files.extend(output.files);
                for (k, v) in output.env {
                    if merged.env.contains_key(&k) {
                        tracing::debug!(
                            provider = %provider_name,
                            key = %k,
                            "env var overwritten by later provider"
                        );
                    }
                    merged.env.insert(k, v);
                }
                for (k, v) in output.template_vars {
                    if merged.template_vars.contains_key(&k) {
                        tracing::debug!(
                            provider = %provider_name,
                            key = %k,
                            "template var overwritten by later provider"
                        );
                    }
                    merged.template_vars.insert(k, v);
                }
            }
            Err(e) => {
                tracing::warn!(
                    provider = %provider_name,
                    error = %e,
                    "provider failed (non-fatal), continuing dispatch"
                );
            }
        }
    }

    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeProvider {
        name: &'static str,
        output: Result<ContextOutput, String>,
    }

    #[async_trait]
    impl ContextProvider for FakeProvider {
        fn name(&self) -> &str {
            self.name
        }

        async fn prepare(&self, _ctx: &DispatchContext) -> anyhow::Result<ContextOutput> {
            match &self.output {
                Ok(o) => Ok(o.clone()),
                Err(msg) => Err(anyhow::anyhow!("{}", msg)),
            }
        }
    }

    fn test_ctx() -> DispatchContext {
        DispatchContext {
            dispatch_id: "test-dispatch".to_string(),
            task_slug: None,
            branch: "main".to_string(),
            worktree_path: PathBuf::from("/tmp/test"),
            directive: Directive::Implement,
            pr_url: None,
            params: HashMap::new(),
            kb_root: PathBuf::from("/tmp/kb"),
            log_dir: PathBuf::from("/tmp/logs"),
            config: Arc::new(AtcConfig::default()),
            comment_id: None,
            comment_type: None,
            kb_workspace: None,
        }
    }

    #[test]
    fn test_make_provider_known() {
        assert!(make_provider("pr-context").is_some());
        assert!(make_provider("kb-context").is_some());
        assert!(make_provider("rebase").is_some());
    }

    #[test]
    fn test_make_provider_unknown() {
        assert!(make_provider("nonexistent").is_none());
    }

    #[tokio::test]
    async fn test_run_providers_merges_outputs() {
        let providers: Vec<Box<dyn ContextProvider>> = vec![
            Box::new(FakeProvider {
                name: "a",
                output: Ok(ContextOutput {
                    preamble_sections: vec!["section-a".to_string()],
                    files: vec![(PathBuf::from("a.md"), "content-a".to_string())],
                    env: HashMap::from([("KEY_A".to_string(), "val_a".to_string())]),
                    template_vars: HashMap::from([("var_a".to_string(), "val_a".to_string())]),
                }),
            }),
            Box::new(FakeProvider {
                name: "b",
                output: Ok(ContextOutput {
                    preamble_sections: vec!["section-b".to_string()],
                    files: vec![(PathBuf::from("b.md"), "content-b".to_string())],
                    env: HashMap::from([("KEY_B".to_string(), "val_b".to_string())]),
                    template_vars: HashMap::from([("var_b".to_string(), "val_b".to_string())]),
                }),
            }),
        ];

        let ctx = test_ctx();
        let merged = run_providers(&providers, &ctx).await;

        assert_eq!(merged.preamble_sections.len(), 2);
        assert_eq!(merged.files.len(), 2);
        assert_eq!(merged.env.len(), 2);
        assert_eq!(merged.template_vars.len(), 2);
    }

    #[tokio::test]
    async fn test_run_providers_error_is_nonfatal() {
        let providers: Vec<Box<dyn ContextProvider>> = vec![
            Box::new(FakeProvider {
                name: "failing",
                output: Err("boom".to_string()),
            }),
            Box::new(FakeProvider {
                name: "ok",
                output: Ok(ContextOutput {
                    preamble_sections: vec!["ok-section".to_string()],
                    ..Default::default()
                }),
            }),
        ];

        let ctx = test_ctx();
        let merged = run_providers(&providers, &ctx).await;

        // The successful provider's output is still present
        assert_eq!(merged.preamble_sections.len(), 1);
        assert_eq!(merged.preamble_sections[0], "ok-section");
    }

    #[test]
    fn test_providers_for_directive_empty_when_no_config() {
        let config = AtcConfig::default();
        let providers = providers_for_directive(&config, &Directive::Implement);
        assert!(providers.is_empty());
    }

    #[test]
    fn test_all_deferred_template_vars_includes_all_provider_vars() {
        let vars = all_deferred_template_vars();
        // pr-context declares "prefetch"
        assert!(
            vars.contains(&"prefetch".to_string()),
            "expected 'prefetch' in deferred vars, got: {:?}",
            vars
        );
        // rebase declares "default_branch" and "rebase_behind_count"
        assert!(
            vars.contains(&"default_branch".to_string()),
            "expected 'default_branch' in deferred vars, got: {:?}",
            vars
        );
        assert!(
            vars.contains(&"rebase_behind_count".to_string()),
            "expected 'rebase_behind_count' in deferred vars, got: {:?}",
            vars
        );
    }

    #[test]
    fn test_pr_context_declares_template_vars() {
        let provider = make_provider("pr-context").unwrap();
        assert_eq!(provider.declared_template_vars(), &["prefetch"]);
    }

    #[test]
    fn test_kb_context_declares_no_template_vars() {
        let provider = make_provider("kb-context").unwrap();
        assert!(provider.declared_template_vars().is_empty());
    }

    #[test]
    fn test_rebase_declares_template_vars() {
        let provider = make_provider("rebase").unwrap();
        let vars = provider.declared_template_vars();
        assert!(vars.contains(&"default_branch"));
        assert!(vars.contains(&"rebase_behind_count"));
    }
}
