use async_trait::async_trait;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, warn};

use super::{ContextOutput, ContextProvider, DispatchContext};

/// Timeout for individual git-kb subprocess calls.
const GIT_KB_TIMEOUT: Duration = Duration::from_secs(30);

/// Provider that assembles supplementary KB context for task-based dispatches.
///
/// This provider is part of the GitKB integration layer. It only activates
/// when the dispatch has a `task_slug` (i.e., came through `TaskResolver`).
#[derive(Default)]
pub struct KbContextProvider;

impl KbContextProvider {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ContextProvider for KbContextProvider {
    fn name(&self) -> &str {
        "kb-context"
    }

    async fn prepare(&self, ctx: &DispatchContext) -> anyhow::Result<ContextOutput> {
        let task_slug = match &ctx.task_slug {
            Some(slug) => slug.clone(),
            None => {
                // No task slug — no-op
                return Ok(ContextOutput::default());
            }
        };

        info!(task_slug = %task_slug, "kb-context: fetching related docs");

        let mut output = ContextOutput::default();
        let kb_root = &ctx.kb_root;

        // 1. Fetch related docs via `git kb graph`
        let related_context = fetch_related_context(&task_slug, kb_root, &ctx.branch).await;
        if let Some(context_section) = related_context {
            output.preamble_sections.push(context_section);
        }

        // 2. Fetch active context summary
        let active_context = fetch_active_context(kb_root, &ctx.branch).await;
        if let Some(active_section) = active_context {
            output.preamble_sections.push(active_section);
        }

        Ok(output)
    }
}

/// Fetch related context docs for a task using `git kb graph`.
async fn fetch_related_context(task_slug: &str, kb_root: &PathBuf, branch: &str) -> Option<String> {
    let output = match tokio::time::timeout(
        GIT_KB_TIMEOUT,
        tokio::process::Command::new("git-kb")
            .args(["graph", task_slug])
            .env("GITKB_ROOT", kb_root)
            .env("GITKB_WORKSPACE", branch)
            .kill_on_drop(true)
            .output(),
    )
    .await
    {
        Ok(result) => result.ok()?,
        Err(_) => {
            warn!(task_slug = %task_slug, "git-kb graph timed out");
            return None;
        }
    };

    if !output.status.success() {
        warn!(
            task_slug = %task_slug,
            "git kb graph failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }

    let graph_output = String::from_utf8_lossy(&output.stdout);

    // Parse graph output for related context docs
    let mut context_slugs = Vec::new();
    for line in graph_output.lines() {
        let trimmed = line.trim();
        // Graph output typically shows relationships like:
        //   -> context/some-doc (depends_on)
        //   <- tasks/other-task (parent)
        // We're interested in related context/ docs
        if trimmed.contains("context/") {
            // Extract slug from the line
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            for part in &parts {
                if part.starts_with("context/") {
                    context_slugs.push(part.to_string());
                }
            }
        }
    }

    if context_slugs.is_empty() {
        return None;
    }

    // Fetch each context doc
    let mut sections = Vec::new();
    for slug in &context_slugs {
        let doc = match tokio::time::timeout(
            GIT_KB_TIMEOUT,
            tokio::process::Command::new("git-kb")
                .args(["show", slug])
                .env("GITKB_ROOT", kb_root)
                .env("GITKB_WORKSPACE", branch)
                .kill_on_drop(true)
                .output(),
        )
        .await
        {
            Ok(result) => result.ok(),
            Err(_) => {
                warn!(slug = %slug, "git-kb show timed out");
                None
            }
        };

        if let Some(doc_output) = doc {
            if doc_output.status.success() {
                let content = String::from_utf8_lossy(&doc_output.stdout);
                if !content.trim().is_empty() {
                    sections.push(format!("### Related: {}\n\n{}", slug, content.trim()));
                }
            }
        }
    }

    if sections.is_empty() {
        return None;
    }

    Some(format!(
        "## Related Context\n\n{}",
        sections.join("\n\n---\n\n")
    ))
}

/// Fetch active context summary from `context/overridable/active`.
async fn fetch_active_context(kb_root: &PathBuf, branch: &str) -> Option<String> {
    let output = match tokio::time::timeout(
        GIT_KB_TIMEOUT,
        tokio::process::Command::new("git-kb")
            .args(["show", "context/overridable/active"])
            .env("GITKB_ROOT", kb_root)
            .env("GITKB_WORKSPACE", branch)
            .kill_on_drop(true)
            .output(),
    )
    .await
    {
        Ok(result) => result.ok()?,
        Err(_) => {
            warn!("git-kb show active context timed out");
            return None;
        }
    };

    if !output.status.success() {
        // Active context is optional — not an error
        return None;
    }

    let content = String::from_utf8_lossy(&output.stdout);
    if content.trim().is_empty() {
        return None;
    }

    Some(format!("## Active Context\n\n{}", content.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AtcConfig;
    use crate::providers::DispatchContext;
    use crate::types::Directive;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn test_ctx_with_task(task_slug: Option<&str>) -> DispatchContext {
        DispatchContext {
            dispatch_id: "test-dispatch".to_string(),
            task_slug: task_slug.map(|s| s.to_string()),
            branch: "main".to_string(),
            worktree_path: PathBuf::from("/tmp/test"),
            directive: Directive::Implement,
            pr_url: None,
            params: HashMap::new(),
            kb_root: PathBuf::from("/tmp/kb"),
            log_dir: PathBuf::from("/tmp/logs"),
            config: Arc::new(AtcConfig::default()),
        }
    }

    #[tokio::test]
    async fn test_kb_context_noop_without_task_slug() {
        let provider = KbContextProvider::new();
        let ctx = test_ctx_with_task(None);
        let output = provider.prepare(&ctx).await.unwrap();
        assert!(output.preamble_sections.is_empty());
        assert!(output.files.is_empty());
    }

    #[test]
    fn test_provider_name() {
        let provider = KbContextProvider::new();
        assert_eq!(provider.name(), "kb-context");
    }
}
