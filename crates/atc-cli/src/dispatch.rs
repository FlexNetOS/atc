//! Shared dispatch utilities used by the DispatchPipeline and resolvers.
//!
//! This module contains infrastructure for worktree management, branch derivation,
//! dispatch ID generation, and other shared concerns. The actual dispatch orchestration
//! lives in `pipeline.rs`.

use anyhow::Result;
use atc_core::registry::Registry;
use atc_core::types::{Mode, Status};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use tracing::{debug, info, warn};

/// Derive branch name from slug: replace `/` with `--`.
///
/// This is bijective for valid GitKB slugs, which conform to the ABNF
/// `segment = 1*(ALPHA / DIGIT / "-" / "_")` — segments cannot contain `--`.
/// If a slug ever contains `--` natively, this mapping would collide;
/// slug validation (git-kb's ABNF enforcement) prevents that.
pub fn derive_branch(slug: &str) -> String {
    slug.replace('/', "--")
}

/// Process-local counter to guarantee unique dispatch IDs even when two
/// calls occur within the same millisecond (e.g. scripted/CI environments).
static DISPATCH_SEQ: AtomicU32 = AtomicU32::new(0);

/// Build dispatch ID: `<branch>@<mode>@<unix-ms>-<rand>`.
///
/// The 4-hex-digit suffix mixes a monotonic counter with the PID and
/// sub-millisecond time, guaranteeing uniqueness within a process and
/// making cross-process collisions effectively impossible.
pub fn build_dispatch_id(branch: &str, mode: &Mode) -> String {
    let ts = Utc::now().timestamp_millis();
    let seq = DISPATCH_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let suffix = nanos ^ std::process::id() ^ seq;
    format!(
        "{}@{}@{}-{:04x}",
        branch,
        mode.as_str(),
        ts,
        suffix & 0xffff
    )
}

/// Validate a branch name via `git check-ref-format`.
pub async fn validate_branch_name(branch: &str) -> Result<()> {
    let output = tokio::process::Command::new("git")
        .args(["check-ref-format", "--branch", branch])
        .output()
        .await?;

    if !output.status.success() {
        anyhow::bail!(
            "invalid branch name '{}': git check-ref-format rejected it",
            branch
        );
    }
    Ok(())
}

/// Resolve GH_TOKEN via env vars → `gh auth token` fallback.
pub async fn resolve_gh_token() -> Result<String> {
    if let Ok(t) = std::env::var("GH_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    if let Ok(t) = std::env::var("GITHUB_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    // fallback: gh auth token
    let out = tokio::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "could not resolve GH_TOKEN: gh auth token failed (exit {:?})",
            out.status.code()
        );
    }
    let token = String::from_utf8(out.stdout)?.trim().to_string();
    if token.is_empty() {
        anyhow::bail!("could not resolve GH_TOKEN: gh auth token returned an empty token");
    }
    Ok(token)
}

/// Compute AGENT_ALLOWED_PATHS for agent sandbox.
pub fn compute_allowed_paths(worktree_root: &Path, extra_paths: &[String]) -> String {
    let mut paths = vec![
        worktree_root.to_string_lossy().into_owned(),
        "/tmp".to_string(),
        "/private/tmp".to_string(),
    ];
    for p in extra_paths {
        if !paths.contains(p) {
            paths.push(p.clone());
        }
    }
    paths.join(":")
}

/// Write diagnostic `.diag` file alongside log.
pub async fn write_diag_file(log_dir: &Path, dispatch_id: &str, gh_token_present: bool) {
    let diag_path = log_dir.join(format!("{}.diag", dispatch_id));
    let mut content = format!(
        "GH_TOKEN set: {}\n",
        if gh_token_present { "yes" } else { "no" }
    );

    // gh auth status (best effort)
    if let Ok(output) = tokio::process::Command::new("gh")
        .args(["auth", "status"])
        .output()
        .await
    {
        content.push_str(&format!(
            "gh auth status (exit {}):\n{}\n{}\n",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }

    if let Err(e) = tokio::fs::write(&diag_path, &content).await {
        warn!(error = %e, "failed to write .diag file");
    }
}

/// Check if a tmux session exists.
pub async fn tmux_session_alive(session: &str) -> bool {
    tokio::process::Command::new("tmux")
        .args(["has-session", "-t", session])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Result of meta workspace discovery.
pub struct MetaDiscovery {
    /// Primary repo alias (e.g., "core")
    pub repo: String,
    /// Workspace root directory (where meta manages sub-repos)
    pub workspace_root: PathBuf,
}

/// Discover meta workspace info using `meta project list --recursive --json`.
/// Returns None if `meta` is not in PATH or fails.
pub async fn discover_meta(cwd: &Path) -> Option<MetaDiscovery> {
    let output = tokio::process::Command::new("meta")
        .args(["project", "list", "--recursive", "--json"])
        .current_dir(cwd)
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    let obj = json.as_object()?;

    if obj.is_empty() {
        return None;
    }

    let workspace_root = obj
        .get(".")
        .and_then(|v| v.get("path"))
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.to_path_buf());

    let repo = obj
        .iter()
        .find(|(_, v)| v.get("provides").is_some())
        .or_else(|| obj.iter().find(|(k, _)| k.as_str() != "."))
        .map(|(name, _)| name.clone());

    match repo {
        Some(repo) => {
            debug!(repo = %repo, root = %workspace_root.display(), "meta workspace discovered");
            Some(MetaDiscovery {
                repo,
                workspace_root,
            })
        }
        None => None,
    }
}

/// Check running dispatches on a worktree path: mark stale records as Failed,
/// bail on live sessions unless `force` is set.
async fn check_worktree_collision(
    running: &[atc_core::types::DispatchRecord],
    worktree_path: &Path,
    registry: &dyn Registry,
    force: bool,
) -> Result<()> {
    for r in running {
        let alive = tmux_session_alive(&r.session).await;
        if alive && !force {
            anyhow::bail!(
                "Worktree {} is in use by dispatch {} (session: {}). Use --force to override.",
                worktree_path.display(),
                r.id,
                r.session,
            );
        }
        if !alive {
            info!(id = %r.id, "marking stale Running record as Failed (dead tmux session)");
            if let Err(e) = registry.update_status(&r.id, Status::Failed).await {
                warn!(id = %r.id, error = %e, "failed to mark stale record as Failed");
            }
        } else if force {
            info!(id = %r.id, session = %r.session, "force-overriding live dispatch; killing session and marking as Failed");
            let _ = tokio::process::Command::new("tmux")
                .args(["kill-session", "-t", &r.session])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await;
            if let Err(e) = registry.update_status(&r.id, Status::Failed).await {
                warn!(id = %r.id, error = %e, "failed to mark force-overridden record as Failed");
            }
        }
    }
    Ok(())
}

/// Parameters for worktree creation/reuse.
pub struct WorktreeOpts<'a> {
    pub worktree_base: &'a Path,
    pub repo: Option<&'a str>,
    pub branch: &'a str,
    pub meta_workspace_root: &'a Path,
    /// Root path for KB operations. Passed as GITKB_ROOT to meta worktree commands.
    pub kb_root: &'a Path,
    pub force: bool,
}

/// Result of worktree creation/reuse, carrying metadata needed for safe rollback.
pub struct WorktreeResult {
    /// The filesystem path of the worktree.
    pub path: PathBuf,
    /// Whether this worktree was newly created by this call (vs reused).
    pub created: bool,
    /// Whether the worktree was created via `meta` (true) or plain `git` (false).
    pub is_meta: bool,
}

/// Ensure a worktree exists for the given branch. Reuses existing worktrees.
#[tracing::instrument(skip(opts, registry), fields(branch = opts.branch))]
pub async fn ensure_worktree(
    opts: &WorktreeOpts<'_>,
    registry: &dyn Registry,
) -> Result<WorktreeResult> {
    let worktree_base = opts.worktree_base;
    let repo = opts.repo;
    let branch = opts.branch;
    let meta_workspace_root = opts.meta_workspace_root;
    let kb_root = opts.kb_root;
    let force = opts.force;

    // Use branch name as the worktree directory name so each dispatch gets a
    // unique path. Previously this used kb_basename, which caused every
    // dispatch to collide on the same worktree name.
    let worktree_path = match repo {
        Some(r) => worktree_base.join(branch).join(r),
        None => worktree_base.join(branch),
    };

    // Collision detection
    let running = registry.find_running_on_worktree(&worktree_path).await?;
    check_worktree_collision(&running, &worktree_path, registry, force).await?;

    // Check if worktree already exists for this branch.
    let probe_dir = match repo {
        Some(r) => {
            let repo_dir = meta_workspace_root.join(r);
            if repo_dir.exists() {
                repo_dir
            } else {
                meta_workspace_root.to_path_buf()
            }
        }
        None => meta_workspace_root.to_path_buf(),
    };
    let output = tokio::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(&probe_dir)
        .output()
        .await?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut current_path: Option<String> = None;
        for line in stdout.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                current_path = Some(path.to_string());
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                if b == branch {
                    if let Some(ref existing) = current_path {
                        let reused_path = PathBuf::from(existing);
                        if reused_path != worktree_path {
                            let reused_running =
                                registry.find_running_on_worktree(&reused_path).await?;
                            check_worktree_collision(
                                &reused_running,
                                &reused_path,
                                registry,
                                force,
                            )
                            .await?;
                        }
                        info!(branch, path = %existing, "reusing existing worktree");
                        let _ = tokio::process::Command::new("git")
                            .args(["-C", existing, "fetch", "origin"])
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status()
                            .await;
                        return Ok(WorktreeResult {
                            path: reused_path,
                            created: false,
                            is_meta: repo.is_some(),
                        });
                    }
                }
            } else if line.is_empty() {
                current_path = None;
            }
        }
    }

    // No existing worktree — create a new one
    if let Some(repo_alias) = repo {
        let output = tokio::process::Command::new("meta")
            .args([
                "git", "worktree", "create", branch, "--repo", repo_alias, "--branch", branch,
            ])
            .env("META_WORKTREES", worktree_base)
            .env("GITKB_ROOT", kb_root)
            .current_dir(meta_workspace_root)
            .output()
            .await?;

        if !output.status.success() {
            anyhow::bail!(
                "meta git worktree create failed (exit {:?}):\n{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    } else {
        let branch_exists = tokio::process::Command::new("git")
            .args(["rev-parse", "--verify", &format!("refs/heads/{}", branch)])
            .current_dir(meta_workspace_root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);

        let wt_path_str = worktree_path.to_string_lossy();
        let mut args = vec!["worktree", "add", &wt_path_str];
        if !branch_exists {
            args.push("-b");
        }
        args.push(branch);

        let output = tokio::process::Command::new("git")
            .args(&args)
            .current_dir(meta_workspace_root)
            .output()
            .await?;

        if !output.status.success() {
            anyhow::bail!(
                "git worktree add failed (exit {:?}):\n{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    Ok(WorktreeResult {
        path: worktree_path,
        created: true,
        is_meta: repo.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_branch() {
        assert_eq!(derive_branch("tasks/gitkb-42"), "tasks--gitkb-42");
        assert_eq!(derive_branch("tasks/gitkb-264"), "tasks--gitkb-264");
        assert_eq!(
            derive_branch("tasks/deep/nested/slug"),
            "tasks--deep--nested--slug"
        );
        assert_eq!(derive_branch("simple"), "simple");
    }

    #[test]
    fn test_derive_branch_edge_cases() {
        assert_eq!(derive_branch(""), "");
        assert_eq!(derive_branch("tasks/"), "tasks--");
        assert_eq!(derive_branch("/tasks"), "--tasks");
        assert_eq!(derive_branch("a//b"), "a----b");
        assert_eq!(derive_branch("no-slashes-here"), "no-slashes-here");
    }

    #[test]
    fn test_build_dispatch_id_format() {
        let id = build_dispatch_id("tasks--gitkb-42", &Mode::Implement);
        let parts: Vec<&str> = id.split('@').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "tasks--gitkb-42");
        assert_eq!(parts[1], "implement");
        let ts_rand: Vec<&str> = parts[2].split('-').collect();
        assert_eq!(
            ts_rand.len(),
            2,
            "expected ts-rand format, got: {}",
            parts[2]
        );
        let ts: i64 = ts_rand[0].parse().expect("timestamp should be a number");
        assert!(ts > 0);
        assert_eq!(ts_rand[1].len(), 4);
        u16::from_str_radix(ts_rand[1], 16).expect("suffix should be valid hex");
    }

    #[test]
    fn test_build_dispatch_id_uniqueness() {
        let id1 = build_dispatch_id("tasks--foo", &Mode::Implement);
        let id2 = build_dispatch_id("tasks--foo", &Mode::Implement);
        assert_ne!(id1, id2, "consecutive dispatch IDs should differ");
    }

    #[test]
    fn test_compute_allowed_paths() {
        let result = compute_allowed_paths(Path::new("/tmp/wt"), &[]);
        assert!(result.contains("/tmp/wt"));
        assert!(result.contains("/tmp"));
        assert!(result.contains("/private/tmp"));

        let result = compute_allowed_paths(Path::new("/tmp/wt"), &["/extra/path".to_string()]);
        assert!(result.contains("/extra/path"));
    }

    #[test]
    fn test_derive_branch_shell_metacharacters() {
        assert_eq!(derive_branch("tasks/$(whoami)"), "tasks--$(whoami)");
        assert_eq!(derive_branch("tasks/;rm -rf /"), "tasks--;rm -rf --");
    }

    #[test]
    fn test_derive_branch_double_hyphen_invariant() {
        assert_eq!(derive_branch("tasks/a--b"), "tasks--a--b");
    }
}
