use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::Path;
use tracing::{debug, info, warn};

use atc_core::config::AtcConfig;
use atc_core::resolver::{InputResolver, ResolvedInput};
use atc_core::types::{DispatchRecord, Mode, RunOpts};

use crate::dispatch::{build_dispatch_id, derive_branch};

/// Resolver for GitKB task dispatches. Consolidates ALL `git kb` interactions.
pub struct TaskResolver;

impl TaskResolver {
    /// Resolve mode from CLI arg or from task frontmatter `directives:` field.
    async fn resolve_mode(cli_mode: Option<Mode>, slug: &str, kb_root: &Path) -> Result<Mode> {
        if let Some(m) = cli_mode {
            debug!(mode = %m.as_str(), "mode provided via CLI arg");
            return Ok(m);
        }

        debug!("no CLI mode; reading directives from task frontmatter");
        let output = tokio::process::Command::new("git-kb")
            .args(["show", "--json", slug])
            .env("GITKB_ROOT", kb_root)
            .output()
            .await?;

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
        let output = tokio::process::Command::new("git-kb")
            .args(["assign", slug, session_name])
            .env("GITKB_ROOT", kb_root)
            .output()
            .await?;

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

    /// Release a CAS claim. Errors are logged but not propagated.
    async fn unassign_task(slug: &str, kb_root: &Path) {
        let status = tokio::process::Command::new("git-kb")
            .args(["unassign", slug])
            .env("GITKB_ROOT", kb_root)
            .status()
            .await;

        match status {
            Ok(s) if !s.success() => {
                warn!(slug, exit_code = ?s.code(), "git kb unassign exited with error");
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
        // Check if input looks like a task slug — try `git kb show --json`
        let kb_root = config
            .dispatch
            .resolved_meta_workspace_root(config.config_dir.as_deref())
            .ok();
        let kb_root = match kb_root {
            Some(r) => r,
            None => {
                // Try CWD as fallback
                std::env::current_dir().unwrap_or_default()
            }
        };

        let output = tokio::process::Command::new("git-kb")
            .args(["show", "--json", input])
            .env("GITKB_ROOT", &kb_root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .await;

        match output {
            Ok(o) => o.status.success(),
            Err(_) => false,
        }
    }

    async fn resolve(
        &self,
        input: &str,
        opts: &RunOpts,
        config: &AtcConfig,
    ) -> Result<ResolvedInput> {
        let slug = input;

        // Resolve kb_root
        let cwd = std::env::current_dir().unwrap_or_default();
        let kb_root = config
            .dispatch
            .resolved_meta_workspace_root(config.config_dir.as_deref())
            .unwrap_or_else(|_| cwd.clone());

        // 1. Resolve mode
        let mode = Self::resolve_mode(opts.mode.clone(), slug, &kb_root).await?;
        info!(%slug, mode = %mode.as_str(), "mode resolved");

        // 2. Derive branch and dispatch ID
        let branch = derive_branch(slug);
        let dispatch_id = build_dispatch_id(&branch, &mode);
        let session_name = dispatch_id.clone();

        // 3. CAS-claim the task
        Self::cas_claim(slug, &session_name, &kb_root).await?;

        // 4. Render system prompt
        let directive = opts.directives.as_deref().unwrap_or("");
        let prompt = match atc_core::prompt_engine::render_prompt(
            &mode, slug, config, directive, None, // worktree_path filled later by pipeline
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                // Rollback CAS claim on prompt failure
                Self::unassign_task(slug, &kb_root).await;
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

    async fn on_cleanup(&self, record: &DispatchRecord, config: &AtcConfig) {
        if let Some(ref slug) = record.task_slug {
            let kb_root = config
                .dispatch
                .resolved_meta_workspace_root(config.config_dir.as_deref())
                .ok();
            if let Some(kb_root) = kb_root {
                Self::unassign_task(slug, &kb_root).await;
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
}
