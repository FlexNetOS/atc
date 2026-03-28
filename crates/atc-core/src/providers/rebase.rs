use async_trait::async_trait;
use std::path::Path;
use std::time::Duration;
use tracing::{info, warn};

use super::{ContextOutput, ContextProvider, DispatchContext};

/// Timeout for git subprocess calls (fetch, rev-list).
const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Provider that detects stale branches and injects rebase data.
///
/// Exports template variables:
/// - `default_branch` — the repo's default branch (e.g. "main")
/// - `rebase_behind_count` — how many commits HEAD is behind origin/default_branch ("0" if up to date)
///
/// When behind > 0, also adds a brief preamble alert. Templates that want
/// full rebase instructions should include the `{{>rebase}}` partial, which
/// uses `{{default_branch}}` from these template vars.
#[derive(Default)]
pub struct RebaseProvider;

impl RebaseProvider {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ContextProvider for RebaseProvider {
    fn name(&self) -> &str {
        "rebase"
    }

    fn declared_template_vars(&self) -> &[&str] {
        &["default_branch", "rebase_behind_count"]
    }

    async fn prepare(&self, ctx: &DispatchContext) -> anyhow::Result<ContextOutput> {
        let worktree = &ctx.worktree_path;
        let default_branch = resolve_default_branch(worktree).await;

        let mut output = ContextOutput::default();

        // Always export default_branch — templates and partials use it regardless
        // of whether a rebase is needed.
        output
            .template_vars
            .insert("default_branch".to_string(), default_branch.clone());

        // 1. Fetch latest from origin (quiet, time-bounded)
        let fetch_result = tokio::time::timeout(
            GIT_TIMEOUT,
            tokio::process::Command::new("git")
                .args([
                    "-C",
                    &worktree.to_string_lossy(),
                    "fetch",
                    "origin",
                    &default_branch,
                    "--quiet",
                ])
                .kill_on_drop(true)
                .output(),
        )
        .await;

        let fetch_output = match fetch_result {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                warn!(error = %e, "rebase: git fetch failed");
                output
                    .template_vars
                    .insert("rebase_behind_count".to_string(), "0".to_string());
                return Ok(output);
            }
            Err(_) => {
                warn!("rebase: git fetch timed out");
                output
                    .template_vars
                    .insert("rebase_behind_count".to_string(), "0".to_string());
                return Ok(output);
            }
        };
        if !fetch_output.status.success() {
            warn!(
                stderr = %String::from_utf8_lossy(&fetch_output.stderr),
                "rebase: git fetch failed"
            );
            output
                .template_vars
                .insert("rebase_behind_count".to_string(), "0".to_string());
            return Ok(output);
        }

        // 2. Count commits behind (time-bounded)
        let count_result = tokio::time::timeout(
            GIT_TIMEOUT,
            tokio::process::Command::new("git")
                .args([
                    "-C",
                    &worktree.to_string_lossy(),
                    "rev-list",
                    &format!("HEAD..origin/{}", default_branch),
                    "--count",
                ])
                .kill_on_drop(true)
                .output(),
        )
        .await;

        let count_output = match count_result {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                warn!(error = %e, "rebase: git rev-list failed");
                output
                    .template_vars
                    .insert("rebase_behind_count".to_string(), "0".to_string());
                return Ok(output);
            }
            Err(_) => {
                warn!("rebase: git rev-list timed out");
                output
                    .template_vars
                    .insert("rebase_behind_count".to_string(), "0".to_string());
                return Ok(output);
            }
        };

        if !count_output.status.success() {
            warn!("rebase: git rev-list --count failed");
            output
                .template_vars
                .insert("rebase_behind_count".to_string(), "0".to_string());
            return Ok(output);
        }

        let count_str = String::from_utf8_lossy(&count_output.stdout)
            .trim()
            .to_string();
        let behind: u64 = count_str.parse().unwrap_or(0);

        output
            .template_vars
            .insert("rebase_behind_count".to_string(), behind.to_string());

        if behind == 0 {
            info!("rebase: branch is up to date with {}", default_branch);
            return Ok(output);
        }

        info!(
            behind = behind,
            default_branch = %default_branch,
            "rebase: branch is behind"
        );

        // 3. Brief preamble alert — detailed instructions live in {{>rebase}} partial
        output.preamble_sections.push(format!(
            "**Rebase required:** {} commits behind `{}`. Rebase before starting work.",
            behind, default_branch
        ));

        Ok(output)
    }
}

/// Resolve the default branch by probing git, falling back to "main".
pub async fn resolve_default_branch(worktree: &Path) -> String {
    // Try `git symbolic-ref refs/remotes/origin/HEAD` to detect the default branch
    if let Ok(Ok(output)) = tokio::time::timeout(
        GIT_TIMEOUT,
        tokio::process::Command::new("git")
            .args([
                "-C",
                &worktree.to_string_lossy(),
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    {
        if output.status.success() {
            let refname = String::from_utf8_lossy(&output.stdout).trim().to_string();
            // refs/remotes/origin/main → main
            if let Some(branch) = refname.strip_prefix("refs/remotes/origin/") {
                return branch.to_string();
            }
        }
    }
    "main".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_name() {
        let provider = RebaseProvider::new();
        assert_eq!(provider.name(), "rebase");
    }

    #[test]
    fn test_declared_template_vars() {
        let provider = RebaseProvider::new();
        assert_eq!(
            provider.declared_template_vars(),
            &["default_branch", "rebase_behind_count"]
        );
    }

    #[tokio::test]
    async fn test_resolve_default_branch_fallback() {
        // With a non-existent worktree, should fall back to "main"
        let result = resolve_default_branch(Path::new("/nonexistent")).await;
        assert_eq!(result, "main");
    }
}
