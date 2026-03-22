use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use tracing::info;

use atc_core::config::AtcConfig;
use atc_core::resolver::{InputResolver, ResolvedInput};
use atc_core::types::{DispatchRecord, Mode, RunOpts};

/// Process-local counter for unique prompt dispatch IDs.
static PROMPT_SEQ: AtomicU32 = AtomicU32::new(0);

/// Resolver for raw prompt string dispatches (catch-all fallback).
///
/// Wraps the raw input as the system prompt with a default mode of Implement.
pub struct PromptResolver;

#[async_trait]
impl InputResolver for PromptResolver {
    fn name(&self) -> &str {
        "prompt"
    }

    /// Always returns true — this is the catch-all fallback resolver.
    async fn can_resolve(&self, _input: &str, _config: &AtcConfig) -> bool {
        true
    }

    async fn resolve(
        &self,
        input: &str,
        opts: &RunOpts,
        _config: &AtcConfig,
    ) -> Result<ResolvedInput> {
        let mode = opts.mode.clone().unwrap_or(Mode::Implement);

        // Build a unique branch name from timestamp
        let ts = Utc::now().timestamp_millis();
        let seq = PROMPT_SEQ.fetch_add(1, Ordering::Relaxed);
        let branch = format!("prompt-{ts}-{seq}");

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let suffix = nanos ^ std::process::id() ^ seq;
        let dispatch_id = format!(
            "{}@{}@{}-{:04x}",
            branch,
            mode.as_str(),
            ts,
            suffix & 0xffff
        );

        info!(mode = %mode.as_str(), "prompt resolver: dispatching raw prompt");

        Ok(ResolvedInput {
            system_prompt: input.to_string(),
            mode,
            task_slug: None,
            branch,
            dispatch_id,
            env_overrides: HashMap::new(),
        })
    }

    async fn on_cleanup(
        &self,
        _record: &DispatchRecord,
        _config: &AtcConfig,
        _registry: Option<&dyn atc_core::registry::Registry>,
    ) {
        // Raw prompts have no external state to clean up
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prompt_resolver_name() {
        let resolver = PromptResolver;
        assert_eq!(resolver.name(), "prompt");
    }

    #[tokio::test]
    async fn test_can_resolve_always_true() {
        let resolver = PromptResolver;
        let config = AtcConfig::default();
        assert!(resolver.can_resolve("anything", &config).await);
        assert!(resolver.can_resolve("", &config).await);
    }

    #[tokio::test]
    async fn test_resolve_prompt() {
        let resolver = PromptResolver;
        let config = AtcConfig::default();
        let opts = RunOpts {
            input: "Fix the bug in auth.rs".to_string(),
            mode: Some(Mode::Implement),
            params: HashMap::new(),
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

        let result = resolver
            .resolve("Fix the bug in auth.rs", &opts, &config)
            .await
            .unwrap();

        assert_eq!(result.system_prompt, "Fix the bug in auth.rs");
        assert_eq!(result.mode, Mode::Implement);
        assert!(result.task_slug.is_none());
        assert!(result.branch.starts_with("prompt-"));
        assert!(result.dispatch_id.contains("@implement@"));
    }

    #[tokio::test]
    async fn test_resolve_prompt_default_mode() {
        let resolver = PromptResolver;
        let config = AtcConfig::default();
        let opts = RunOpts {
            input: "test".to_string(),
            mode: None, // Should default to Implement
            params: HashMap::new(),
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

        let result = resolver.resolve("test", &opts, &config).await.unwrap();
        assert_eq!(result.mode, Mode::Implement);
    }
}
