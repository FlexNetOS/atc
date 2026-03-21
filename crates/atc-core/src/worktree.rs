//! Shared worktree cleanup utilities.
//!
//! Used by `atc cleanup`, post-completion (Phase 2), and health checks (Phase 7).

use std::path::Path;
use tracing::{info, warn};

/// Known safe base directories for worktree removal.
const KNOWN_BASES: &[&str] = &["/tmp/worktrees/"];

/// Remove a git worktree if it passes safety checks.
///
/// Returns `true` if the worktree was successfully removed, `false` otherwise.
///
/// Safety: only removes the worktree if its path starts with `worktree_base`,
/// `/tmp/worktrees/`, or contains `/.worktrees/`.
///
/// After removal, attempts to clean up the empty parent directory if it falls
/// within the worktree base.
pub async fn cleanup_worktree(worktree_path: &Path, worktree_base: &Path) -> anyhow::Result<bool> {
    if !worktree_path.exists() {
        info!(
            worktree = %worktree_path.display(),
            "worktree path does not exist; nothing to remove"
        );
        return Ok(false);
    }

    if !is_safe_worktree_path(worktree_path, worktree_base) {
        warn!(
            worktree = %worktree_path.display(),
            worktree_base = %worktree_base.display(),
            "refusing to remove worktree: path is not under a known worktree base"
        );
        return Ok(false);
    }

    // Use `git worktree remove --force` to remove the worktree
    let status = tokio::process::Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(worktree_path)
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .status()
        .await;

    match status {
        Ok(s) if s.success() => {
            info!(
                worktree = %worktree_path.display(),
                "worktree removed"
            );

            // Attempt to rmdir parent if empty and inside worktree_base
            if let Some(parent) = worktree_path.parent() {
                if parent.starts_with(worktree_base) && parent != worktree_base {
                    let _ = std::fs::remove_dir(parent); // ignore error (not empty)
                }
            }

            Ok(true)
        }
        Ok(s) => {
            warn!(
                worktree = %worktree_path.display(),
                exit_code = ?s.code(),
                "git worktree remove failed"
            );
            Ok(false)
        }
        Err(e) => {
            warn!(
                worktree = %worktree_path.display(),
                error = %e,
                "git worktree remove failed"
            );
            Ok(false)
        }
    }
}

/// Check whether a worktree path is safe to remove.
///
/// A path is considered safe if:
/// - It starts with `worktree_base`, OR
/// - It starts with a known base (`/tmp/worktrees/`), OR
/// - It contains `/.worktrees/` somewhere in the path
pub fn is_safe_worktree_path(worktree_path: &Path, worktree_base: &Path) -> bool {
    // Reject any path containing ".." components to prevent traversal attacks
    if worktree_path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return false;
    }

    let path_str = worktree_path.to_string_lossy();

    // Under configured worktree_base
    if worktree_path.starts_with(worktree_base) {
        return true;
    }

    // Under known bases
    for base in KNOWN_BASES {
        if path_str.starts_with(base) {
            return true;
        }
    }

    // Contains .worktrees as a real path component (not bypassable via "..")
    if worktree_path
        .components()
        .any(|c| c.as_os_str() == ".worktrees")
    {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_safe_under_worktree_base() {
        let base = PathBuf::from("/home/user/worktrees");
        let path = PathBuf::from("/home/user/worktrees/my-branch");
        assert!(is_safe_worktree_path(&path, &base));
    }

    #[test]
    fn test_safe_under_tmp_worktrees() {
        let base = PathBuf::from("/some/other/base");
        let path = PathBuf::from("/tmp/worktrees/harmony/my-branch");
        assert!(is_safe_worktree_path(&path, &base));
    }

    #[test]
    fn test_safe_with_dot_worktrees() {
        let base = PathBuf::from("/some/other/base");
        let path = PathBuf::from("/home/user/project/.worktrees/feature-x");
        assert!(is_safe_worktree_path(&path, &base));
    }

    #[test]
    fn test_unsafe_random_path() {
        let base = PathBuf::from("/home/user/worktrees");
        let path = PathBuf::from("/home/user/projects/my-repo");
        assert!(!is_safe_worktree_path(&path, &base));
    }

    #[test]
    fn test_unsafe_root_path() {
        let base = PathBuf::from("/home/user/worktrees");
        let path = PathBuf::from("/");
        assert!(!is_safe_worktree_path(&path, &base));
    }

    #[test]
    fn test_unsafe_home_dir() {
        let base = PathBuf::from("/tmp/worktrees");
        let path = PathBuf::from("/home/user");
        assert!(!is_safe_worktree_path(&path, &base));
    }

    #[test]
    fn test_unsafe_path_traversal_under_known_base() {
        let base = PathBuf::from("/home/user/worktrees");
        let path = PathBuf::from("/tmp/worktrees/../../../etc/passwd");
        // Even though the string starts with /tmp/worktrees/, the ".." components
        // cause early rejection to prevent traversal attacks.
        assert!(!is_safe_worktree_path(&path, &base));
    }

    #[test]
    fn test_unsafe_path_traversal_outside_all_bases() {
        let base = PathBuf::from("/home/user/worktrees");
        let path = PathBuf::from("/opt/../../../etc/passwd");
        assert!(!is_safe_worktree_path(&path, &base));
    }

    #[test]
    fn test_unsafe_dot_worktrees_with_traversal() {
        let base = PathBuf::from("/some/base");
        let path = PathBuf::from("/home/project/.worktrees/../../important-dir");
        // ".." components are rejected even when .worktrees is present
        assert!(!is_safe_worktree_path(&path, &base));
    }

    #[test]
    fn test_worktree_base_itself_is_safe() {
        let base = PathBuf::from("/tmp/worktrees");
        let path = PathBuf::from("/tmp/worktrees");
        assert!(is_safe_worktree_path(&path, &base));
    }
}
