use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use tracing::{debug, info, warn};

use atc_core::config::AtcConfig;
use atc_core::registry::Registry;
use atc_core::resolver::{InputResolver, ResolvedInput};
use atc_core::types::{Directive, DispatchRecord, RunOpts};

use crate::dispatch::{build_dispatch_id, derive_branch};
use crate::subprocess::run_cmd_with_timeout;

/// Timeout for git-kb subprocess calls.
const KB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Max concurrent `git-kb show` calls during multi-KB discovery.
/// Keeps file descriptor usage bounded in large monorepos.
const KB_DISCOVERY_CONCURRENCY: usize = 16;

/// Resolver for GitKB task dispatches. Consolidates ALL `git kb` interactions.
pub struct TaskResolver {
    /// Cache from the last successful `discover_kb_root` call, keyed by slug.
    /// Avoids redundant subprocess spawns when `can_resolve` and `resolve` run
    /// back-to-back for the same input.
    last_discovered: std::sync::Mutex<Option<(String, PathBuf)>>,
}

impl Default for TaskResolver {
    fn default() -> Self {
        Self {
            last_discovered: std::sync::Mutex::new(None),
        }
    }
}

impl TaskResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Try `git kb show --json <slug>` against a specific KB root.
    /// Returns true if the command succeeds.
    /// When `branch` is provided, `GITKB_WORKSPACE` is set for per-worktree isolation.
    async fn kb_show_succeeds(slug: &str, kb_root: &Path, branch: Option<&str>) -> bool {
        let mut cmd = tokio::process::Command::new("git-kb");
        cmd.args(["show", "--json", slug])
            .env("GITKB_ROOT", kb_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Some(b) = branch {
            cmd.env("GITKB_WORKSPACE", b);
        }
        let child = cmd.spawn();

        match child {
            Ok(child) => match tokio::time::timeout(KB_TIMEOUT, child.wait_with_output()).await {
                Ok(Ok(o)) => o.status.success(),
                Ok(Err(e)) => {
                    debug!(slug, ?kb_root, error = %e, "git-kb show failed");
                    false
                }
                Err(_) => {
                    debug!(slug, ?kb_root, "git-kb show timed out");
                    false
                }
            },
            Err(e) => {
                debug!(slug, ?kb_root, error = %e, "failed to spawn git-kb");
                false
            }
        }
    }

    /// Discover sub-project paths via `meta project list --recursive --json`.
    /// Returns empty vec if `meta` is not available or fails.
    async fn discover_meta_projects(workspace_root: &Path) -> Vec<PathBuf> {
        Self::discover_meta_projects_with_env(workspace_root, None).await
    }

    /// Inner implementation that accepts an optional PATH override for testing.
    /// When `path_env` is `Some`, only the subprocess sees the override —
    /// the process-wide environment is never mutated.
    async fn discover_meta_projects_with_env(
        workspace_root: &Path,
        path_env: Option<&str>,
    ) -> Vec<PathBuf> {
        let mut cmd = tokio::process::Command::new("meta");
        cmd.args(["project", "list", "--recursive", "--json"])
            .current_dir(workspace_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Some(path) = path_env {
            cmd.env("PATH", path);
        }
        let child = cmd.spawn();

        let child = match child {
            Ok(c) => c,
            Err(e) => {
                debug!(error = %e, "meta not available, skipping multi-KB discovery");
                return Vec::new();
            }
        };

        let output = match tokio::time::timeout(KB_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(o)) if o.status.success() => o,
            _ => {
                debug!("meta project list failed or timed out");
                return Vec::new();
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        // meta project list --json outputs a JSON object: { "project-name": { "path": "rel/path" }, ... }
        let json: serde_json::Value = match serde_json::from_str(&stdout) {
            Ok(v) => v,
            Err(e) => {
                debug!(error = %e, "failed to parse meta project list JSON");
                return Vec::new();
            }
        };

        let mut paths = Vec::new();
        if let Some(obj) = json.as_object() {
            for (_name, info) in obj {
                // Each entry may be an object with "path" or just a string path
                let rel_path = info.as_str().map(|s| s.to_string()).or_else(|| {
                    info.get("path")
                        .and_then(|v| v.as_str())
                        .map(|p| p.to_string())
                });
                if let Some(rel) = rel_path {
                    let rel = Path::new(&rel);
                    if rel.is_absolute()
                        || rel.components().any(|c| matches!(c, Component::ParentDir))
                    {
                        warn!(path = %rel.display(), "skipping unsafe meta project path");
                        continue;
                    }
                    let joined = workspace_root.join(rel);
                    // Skip paths that resolve to the workspace root itself
                    // (avoids false-positive ambiguity when "." appears in meta).
                    if let (Ok(canon), Ok(root)) =
                        (joined.canonicalize(), workspace_root.canonicalize())
                    {
                        if canon == root {
                            debug!(path = %rel.display(), "skipping workspace root self-reference");
                            continue;
                        }
                    }
                    // Skip paths that don't exist on disk — stale meta entries
                    // would otherwise spawn doomed git-kb subprocesses.
                    if !joined.is_dir() {
                        debug!(path = %joined.display(), "skipping non-existent meta project path");
                        continue;
                    }
                    paths.push(joined);
                }
            }
        }

        paths
    }

    /// Find which KB root contains the given slug.
    /// Tries the primary KB root first, then discovers sub-repos via `meta`.
    /// Returns the KB root path if found.
    async fn discover_kb_root(slug: &str, primary_kb_root: &Path) -> Option<PathBuf> {
        // Prefer the primary KB root, but keep scanning so we can still warn
        // if the same slug exists in multiple KBs.
        let mut found = if Self::kb_show_succeeds(slug, primary_kb_root, None).await {
            Some(primary_kb_root.to_path_buf())
        } else {
            None
        };

        // Try multi-KB discovery via meta
        let sub_projects = Self::discover_meta_projects(primary_kb_root).await;
        if sub_projects.is_empty() {
            return found;
        }

        debug!(
            count = sub_projects.len(),
            "searching sub-projects for task slug"
        );

        // Check sub-projects concurrently with a bounded concurrency limit
        // to avoid exhausting file descriptors in large monorepos.
        // Use `buffered` (not `buffer_unordered`) to preserve input order,
        // giving deterministic "first match wins" semantics.
        use futures::stream::{self, StreamExt};
        let slug_owned = slug.to_string();
        let results: Vec<(PathBuf, bool)> = stream::iter(sub_projects.into_iter())
            .map(|p| {
                let s = slug_owned.clone();
                async move {
                    let hit = Self::kb_show_succeeds(&s, &p, None).await;
                    (p, hit)
                }
            })
            .buffered(KB_DISCOVERY_CONCURRENCY)
            .collect()
            .await;

        for (path, hit) in results {
            if hit {
                if let Some(ref first) = found {
                    warn!(
                        slug,
                        first = %first.display(),
                        duplicate = %path.display(),
                        "ambiguous slug: found in multiple KBs, using first match"
                    );
                } else {
                    debug!(slug, kb_root = %path.display(), "found task in sub-project KB");
                    found = Some(path);
                }
            }
        }

        found
    }

    /// Resolve directive from CLI arg or from task frontmatter `directives:` field.
    async fn resolve_directive(
        cli_directive: Option<Directive>,
        slug: &str,
        kb_root: &Path,
        branch: Option<&str>,
    ) -> Result<Directive> {
        if let Some(m) = cli_directive {
            debug!(directive = %m.as_str(), "directive provided via CLI arg");
            return Ok(m);
        }

        debug!("no CLI directive; reading directives from task frontmatter");
        let mut cmd = tokio::process::Command::new("git-kb");
        cmd.args(["show", "--json", slug])
            .env("GITKB_ROOT", kb_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(b) = branch {
            cmd.env("GITKB_WORKSPACE", b);
        }
        let child = cmd.spawn()?;

        // kill_on_drop(true) ensures the child is killed if the timeout fires
        let output = tokio::time::timeout(KB_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "git-kb show --json {} timed out after {:?}",
                    slug,
                    KB_TIMEOUT
                )
            })??;

        if !output.status.success() {
            anyhow::bail!(
                "git kb show --json {} failed: {}",
                slug,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;

        if let Some(directives) = json.get("directives") {
            match directives {
                serde_json::Value::Array(arr) if !arr.is_empty() => {
                    if let Some(s) = arr[0].as_str() {
                        return s.parse::<Directive>();
                    }
                }
                serde_json::Value::String(s) => {
                    return s.parse::<Directive>();
                }
                _ => {}
            }
        }

        anyhow::bail!(
            "no directive specified: pass --directive or set `directives` in task frontmatter for {}",
            slug
        );
    }

    /// Compute the primary KB root from config, falling back to cwd.
    /// Shared by `resolve()` and `on_cleanup()` so both use the same fallback.
    fn primary_kb_root(config: &AtcConfig) -> PathBuf {
        config
            .dispatch
            .resolved_meta_workspace_root(config.config_dir.as_deref())
            .ok()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default()
    }

    /// CAS-claim a task via `git kb assign`.
    async fn cas_claim(
        slug: &str,
        session_name: &str,
        kb_root: &Path,
        branch: Option<&str>,
    ) -> Result<()> {
        let mut cmd = tokio::process::Command::new("git-kb");
        cmd.args(["assign", slug, session_name])
            .env("GITKB_ROOT", kb_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(b) = branch {
            cmd.env("GITKB_WORKSPACE", b);
        }
        let child = cmd.spawn()?;

        // kill_on_drop(true) ensures the child is killed if the timeout fires
        let output = tokio::time::timeout(KB_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| {
                anyhow::anyhow!("git-kb assign {} timed out after {:?}", slug, KB_TIMEOUT)
            })??;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let msg = if stderr.contains("already assigned") || stderr.contains("already claimed") {
                format!(
                    "task {} is already claimed; use `atc status` to check",
                    slug
                )
            } else {
                format!("failed to claim task {}", slug)
            };
            anyhow::bail!("{}\n{}", msg, stderr.trim());
        }

        Ok(())
    }

    /// Release a CAS claim. Errors are logged but not propagated (best-effort with timeout).
    async fn unassign_task(slug: &str, kb_root: &Path, branch: Option<&str>) {
        let mut cmd = tokio::process::Command::new("git-kb");
        cmd.args(["unassign", slug]).env("GITKB_ROOT", kb_root);
        if let Some(b) = branch {
            cmd.env("GITKB_WORKSPACE", b);
        }
        let status = run_cmd_with_timeout(&mut cmd, KB_TIMEOUT).await;

        match status {
            Ok(Some(s)) if !s.success() => {
                warn!(slug, exit_code = ?s.code(), "git kb unassign exited with error");
            }
            Ok(None) => {
                warn!(slug, "git kb unassign timed out (non-fatal)");
            }
            Err(e) => {
                warn!(slug, error = %e, "git kb unassign failed");
            }
            _ => {
                debug!(slug, "unassign succeeded");
            }
        }
    }
}

#[async_trait]
impl InputResolver for TaskResolver {
    fn name(&self) -> &str {
        "task"
    }

    async fn can_resolve(&self, input: &str, config: &AtcConfig) -> bool {
        let primary = Self::primary_kb_root(config);
        if let Some(found) = Self::discover_kb_root(input, &primary).await {
            // Cache the result so resolve() can skip the redundant discovery
            match self.last_discovered.lock() {
                Ok(mut cache) => *cache = Some((input.to_string(), found)),
                Err(_) => warn!("KB discovery cache lock poisoned, skipping cache write"),
            }
            true
        } else {
            false
        }
    }

    async fn resolve(
        &self,
        input: &str,
        opts: &RunOpts,
        config: &AtcConfig,
    ) -> Result<ResolvedInput> {
        let slug = input;

        // Use cached KB root from can_resolve() if available for the same slug,
        // otherwise perform full discovery.
        // Only consume the cache if the slug matches; otherwise leave it
        // for a potential later call with the correct slug.
        let cached = match self.last_discovered.lock() {
            Ok(mut guard) => {
                if guard.as_ref().is_some_and(|(s, _)| s == slug) {
                    guard.take().map(|(_, path)| path)
                } else {
                    None
                }
            }
            Err(_) => {
                warn!("KB discovery cache lock poisoned, skipping cache read");
                None
            }
        };

        let kb_root = if let Some(path) = cached {
            debug!(slug, kb_root = %path.display(), "using cached KB root from can_resolve");
            path
        } else {
            let primary_kb_root = Self::primary_kb_root(config);
            Self::discover_kb_root(slug, &primary_kb_root)
                .await
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "task slug '{}' not found in any KB (searched primary root and meta sub-projects)",
                        slug
                    )
                })?
        };

        // 1. Derive branch and resolve directive in that workspace
        let branch = derive_branch(slug);
        let resolved_directive =
            Self::resolve_directive(opts.directive.clone(), slug, &kb_root, Some(&branch)).await?;
        info!(%slug, directive = %resolved_directive.as_str(), "directive resolved");

        // 2. Build dispatch ID
        let dispatch_id = build_dispatch_id(&branch, &resolved_directive);
        let session_name = dispatch_id.clone();

        // 3. CAS-claim the task (skip for dry-run to avoid transient state mutation)
        if !opts.dry_run {
            Self::cas_claim(slug, &session_name, &kb_root, Some(&branch)).await?;
        }

        // 4. Render system prompt
        // Pass kb_root as worktree_path fallback so project-level .dispatch/partials/
        // can be resolved before the actual worktree is created by the pipeline.
        let directive_text = opts.directives.as_deref().unwrap_or("");
        let prompt = match atc_core::prompt_engine::render_prompt(
            &resolved_directive,
            slug,
            config,
            directive_text,
            Some(kb_root.as_path()),
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                // Rollback CAS claim on prompt failure (only if we claimed)
                if !opts.dry_run {
                    Self::unassign_task(slug, &kb_root, Some(&branch)).await;
                }
                return Err(e);
            }
        };

        // 5. Build env overrides
        let mut env_overrides = HashMap::new();
        env_overrides.insert("GITKB_WORKSPACE".to_string(), branch.clone());
        env_overrides.insert(
            "GITKB_ROOT".to_string(),
            kb_root.to_string_lossy().into_owned(),
        );

        Ok(ResolvedInput {
            system_prompt: prompt,
            directive: resolved_directive,
            task_slug: Some(slug.to_string()),
            branch,
            dispatch_id,
            env_overrides,
            kb_root: Some(kb_root),
            is_template: false,
            template_body: None,
            max_turns: None,
        })
    }

    async fn on_cleanup(
        &self,
        record: &DispatchRecord,
        config: &AtcConfig,
        registry: Option<&dyn Registry>,
    ) {
        if let Some(ref slug) = record.task_slug {
            // Check if other live (non-terminal) dispatches exist for this slug.
            // If so, don't unassign — the sibling dispatch still holds the claim.
            if let Some(reg) = registry {
                match reg.find_by_task_slug(slug).await {
                    Ok(records) => {
                        let has_other_live = records
                            .iter()
                            .any(|r| r.id != record.id && !r.status.is_terminal());
                        if has_other_live {
                            debug!(
                                slug,
                                id = %record.id,
                                "skipping unassign: another live dispatch exists for this slug"
                            );
                            return;
                        }
                    }
                    Err(e) => {
                        warn!(slug, error = %e, "failed to check for sibling dispatches; skipping unassign for safety");
                        return;
                    }
                }
            }

            // Use the persisted KB root when available, falling back to
            // re-discovery for records created before the field existed.
            let kb_root = if let Some(ref root) = record.kb_root {
                root.clone()
            } else {
                let primary = Self::primary_kb_root(config);
                match Self::discover_kb_root(slug, &primary).await {
                    Some(found) => found,
                    None => {
                        warn!(
                            slug,
                            "could not re-discover KB root for unassign; task may remain assigned"
                        );
                        return;
                    }
                }
            };
            Self::unassign_task(slug, &kb_root, Some(record.branch.as_str())).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_resolver_name() {
        let resolver = TaskResolver::new();
        assert_eq!(resolver.name(), "task");
    }

    #[tokio::test]
    async fn test_discover_meta_projects_meta_not_available() {
        // Use subprocess-level PATH override instead of mutating the process
        // environment, which would race with other test threads.
        let tmp = tempfile::tempdir().unwrap();
        let projects = TaskResolver::discover_meta_projects_with_env(tmp.path(), Some("")).await;
        assert!(
            projects.is_empty(),
            "expected empty vec when meta is not available"
        );
    }

    #[tokio::test]
    async fn test_kb_show_succeeds_nonexistent_slug() {
        // git-kb is unlikely to be in PATH in CI; should return false gracefully
        let tmp = tempfile::tempdir().unwrap();
        let result = TaskResolver::kb_show_succeeds("nonexistent/slug", tmp.path(), None).await;
        assert!(!result);
    }

    #[tokio::test]
    async fn test_discover_kb_root_returns_none_for_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let result = TaskResolver::discover_kb_root("nonexistent/slug", tmp.path()).await;
        assert!(result.is_none());
    }
}
