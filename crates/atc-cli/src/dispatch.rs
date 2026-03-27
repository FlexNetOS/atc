//! Shared dispatch utilities used by the DispatchPipeline and resolvers.
//!
//! This module contains infrastructure for worktree management, branch derivation,
//! dispatch ID generation, and other shared concerns. The actual dispatch orchestration
//! lives in `pipeline.rs`.

use anyhow::Result;
use atc_core::registry::Registry;
use atc_core::types::{Directive, Status};
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

/// Build dispatch ID: `<branch>@<directive>@<unix-ms>-<rand>`.
///
/// The 4-hex-digit suffix mixes a monotonic counter with the PID and
/// sub-millisecond time, guaranteeing uniqueness within a process and
/// making cross-process collisions effectively impossible.
pub fn build_dispatch_id(branch: &str, directive: &Directive) -> String {
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
        directive.as_str(),
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

/// Timeout for subprocess helpers (`gh pr view`, `meta project list`, etc.).
const SUBPROCESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Extract the head branch name from a GitHub PR URL via `gh pr view`.
pub async fn extract_pr_head_branch(pr_url: &str) -> Result<String> {
    let output = tokio::time::timeout(
        SUBPROCESS_TIMEOUT,
        tokio::process::Command::new("gh")
            .args([
                "pr",
                "view",
                pr_url,
                "--json",
                "headRefName",
                "-q",
                ".headRefName",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("gh pr view timed out after {:?}", SUBPROCESS_TIMEOUT))??;

    if !output.status.success() {
        anyhow::bail!(
            "gh pr view --json headRefName failed (exit {:?}):\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let branch = String::from_utf8(output.stdout)?;
    let branch = branch.trim().to_string();
    if branch.is_empty() {
        anyhow::bail!("gh pr view returned empty headRefName for {}", pr_url);
    }
    Ok(branch)
}

/// Search a `meta project list --recursive --json` tree for the project whose
/// `repo` URL matches `target` (an `org/repo` string). Returns the relative
/// path from the workspace root (e.g. `"open-source/atc"`).
fn find_repo(value: &serde_json::Value, prefix: &str, target: &str) -> Option<String> {
    let projects = value.get("projects").and_then(|p| p.as_array())?;

    for project in projects {
        let Some(path) = project.get("path").and_then(|p| p.as_str()) else {
            continue;
        };
        let rel = std::path::Path::new(path);
        if rel.is_absolute()
            || rel.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        {
            warn!(path = %rel.display(), "skipping unsafe meta project path");
            continue;
        }

        let full = if prefix.is_empty() || prefix == "." {
            path.to_string()
        } else {
            format!("{}/{}", prefix, path)
        };

        if let Some(repo_url) = project.get("repo").and_then(|v| v.as_str()) {
            let normalized = repo_url
                .trim_start_matches("git@github.com:")
                .trim_start_matches("https://github.com/")
                .trim_end_matches(".git");
            if normalized == target {
                return Some(full);
            }
        }

        // Recurse into nested projects
        if project.get("projects").is_some() {
            if let Some(found) = find_repo(project, &full, target) {
                return Some(found);
            }
        }
    }
    None
}

/// Resolve a GitHub PR URL to a local repo path within a meta workspace.
///
/// Extracts org/repo from the PR URL and searches `meta project list --recursive --json`
/// for a matching remote URL. Returns the relative path (e.g., "open-source/atc").
pub async fn resolve_pr_repo_path(
    pr_url: &str,
    meta_workspace_root: &Path,
) -> Result<Option<String>> {
    // Extract org/repo from PR URL: "https://github.com/harmony-labs/atc/pull/27" → "harmony-labs/atc"
    let github_repo = pr_url
        .strip_prefix("https://github.com/")
        .and_then(|s| s.split("/pull/").next())
        .ok_or_else(|| anyhow::anyhow!("cannot extract org/repo from PR URL: {}", pr_url))?;

    let output = tokio::time::timeout(
        SUBPROCESS_TIMEOUT,
        tokio::process::Command::new("meta")
            .args(["project", "list", "--recursive", "--json"])
            .current_dir(meta_workspace_root)
            .kill_on_drop(true)
            .output(),
    )
    .await;

    let output = match output {
        Ok(Ok(o)) if o.status.success() => o,
        Ok(Ok(o)) => {
            debug!(
                "meta project list failed (exit {:?}), cannot resolve PR repo path",
                o.status.code()
            );
            return Ok(None);
        }
        Ok(Err(e)) => {
            debug!("meta not available: {}, cannot resolve PR repo path", e);
            return Ok(None);
        }
        Err(_) => {
            debug!(
                "meta project list timed out after {:?}, cannot resolve PR repo path",
                SUBPROCESS_TIMEOUT
            );
            return Ok(None);
        }
    };

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;

    Ok(find_repo(&json, "", github_repo))
}

/// Parse a comment URL to extract the comment ID and type.
///
/// Returns `(comment_id, comment_type)` where comment_type is one of:
/// "issue", "review_comment", "review", or empty string if unrecognized.
pub fn parse_comment_url(comment_url: &str) -> (Option<String>, Option<String>) {
    if let Some(fragment) = comment_url.split('#').nth(1) {
        let id: String = fragment.chars().filter(|c| c.is_ascii_digit()).collect();
        let ctype = if fragment.starts_with("issuecomment-") {
            "issue"
        } else if fragment.starts_with("discussion_r") {
            "review_comment"
        } else if fragment.starts_with("pullrequestreview-") {
            "review"
        } else {
            return (None, None);
        };

        if id.is_empty() {
            return (None, None);
        }

        (Some(id), Some(ctype.to_string()))
    } else {
        (None, None)
    }
}

/// Derive a PR URL from a comment URL by stripping the fragment.
///
/// "https://github.com/org/repo/pull/42#issuecomment-123" → "https://github.com/org/repo/pull/42"
pub fn derive_pr_url_from_comment(comment_url: &str) -> Option<String> {
    let base = comment_url.split('#').next()?;
    // Verify it looks like a PR URL
    if base.contains("/pull/") {
        Some(base.to_string())
    } else {
        None
    }
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
    /// Target repo path(s). Empty = no meta worktree, single = current behavior,
    /// multiple = multi-repo worktree set.
    pub repos: Vec<&'a str>,
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
    let repos = &opts.repos;
    let branch = opts.branch;
    let meta_workspace_root = opts.meta_workspace_root;
    let kb_root = opts.kb_root;
    let force = opts.force;

    // Use sanitized branch name as the worktree directory name so each dispatch
    // gets a unique path. Slashes replaced with -- to avoid nested directories.
    let primary_repo = repos.first().copied();
    let sanitized_branch = branch.replace('/', "--");
    let worktree_path = match primary_repo {
        Some(r) => worktree_base.join(&sanitized_branch).join(r),
        None => worktree_base.join(&sanitized_branch),
    };

    // Collision detection
    let running = registry.find_running_on_worktree(&worktree_path).await?;
    check_worktree_collision(&running, &worktree_path, registry, force).await?;

    // Check if worktree already exists for this branch.
    let probe_dir = match primary_repo {
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
                            is_meta: !repos.is_empty(),
                        });
                    }
                }
            } else if line.is_empty() {
                current_path = None;
            }
        }
    }

    // No existing worktree — create a new one.
    // Worktree NAME must not contain path separators — sanitize by replacing / with --.
    // The git BRANCH name (--branch) keeps the original value (slashes are valid in git refs).
    let worktree_name = branch.replace('/', "--");
    if !repos.is_empty() {
        let mut args = vec!["git", "worktree", "create", &worktree_name];
        for r in repos {
            args.push("--repo");
            args.push(r);
        }
        args.extend(["--branch", branch]);

        let output = tokio::process::Command::new("meta")
            .args(&args)
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
        is_meta: !repos.is_empty(),
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
        let id = build_dispatch_id("tasks--gitkb-42", &Directive::Implement);
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
        let id1 = build_dispatch_id("tasks--foo", &Directive::Implement);
        let id2 = build_dispatch_id("tasks--foo", &Directive::Implement);
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

    #[test]
    fn test_find_repo_nested_meta_workspace() {
        // Simulates the output of `meta project list --recursive --json`
        let json: serde_json::Value = serde_json::json!({
            "path": ".",
            "repo": "git@github.com:harmony-labs/harmony.git",
            "projects": [
                {
                    "name": "clients",
                    "path": "clients",
                    "repo": "git@github.com:harmony-labs/harmony-clients.git",
                    "is_meta": true,
                    "projects": [
                        {
                            "name": "desktop",
                            "path": "desktop",
                            "repo": "git@github.com:harmony-labs/harmony-desktop.git"
                        },
                        {
                            "name": "mobile",
                            "path": "mobile",
                            "repo": "https://github.com/harmony-labs/harmony-mobile.git"
                        }
                    ]
                },
                {
                    "name": "open-source",
                    "path": "open-source",
                    "repo": "git@github.com:harmony-labs/harmony-oss.git",
                    "is_meta": true,
                    "projects": [
                        {
                            "name": "atc",
                            "path": "atc",
                            "repo": "git@github.com:harmony-labs/atc.git"
                        },
                        {
                            "name": "gitkb",
                            "path": "gitkb",
                            "repo": "git@github.com:harmony-labs/harmony-gitkb.git",
                            "is_meta": true,
                            "projects": [
                                {
                                    "name": "core",
                                    "path": "core",
                                    "repo": "git@github.com:harmony-labs/gitkb-core.git"
                                }
                            ]
                        }
                    ]
                },
                {
                    "name": "platform",
                    "path": "platform",
                    "repo": "git@github.com:harmony-labs/harmony-platform.git",
                    "is_meta": true,
                    "projects": [
                        {
                            "name": "api",
                            "path": "api",
                            "repo": "https://github.com/harmony-labs/harmony-api"
                        }
                    ]
                }
            ]
        });

        // Nested child: clients/desktop
        assert_eq!(
            resolve_pr_repo_path_sync(&json, "harmony-labs/harmony-desktop"),
            Some("clients/desktop".to_string())
        );

        // Nested child: open-source/atc
        assert_eq!(
            resolve_pr_repo_path_sync(&json, "harmony-labs/atc"),
            Some("open-source/atc".to_string())
        );

        // Nested child: platform/api (https URL, no .git suffix)
        assert_eq!(
            resolve_pr_repo_path_sync(&json, "harmony-labs/harmony-api"),
            Some("platform/api".to_string())
        );

        // Deep nesting: open-source/gitkb/core
        assert_eq!(
            resolve_pr_repo_path_sync(&json, "harmony-labs/gitkb-core"),
            Some("open-source/gitkb/core".to_string())
        );

        // HTTPS URL with .git suffix
        assert_eq!(
            resolve_pr_repo_path_sync(&json, "harmony-labs/harmony-mobile"),
            Some("clients/mobile".to_string())
        );

        // Non-existent repo
        assert_eq!(
            resolve_pr_repo_path_sync(&json, "harmony-labs/nonexistent"),
            None
        );

        // Top-level group (not a leaf repo match)
        assert_eq!(
            resolve_pr_repo_path_sync(&json, "harmony-labs/harmony-clients"),
            Some("clients".to_string())
        );
    }

    /// Synchronous helper that delegates to the module-level `find_repo`.
    fn resolve_pr_repo_path_sync(json: &serde_json::Value, target: &str) -> Option<String> {
        super::find_repo(json, "", target)
    }

    #[test]
    fn test_find_repo_skips_missing_path_and_unsafe_entries() {
        let json: serde_json::Value = serde_json::json!({
            "projects": [
                {
                    "name": "no-path",
                    "repo": "git@github.com:org/no-path.git"
                },
                {
                    "name": "traversal",
                    "path": "../escaped",
                    "repo": "git@github.com:org/escaped.git"
                },
                {
                    "name": "absolute",
                    "path": "/etc/passwd",
                    "repo": "git@github.com:org/absolute.git"
                },
                {
                    "name": "dot",
                    "path": ".",
                    "repo": "git@github.com:org/dot.git"
                },
                {
                    "name": "valid",
                    "path": "valid-repo",
                    "repo": "git@github.com:org/valid.git"
                }
            ]
        });

        // Missing path → skipped, doesn't abort
        assert_eq!(resolve_pr_repo_path_sync(&json, "org/no-path"), None);
        // Path traversal → skipped
        assert_eq!(resolve_pr_repo_path_sync(&json, "org/escaped"), None);
        // Absolute path → skipped
        assert_eq!(resolve_pr_repo_path_sync(&json, "org/absolute"), None);
        // CurDir path → skipped
        assert_eq!(resolve_pr_repo_path_sync(&json, "org/dot"), None);
        // Valid entry after all bad ones → found
        assert_eq!(
            resolve_pr_repo_path_sync(&json, "org/valid"),
            Some("valid-repo".to_string())
        );
    }

    #[test]
    fn test_parse_comment_url_issue_comment() {
        let (id, ctype) =
            parse_comment_url("https://github.com/org/repo/pull/42#issuecomment-123456");
        assert_eq!(id.as_deref(), Some("123456"));
        assert_eq!(ctype.as_deref(), Some("issue"));
    }

    #[test]
    fn test_parse_comment_url_review_comment() {
        let (id, ctype) =
            parse_comment_url("https://github.com/org/repo/pull/42#discussion_r789012");
        assert_eq!(id.as_deref(), Some("789012"));
        assert_eq!(ctype.as_deref(), Some("review_comment"));
    }

    #[test]
    fn test_parse_comment_url_review() {
        let (id, ctype) =
            parse_comment_url("https://github.com/org/repo/pull/42#pullrequestreview-456789");
        assert_eq!(id.as_deref(), Some("456789"));
        assert_eq!(ctype.as_deref(), Some("review"));
    }

    #[test]
    fn test_parse_comment_url_no_fragment() {
        let (id, ctype) = parse_comment_url("https://github.com/org/repo/pull/42");
        assert!(id.is_none());
        assert!(ctype.is_none());
    }

    #[test]
    fn test_parse_comment_url_unknown_fragment() {
        let (id, ctype) = parse_comment_url("https://github.com/org/repo/pull/42#unknown-fragment");
        assert!(id.is_none());
        assert!(ctype.is_none());
    }

    #[test]
    fn test_derive_pr_url_from_comment() {
        assert_eq!(
            derive_pr_url_from_comment("https://github.com/org/repo/pull/42#issuecomment-123"),
            Some("https://github.com/org/repo/pull/42".to_string())
        );
        assert_eq!(
            derive_pr_url_from_comment("https://github.com/org/repo/pull/42#discussion_r789"),
            Some("https://github.com/org/repo/pull/42".to_string())
        );
        // No fragment
        assert_eq!(
            derive_pr_url_from_comment("https://github.com/org/repo/pull/42"),
            Some("https://github.com/org/repo/pull/42".to_string())
        );
        // Not a PR URL
        assert_eq!(
            derive_pr_url_from_comment("https://github.com/org/repo/issues/42#issuecomment-123"),
            None
        );
    }
}
