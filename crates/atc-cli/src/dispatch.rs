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

/// Sanitize a string by replacing `/` with `--`. Used for:
/// - Deriving branch names from slugs (`tasks/foo` → `tasks--foo`)
/// - Sanitizing branch names for worktree names and dispatch IDs
///
/// Bijective for valid GitKB slugs (ABNF segments cannot contain `--`).
pub fn sanitize_slashes(s: &str) -> String {
    s.replace('/', "--")
}

/// Derive branch name from slug: replace `/` with `--`.
pub fn derive_branch(slug: &str) -> String {
    sanitize_slashes(slug)
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
    let safe_branch = sanitize_slashes(branch);
    let ts = Utc::now().timestamp_millis();
    let seq = DISPATCH_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let suffix = nanos ^ std::process::id() ^ seq;
    format!(
        "{}@{}@{}-{:04x}",
        safe_branch,
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

/// Result of resolving a document's workspace location.
#[derive(Debug, Clone)]
pub struct DocumentWorkspace {
    /// Filesystem path for agent CWD.
    pub cwd: PathBuf,
    /// KB workspace branch name (e.g., "main", "tasks--harmony-350").
    pub workspace_branch: String,
}

/// Resolve where a document lives and where the agent should work.
///
/// Phase 1: Collect all KB workspaces that contain the slug.
///
/// Phase 2: Select the best match using priority rules:
///   1. Current branch (if the slug is checked out there)
///   2. Non-main branch (if exactly one; ambiguity is warned)
///   3. Main workspace (fallback)
///
/// Phase 3: If non-main, find the corresponding code worktree.
///
/// Returns `Ok(None)` if the document isn't checked out in any workspace.
/// Returns `Err` on I/O failures (permission denied, etc.) so callers can
/// distinguish a genuine miss from a scan failure.
pub async fn resolve_document_workspace(
    slug: &str,
    kb_root: &Path,
    worktree_base: &Path,
    workspace_root: &Path,
) -> Result<Option<DocumentWorkspace>> {
    // Validate slug before any path join to prevent traversal/injection.
    let slug_path = Path::new(slug);
    if slug.is_empty()
        || slug_path.is_absolute()
        || slug.contains('\\')
        || slug.contains('\0')
        || slug_path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir
                    | std::path::Component::CurDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        anyhow::bail!("invalid document slug for workspace resolution: {}", slug);
    }

    let workspaces_dir = kb_root.join(".kb/workspaces");

    // Phase 1: Collect all workspaces that contain this slug.
    let mut matches: Vec<String> = Vec::new();

    let main_path = workspaces_dir.join("main").join(format!("{}.md", slug));
    if main_path.try_exists().map_err(|e| {
        anyhow::anyhow!(
            "failed to stat KB workspace document {}: {}",
            main_path.display(),
            e
        )
    })? {
        matches.push("main".to_string());
    }

    // Propagate read_dir errors (permission denied, etc.) instead of silently
    // falling through to the "not found" path.
    let entries = match std::fs::read_dir(&workspaces_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Workspaces dir doesn't exist yet — that's a genuine miss.
            return Ok(None);
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "failed to scan KB workspaces at {}: {}",
                workspaces_dir.display(),
                e
            ));
        }
    };

    for entry in entries {
        let entry = entry.map_err(|e| {
            anyhow::anyhow!(
                "failed to read KB workspace entry under {}: {}",
                workspaces_dir.display(),
                e
            )
        })?;
        let branch_name = entry.file_name().to_string_lossy().into_owned();
        if branch_name == "main" {
            continue;
        }
        // Reject directory names that could escape the workspaces path.
        if branch_name.contains('/')
            || branch_name.contains('\\')
            || branch_name == ".."
            || branch_name == "."
        {
            warn!(branch_name, "skipping unsafe workspace directory name");
            continue;
        }
        let doc_path = entry.path().join(format!("{}.md", slug));
        if doc_path.try_exists().map_err(|e| {
            anyhow::anyhow!(
                "failed to stat KB workspace document {}: {}",
                doc_path.display(),
                e
            )
        })? {
            matches.push(branch_name);
        }
    }

    if matches.is_empty() {
        return Ok(None);
    }

    // Phase 2: Select best match.
    // Prefer the current git branch if it's among the matches.
    let current_branch = tokio::time::timeout(
        SUBPROCESS_TIMEOUT,
        tokio::process::Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(workspace_root)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()
    .and_then(|r| r.ok())
    .and_then(|o| {
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    });

    let selected = if let Some(ref current) = current_branch {
        let sanitized_current = current.replace('/', "--");
        // Find the actual matched value from the vector (may be the original
        // or sanitized form depending on workspace directory naming).
        if let Some(matched) = matches
            .iter()
            .find(|m| *m == current || *m == &sanitized_current)
        {
            matched.clone()
        } else {
            match select_from_matches(&matches, slug) {
                Some(s) => s,
                None => return Ok(None),
            }
        }
    } else {
        match select_from_matches(&matches, slug) {
            Some(s) => s,
            None => return Ok(None),
        }
    };

    // Phase 3: Resolve code worktree for non-main branches.
    if selected == "main" {
        Ok(Some(DocumentWorkspace {
            cwd: workspace_root.to_path_buf(),
            workspace_branch: "main".to_string(),
        }))
    } else {
        let cwd = find_worktree_for_branch(&selected, worktree_base, workspace_root)
            .unwrap_or_else(|| {
                warn!(branch = %selected, "no worktree found for branch, falling back to workspace_root");
                workspace_root.to_path_buf()
            });
        Ok(Some(DocumentWorkspace {
            cwd,
            workspace_branch: selected,
        }))
    }
}

/// Pick a single workspace from the match set, logging ambiguity.
fn select_from_matches(matches: &[String], slug: &str) -> Option<String> {
    let non_main: Vec<&String> = matches.iter().filter(|m| m.as_str() != "main").collect();
    match non_main.len() {
        0 => {
            // Only main matched.
            Some("main".to_string())
        }
        1 => Some(non_main[0].clone()),
        _ => {
            // Ambiguous: slug is in multiple non-main workspaces and the current
            // branch didn't resolve the tie. Warn and pick the first alphabetically
            // for determinism rather than silently using filesystem order.
            let mut sorted: Vec<&String> = non_main;
            sorted.sort();
            warn!(
                slug,
                workspaces = ?sorted,
                "document found in multiple workspaces; selecting first alphabetically"
            );
            Some(sorted[0].clone())
        }
    }
}

/// Find a code worktree corresponding to a KB workspace branch.
///
/// Search order:
/// 1. `<worktree_base>/<branch>/` — meta git worktrees (e.g., /tmp/worktrees/tasks--harmony-350/)
/// 2. `<workspace_root>/.worktrees/<branch>/` — local git worktrees
///
/// Returns `None` if no worktree exists for this branch.
pub fn find_worktree_for_branch(
    branch: &str,
    worktree_base: &Path,
    workspace_root: &Path,
) -> Option<PathBuf> {
    let sanitized = branch.replace('/', "--");

    // 1. Check configured worktree_base (default: /tmp/worktrees/)
    for name in [sanitized.as_str(), branch] {
        let path = worktree_base.join(name);
        if path.exists() {
            return Some(path);
        }
    }

    // 2. Check local .worktrees/ directory
    for name in [sanitized.as_str(), branch] {
        let path = workspace_root.join(".worktrees").join(name);
        if path.exists() {
            return Some(path);
        }
    }

    // 3. No worktree found — agent works in canonical repo on this branch
    None
}

/// Auto-checkout a document to the main KB workspace.
pub async fn auto_checkout_to_main(slug: &str, kb_root: &Path) -> Result<()> {
    info!(slug, kb_root = %kb_root.display(), "auto-checking out document to main workspace");
    let child = tokio::process::Command::new("git-kb")
        .args(["checkout", slug])
        .env("GITKB_ROOT", kb_root)
        .env("GITKB_WORKSPACE", "main")
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let output = tokio::time::timeout(SUBPROCESS_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            anyhow::anyhow!("git-kb checkout timed out after {:?}", SUBPROCESS_TIMEOUT)
        })??;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(
            slug,
            stderr = %stderr,
            "git-kb checkout failed for document (non-fatal)"
        );
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
    let sanitized_branch = sanitize_slashes(branch);
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
    // Reuse sanitized_branch for the worktree name (git branch keeps original slashes).
    if !repos.is_empty() {
        let args = build_meta_worktree_args(&sanitized_branch, repos, branch);

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

/// Build the argument list for `meta git worktree create`.
///
/// Always passes `--recursive` so nested `.meta.yaml` `depends_on` entries
/// are resolved transitively.
fn build_meta_worktree_args<'a>(
    sanitized_branch: &'a str,
    repos: &[&'a str],
    branch: &'a str,
) -> Vec<&'a str> {
    let mut args = vec!["git", "worktree", "create", sanitized_branch, "--recursive"];
    for r in repos {
        args.push("--repo");
        args.push(r);
    }
    args.extend(["--branch", branch]);
    args
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
    fn test_build_dispatch_id_sanitizes_slashes() {
        let id = build_dispatch_id("fix/simplify-rebase-msg", &Directive::ReviewFix);
        assert!(
            !id.contains('/'),
            "dispatch ID must not contain slashes (would create subdirs in log path), got: {id}"
        );
        assert!(
            id.starts_with("fix--simplify-rebase-msg@review-fix@"),
            "expected sanitized branch in ID, got: {id}"
        );
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

    #[tokio::test]
    async fn test_resolve_document_workspace_finds_in_main() {
        let dir = tempfile::tempdir().unwrap();
        let kb_root = dir.path();
        let main_ws = kb_root.join(".kb/workspaces/main/tasks");
        std::fs::create_dir_all(&main_ws).unwrap();
        std::fs::write(main_ws.join("harmony-350.md"), "---\ntitle: test\n---\n").unwrap();

        let result = resolve_document_workspace(
            "tasks/harmony-350",
            kb_root,
            Path::new("/tmp/worktrees"),
            kb_root,
        )
        .await
        .unwrap();
        assert!(result.is_some());
        let ws = result.unwrap();
        assert_eq!(ws.workspace_branch, "main");
        assert_eq!(ws.cwd, kb_root);
    }

    #[tokio::test]
    async fn test_resolve_document_workspace_finds_in_worktree_branch() {
        let dir = tempfile::tempdir().unwrap();
        let kb_root = dir.path();

        // Create a non-main workspace with the doc
        let branch_ws = kb_root.join(".kb/workspaces/tasks--harmony-350/tasks");
        std::fs::create_dir_all(&branch_ws).unwrap();
        std::fs::write(branch_ws.join("harmony-350.md"), "---\ntitle: test\n---\n").unwrap();

        // Create a worktree directory that matches
        let wt_base = dir.path().join("worktrees");
        let wt_path = wt_base.join("tasks--harmony-350");
        std::fs::create_dir_all(&wt_path).unwrap();

        let result = resolve_document_workspace("tasks/harmony-350", kb_root, &wt_base, kb_root)
            .await
            .unwrap();
        assert!(result.is_some());
        let ws = result.unwrap();
        assert_eq!(ws.workspace_branch, "tasks--harmony-350");
        assert_eq!(ws.cwd, wt_path);
    }

    #[tokio::test]
    async fn test_resolve_document_workspace_returns_none_when_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let kb_root = dir.path();

        // Create main workspace but without the target doc
        let main_ws = kb_root.join(".kb/workspaces/main");
        std::fs::create_dir_all(&main_ws).unwrap();

        let result = resolve_document_workspace(
            "tasks/nonexistent",
            kb_root,
            Path::new("/tmp/worktrees"),
            kb_root,
        )
        .await
        .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_resolve_document_workspace_ambiguous_warns_and_picks_alphabetically() {
        let dir = tempfile::tempdir().unwrap();
        let kb_root = dir.path();

        // Create doc in two non-main workspaces (branch-b and branch-a)
        for branch in &["branch-b", "branch-a"] {
            let ws = kb_root.join(format!(".kb/workspaces/{}/tasks", branch));
            std::fs::create_dir_all(&ws).unwrap();
            std::fs::write(ws.join("harmony-999.md"), "---\ntitle: test\n---\n").unwrap();
        }

        let result = resolve_document_workspace(
            "tasks/harmony-999",
            kb_root,
            Path::new("/tmp/worktrees"),
            kb_root,
        )
        .await
        .unwrap();
        assert!(result.is_some());
        let ws = result.unwrap();
        // Should pick first alphabetically
        assert_eq!(ws.workspace_branch, "branch-a");
    }

    #[test]
    fn test_find_worktree_for_branch_worktree_base() {
        let dir = tempfile::tempdir().unwrap();
        let wt_base = dir.path().join("worktrees");
        let wt_path = wt_base.join("tasks--foo");
        std::fs::create_dir_all(&wt_path).unwrap();

        let workspace_root = dir.path().join("repo");
        std::fs::create_dir_all(&workspace_root).unwrap();

        let result = find_worktree_for_branch("tasks--foo", &wt_base, &workspace_root);
        assert_eq!(result, Some(wt_path));
    }

    #[test]
    fn test_find_worktree_for_branch_local_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let workspace_root = dir.path().join("repo");
        let local_wt = workspace_root.join(".worktrees/tasks--bar");
        std::fs::create_dir_all(&local_wt).unwrap();

        let wt_base = dir.path().join("empty-base");
        std::fs::create_dir_all(&wt_base).unwrap();

        let result = find_worktree_for_branch("tasks--bar", &wt_base, &workspace_root);
        assert_eq!(result, Some(local_wt));
    }

    #[test]
    fn test_find_worktree_for_branch_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let wt_base = dir.path().join("empty");
        std::fs::create_dir_all(&wt_base).unwrap();
        let workspace_root = dir.path().join("repo");
        std::fs::create_dir_all(&workspace_root).unwrap();

        let result = find_worktree_for_branch("nonexistent", &wt_base, &workspace_root);
        assert_eq!(result, None);
    }

    #[test]
    fn test_build_meta_worktree_args_includes_recursive() {
        let args = build_meta_worktree_args(
            "tasks--harmony-407",
            &["open-source/gitkb/core"],
            "tasks/harmony-407",
        );
        assert!(
            args.contains(&"--recursive"),
            "args must include --recursive for nested dep resolution: {args:?}"
        );
        assert_eq!(
            args,
            vec![
                "git",
                "worktree",
                "create",
                "tasks--harmony-407",
                "--recursive",
                "--repo",
                "open-source/gitkb/core",
                "--branch",
                "tasks/harmony-407",
            ]
        );
    }

    #[test]
    fn test_build_meta_worktree_args_multiple_repos() {
        let args =
            build_meta_worktree_args("my-branch", &["repo-a", "repo-b"], "feature/my-branch");
        assert!(args.contains(&"--recursive"));
        assert_eq!(
            args.iter().filter(|a| **a == "--repo").count(),
            2,
            "should have two --repo flags"
        );
    }
}
