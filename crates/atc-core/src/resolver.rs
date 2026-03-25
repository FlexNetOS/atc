use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::config::AtcConfig;
use crate::registry::Registry;
use crate::types::{Directive, DispatchRecord, RunOpts};

/// Result of resolving an input string into dispatch parameters.
#[derive(Debug, Clone)]
pub struct ResolvedInput {
    /// Rendered system prompt (from components, templates, or defaults).
    pub system_prompt: String,
    /// The dispatch mode.
    pub directive: Directive,
    /// Task slug (only for task-based dispatches).
    pub task_slug: Option<String>,
    /// Branch name for worktree creation.
    pub branch: String,
    /// Unique dispatch ID.
    pub dispatch_id: String,
    /// Environment variable overrides from the resolver (e.g. GITKB_ROOT).
    pub env_overrides: HashMap<String, String>,
    /// Discovered KB root path (for task-based dispatches where the KB root
    /// may differ from the workspace root, e.g. multi-KB discovery).
    pub kb_root: Option<PathBuf>,
}

/// Trait defining how an input string is resolved into dispatch parameters.
///
/// Resolvers form a chain: the first resolver whose `can_resolve()` returns true
/// handles the input. The resolver order is configurable via `[resolvers]` config.
#[async_trait]
pub trait InputResolver: Send + Sync {
    /// Short name for this resolver (e.g. "task", "template", "prompt").
    fn name(&self) -> &str;

    /// Returns true if this resolver can handle the given input.
    async fn can_resolve(&self, input: &str, config: &AtcConfig) -> bool;

    /// Resolve the input into dispatch parameters.
    ///
    /// Implementations should handle their own cleanup on internal errors
    /// (e.g. if CAS claim succeeds but a later step fails, unassign).
    async fn resolve(
        &self,
        input: &str,
        opts: &RunOpts,
        config: &AtcConfig,
    ) -> anyhow::Result<ResolvedInput>;

    /// Called on stop/cleanup/close/retry for dispatches created by this resolver.
    ///
    /// `registry` is provided when the dispatch is already registered, allowing
    /// resolvers to check for sibling dispatches before releasing shared resources.
    /// It is `None` when cleaning up before registry insertion (error paths in pipeline).
    /// Default implementation is a no-op.
    async fn on_cleanup(
        &self,
        _record: &DispatchRecord,
        _config: &AtcConfig,
        _registry: Option<&dyn Registry>,
    ) {
        // no-op by default
    }
}
