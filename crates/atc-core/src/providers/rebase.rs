use async_trait::async_trait;
use std::path::Path;
use std::time::Duration;
use tracing::{info, warn};

use super::{ContextOutput, ContextProvider, DispatchContext};

/// Timeout for git subprocess calls (fetch, rev-list).
const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Provider that detects stale branches and injects rebase instructions.
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

    async fn prepare(&self, ctx: &DispatchContext) -> anyhow::Result<ContextOutput> {
        let worktree = &ctx.worktree_path;
        let default_branch = resolve_default_branch(worktree).await;

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
                return Ok(ContextOutput::default());
            }
            Err(_) => {
                warn!("rebase: git fetch timed out");
                return Ok(ContextOutput::default());
            }
        };
        if !fetch_output.status.success() {
            warn!(
                stderr = %String::from_utf8_lossy(&fetch_output.stderr),
                "rebase: git fetch failed"
            );
            return Ok(ContextOutput::default());
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
                return Ok(ContextOutput::default());
            }
            Err(_) => {
                warn!("rebase: git rev-list timed out");
                return Ok(ContextOutput::default());
            }
        };

        if !count_output.status.success() {
            warn!("rebase: git rev-list --count failed");
            return Ok(ContextOutput::default());
        }

        let count_str = String::from_utf8_lossy(&count_output.stdout)
            .trim()
            .to_string();
        let behind: u64 = count_str.parse().unwrap_or(0);

        if behind == 0 {
            info!("rebase: branch is up to date with {}", default_branch);
            return Ok(ContextOutput::default());
        }

        info!(
            behind = behind,
            default_branch = %default_branch,
            "rebase: branch is behind"
        );

        // 3. Build rebase instruction
        let mut instruction = format!(
            "**Rebase required:** {} commits behind `{}`. ",
            behind, default_branch
        );

        // Try to read rebase partial from partials directory
        let partials_dir = &ctx.config.prompt.partials_dir;
        let rebase_partial_path = resolve_partial_path(partials_dir, worktree);
        if let Ok(partial_content) = tokio::fs::read_to_string(&rebase_partial_path).await {
            let rendered = partial_content.replace("{{default_branch}}", &default_branch);
            instruction.push_str(&rendered);
        } else {
            // Fallback instruction
            instruction.push_str(&format!(
                "Before starting work, rebase onto `origin/{}` to avoid merge conflicts:\n\n\
                 ```bash\n\
                 git rebase origin/{}\n\
                 ```\n\n\
                 If there are conflicts, resolve them before proceeding.",
                default_branch, default_branch
            ));
        }

        let mut output = ContextOutput::default();
        output.preamble_sections.push(instruction);
        Ok(output)
    }
}

/// Resolve the default branch by probing git, falling back to "main".
async fn resolve_default_branch(worktree: &Path) -> String {
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

/// Resolve the rebase partial file path.
fn resolve_partial_path(partials_dir: &str, worktree: &Path) -> std::path::PathBuf {
    let partials = if partials_dir.starts_with('.') || !Path::new(partials_dir).is_absolute() {
        worktree.join(partials_dir)
    } else {
        std::path::PathBuf::from(partials_dir)
    };
    partials.join("rebase.md")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_provider_name() {
        let provider = RebaseProvider::new();
        assert_eq!(provider.name(), "rebase");
    }

    #[tokio::test]
    async fn test_resolve_default_branch_fallback() {
        // With a non-existent worktree, should fall back to "main"
        let result = resolve_default_branch(Path::new("/nonexistent")).await;
        assert_eq!(result, "main");
    }

    #[test]
    fn test_resolve_partial_path_relative() {
        let path = resolve_partial_path(".claude/prompts/partials", Path::new("/tmp/worktree"));
        assert_eq!(
            path,
            PathBuf::from("/tmp/worktree/.claude/prompts/partials/rebase.md")
        );
    }

    #[test]
    fn test_resolve_partial_path_absolute() {
        let path = resolve_partial_path("/opt/prompts/partials", Path::new("/tmp/worktree"));
        assert_eq!(path, PathBuf::from("/opt/prompts/partials/rebase.md"));
    }
}
