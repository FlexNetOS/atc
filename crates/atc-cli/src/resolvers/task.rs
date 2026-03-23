use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

use atc_core::config::AtcConfig;
use atc_core::registry::Registry;
use atc_core::resolver::{InputResolver, ResolvedInput};
use atc_core::types::{DispatchRecord, Mode, RunOpts};

use crate::dispatch::{build_dispatch_id, derive_branch};
use crate::subprocess::run_cmd_with_timeout;

/// Timeout for git-kb subprocess calls.
const KB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Resolver for GitKB task dispatches. Consolidates ALL `git kb` interactions.
pub struct TaskResolver;

impl TaskResolver {
    /// Try `git kb show --json <slug>` against a specific KB root.
    /// Returns true if the command succeeds.
    async fn kb_show_succeeds(slug: &str, kb_root: &Path) -> bool {
        let child = tokio::process::Command::new("git-kb")
            .args(["show", "--json", slug])
            .env("GITKB_ROOT", kb_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn();

        match child {
            Ok(child) => match tokio::time::timeout(KB_TIMEOUT, child.wait_with_output()).await {
                Ok(Ok(o)) => o.status.success(),
                _ => false,
            },
            Err(_) => false,
        }
    }

    /// Discover sub-project paths via `meta project list --recursive --json`.
    /// Returns empty vec if `meta` is not available or fails.
    async fn discover_meta_projects(workspace_root: &Path) -> Vec<PathBuf> {
        let child = tokio::process::Command::new("meta")
            .args(["project", "list", "--recursive", "--json"])
            .current_dir(workspace_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn();

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
                let rel_path = if let Some(s) = info.as_str() {
                    Some(s.to_string())
                } else if let Some(p) = info.get("path").and_then(|v| v.as_str()) {
                    Some(p.to_string())
                } else {
                    None
                };
                if let Some(rel) = rel_path {
                    let abs = if Path::new(&rel).is_absolute() {
                        PathBuf::from(&rel)
                    } else {
                        workspace_root.join(&rel)
                    };
                    paths.push(abs);
                }
            }
        }

        paths
    }

    /// Find which KB root contains the given slug.
    /// Tries the primary KB root first, then discovers sub-repos via `meta`.
    /// Returns the KB root path if found.
    async fn discover_kb_root(slug: &str, primary_kb_root: &Path) -> Option<PathBuf> {
        // Try primary KB root first
        if Self::kb_show_succeeds(slug, primary_kb_root).await {
            return Some(primary_kb_root.to_path_buf());
        }

        // Try multi-KB discovery via meta
        let sub_projects = Self::discover_meta_projects(primary_kb_root).await;
        if sub_projects.is_empty() {
            return None;
        }

        debug!(
            count = sub_projects.len(),
            "searching sub-projects for task slug"
        );

        let mut found: Option<PathBuf> = None;
        for project_path in &sub_projects {
            if Self::kb_show_succeeds(slug, project_path).await {
                if let Some(ref first) = found {
                    warn!(
                        slug,
                        first = %first.display(),
                        duplicate = %project_path.display(),
                        "ambiguous slug: found in multiple KBs, using first match"
                    );
                } else {
                    debug!(slug, kb_root = %project_path.display(), "found task in sub-project KB");
                    found = Some(project_path.clone());
                }
            }
        }

        found
    }

    /// Resolve mode from CLI arg or from task frontmatter `directives:` field.
    async fn resolve_mode(cli_mode: Option<Mode>, slug: &str, kb_root: &Path) -> Result<Mode> {
        if let Some(m) = cli_mode {
            debug!(mode = %m.as_str(), "mode provided via CLI arg");
            return Ok(m);
        }

        debug!("no CLI mode; reading directives from task frontmatter");
        let child = tokio::process::Command::new("git-kb")
            .args(["show", "--json", slug])
            .env("GITKB_ROOT", kb_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

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
                        return s.parse::<Mode>();
                    }
                }
                serde_json::Value::String(s) => {
                    return s.parse::<Mode>();
                }
                _ => {}
            }
        }

        anyhow::bail!(
            "no mode specified: pass --mode or set `directives` in task frontmatter for {}",
            slug
        );
    }

    /// CAS-claim a task via `git kb assign`.
    async fn cas_claim(slug: &str, session_name: &str, kb_root: &Path) -> Result<()> {
        let child = tokio::process::Command::new("git-kb")
            .args(["assign", slug, session_name])
            .env("GITKB_ROOT", kb_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

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
    async fn unassign_task(slug: &str, kb_root: &Path) {
        let status = run_cmd_with_timeout(
            tokio::process::Command::new("git-kb")
                .args(["unassign", slug])
                .env("GITKB_ROOT", kb_root),
            KB_TIMEOUT,
        )
        .await;

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
        let kb_root = config
            .dispatch
            .resolved_meta_workspace_root(config.config_dir.as_deref())
            .ok()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();

        Self::discover_kb_root(input, &kb_root).await.is_some()
    }

    async fn resolve(
        &self,
        input: &str,
        opts: &RunOpts,
        config: &AtcConfig,
    ) -> Result<ResolvedInput> {
        let slug = input;

        // Resolve kb_root — try primary, then multi-KB discovery
        let cwd = std::env::current_dir().unwrap_or_default();
        let primary_kb_root = config
            .dispatch
            .resolved_meta_workspace_root(config.config_dir.as_deref())
            .unwrap_or_else(|_| cwd.clone());

        let kb_root = Self::discover_kb_root(slug, &primary_kb_root)
            .await
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "task slug '{}' not found in any KB (searched primary root and meta sub-projects)",
                    slug
                )
            })?;

        // 1. Resolve mode
        let mode = Self::resolve_mode(opts.mode.clone(), slug, &kb_root).await?;
        info!(%slug, mode = %mode.as_str(), "mode resolved");

        // 2. Derive branch and dispatch ID
        let branch = derive_branch(slug);
        let dispatch_id = build_dispatch_id(&branch, &mode);
        let session_name = dispatch_id.clone();

        // 3. CAS-claim the task (skip for dry-run to avoid transient state mutation)
        if !opts.dry_run {
            Self::cas_claim(slug, &session_name, &kb_root).await?;
        }

        // 4. Render system prompt
        // Pass kb_root as worktree_path fallback so project-level .dispatch/partials/
        // can be resolved before the actual worktree is created by the pipeline.
        let directive = opts.directives.as_deref().unwrap_or("");
        let prompt = match atc_core::prompt_engine::render_prompt(
            &mode,
            slug,
            config,
            directive,
            Some(kb_root.as_path()),
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                // Rollback CAS claim on prompt failure (only if we claimed)
                if !opts.dry_run {
                    Self::unassign_task(slug, &kb_root).await;
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
            mode,
            task_slug: Some(slug.to_string()),
            branch,
            dispatch_id,
            env_overrides,
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

            // Use the same kb_root fallback as resolve() — config → cwd
            let kb_root = config
                .dispatch
                .resolved_meta_workspace_root(config.config_dir.as_deref())
                .ok()
                .or_else(|| std::env::current_dir().ok());
            if let Some(kb_root) = kb_root {
                Self::unassign_task(slug, &kb_root).await;
            } else {
                warn!(
                    slug,
                    "could not resolve kb_root for unassign (no config, no cwd)"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_resolver_name() {
        let resolver = TaskResolver;
        assert_eq!(resolver.name(), "task");
    }

    #[tokio::test]
    async fn test_discover_meta_projects_meta_not_available() {
        // When meta is not in PATH, discovery should return empty vec (graceful skip)
        let tmp = tempfile::tempdir().unwrap();
        let projects = TaskResolver::discover_meta_projects(tmp.path()).await;
        // This may or may not be empty depending on whether `meta` is installed,
        // but it should NOT panic or error.
        let _ = projects;
    }

    #[tokio::test]
    async fn test_kb_show_succeeds_nonexistent_slug() {
        // git-kb is unlikely to be in PATH in CI; should return false gracefully
        let tmp = tempfile::tempdir().unwrap();
        let result = TaskResolver::kb_show_succeeds("nonexistent/slug", tmp.path()).await;
        assert!(!result);
    }

    #[tokio::test]
    async fn test_discover_kb_root_returns_none_for_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let result = TaskResolver::discover_kb_root("nonexistent/slug", tmp.path()).await;
        assert!(result.is_none());
    }
}
