use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::executor::{AgentExecutor, AgentOpts};
use atc_core::registry::Registry;
use atc_core::types::{DispatchOpts, DispatchOutcome, DispatchRecord, HealthChecks, Mode, Status};
use chrono::Utc;
use std::collections::HashMap;
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
async fn validate_branch_name(branch: &str) -> Result<()> {
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
async fn resolve_gh_token() -> Result<String> {
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
fn compute_allowed_paths(worktree_root: &Path, extra_paths: &[String]) -> String {
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

/// Write `.envrc` file to worktree with env overrides.
/// Currently unused — will be called once InputResolver provides per-task overrides (phase-1).
#[allow(dead_code)]
async fn write_envrc(worktree_path: &Path, env_overrides: &HashMap<String, String>) -> Result<()> {
    if env_overrides.is_empty() {
        return Ok(());
    }
    let mut content = String::new();
    for (k, v) in env_overrides {
        // Validate key is a safe shell identifier (letters, digits, underscore; not starting with digit)
        if !k.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            anyhow::bail!("invalid env key: {:?}", k);
        }
        // Single-quote values with interior single-quote escaping to prevent
        // shell injection via $(), backticks, or double-quote expansion.
        let escaped = v.replace('\'', "'\\''");
        content.push_str(&format!("export {}='{}'\n", k, escaped));
    }
    let envrc_path = worktree_path.join(".envrc");
    tokio::fs::write(&envrc_path, &content).await?;
    Ok(())
}

/// Write diagnostic `.diag` file alongside log.
async fn write_diag_file(log_dir: &Path, dispatch_id: &str, gh_token_present: bool) {
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
async fn tmux_session_alive(session: &str) -> bool {
    tokio::process::Command::new("tmux")
        .args(["has-session", "-t", session])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Post a comment on a PR via `gh pr comment`.
async fn post_pr_comment(pr_url: &str, body: &str) {
    let result = tokio::process::Command::new("gh")
        .args(["pr", "comment", pr_url, "--body", body])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
    match result {
        Ok(s) if !s.success() => {
            warn!(pr_url, "gh pr comment failed (non-fatal)");
        }
        Err(e) => {
            warn!(pr_url, error = %e, "gh pr comment failed (non-fatal)");
        }
        _ => {}
    }
}

/// Result of meta workspace discovery.
struct MetaDiscovery {
    /// Primary repo alias (e.g., "core")
    repo: String,
    /// Workspace root directory (where meta manages sub-repos)
    workspace_root: PathBuf,
}

/// Discover meta workspace info using `meta project list --recursive --json`.
/// Never inspects .meta.yaml — meta's CLI is the only interface to meta's internals.
/// Returns None if `meta` is not in PATH or fails (i.e., not a meta workspace).
async fn discover_meta(cwd: &Path) -> Option<MetaDiscovery> {
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

    // Determine workspace root from the "." project's path, or fall back to cwd
    let workspace_root = obj
        .get(".")
        .and_then(|v| v.get("path"))
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.to_path_buf());

    // Find primary repo: project with "provides" key, or first non-root project
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

/// Resolve mode from CLI arg or from task frontmatter `directives` field.
#[tracing::instrument(skip(kb_root), fields(slug))]
async fn resolve_mode(cli_mode: Option<Mode>, slug: &str, kb_root: &Path) -> Result<Mode> {
    if let Some(m) = cli_mode {
        debug!(mode = %m.as_str(), "mode provided via CLI arg");
        return Ok(m);
    }

    debug!("no CLI mode; reading directives from task frontmatter");
    // Fall back to reading directives from task frontmatter
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

    // Parse directives field — handles both inline `[implement]` and block YAML formats.
    // In JSON output, both parse to a JSON array.
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
        "no mode specified: pass a mode argument or set `directives` in task frontmatter for {}",
        slug
    );
}

/// CAS-claim a task via `git kb assign`.
#[tracing::instrument(skip(kb_root))]
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

/// Release a CAS claim on failure. Errors are logged but not propagated.
#[tracing::instrument(skip(kb_root))]
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

/// Roll back the CAS claim and (if newly created) the worktree on early failure.
///
/// Shared by prompt-rendering and agent-spawn error paths so that a failure
/// after `cas_claim()` / `ensure_worktree()` doesn't leave orphaned state.
async fn rollback_claim_and_worktree(
    slug: &str,
    kb_root: &Path,
    wt_created: bool,
    wt_is_meta: bool,
    worktree_path: &Path,
    workspace_root: &Path,
) {
    unassign_task(slug, kb_root).await;
    // Only tear down worktrees we created; reused checkouts may have
    // local changes or be shared with other dispatches.
    if wt_created {
        let cmd = if wt_is_meta { "meta" } else { "git" };
        let mut args: Vec<&str> = Vec::new();
        if wt_is_meta {
            args.extend(["git", "worktree", "remove", "--force"]);
        } else {
            args.extend(["worktree", "remove", "--force"]);
        }
        let wt_str = worktree_path.to_string_lossy();
        args.push(&wt_str);
        let _ = tokio::process::Command::new(cmd)
            .args(&args)
            .current_dir(workspace_root)
            .status()
            .await;
    }
}

/// Check running dispatches on a worktree path: mark stale records as Failed,
/// bail on live sessions unless `force` is set.
///
/// Checks each record exactly once to avoid TOCTOU races from calling
/// `tmux_session_alive` twice on the same session.
async fn check_worktree_collision(
    running: &[DispatchRecord],
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
            // Force-override: kill the displaced tmux session and mark as Failed
            // so the registry doesn't retain an orphaned Running record.
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
struct WorktreeOpts<'a> {
    worktree_base: &'a Path,
    kb_basename: &'a str,
    repo: Option<&'a str>,
    branch: &'a str,
    meta_workspace_root: &'a Path,
    kb_root: &'a Path,
    force: bool,
}

/// Result of worktree creation/reuse, carrying metadata needed for safe rollback.
struct WorktreeResult {
    /// The filesystem path of the worktree.
    path: PathBuf,
    /// Whether this worktree was newly created by this call (vs reused).
    created: bool,
    /// Whether the worktree was created via `meta` (true) or plain `git` (false).
    is_meta: bool,
}

/// Ensure a worktree exists for the given branch. Reuses existing worktrees.
#[tracing::instrument(skip(opts, registry), fields(branch = opts.branch))]
async fn ensure_worktree(
    opts: &WorktreeOpts<'_>,
    registry: &dyn Registry,
) -> Result<WorktreeResult> {
    let worktree_base = opts.worktree_base;
    let kb_basename = opts.kb_basename;
    let repo = opts.repo;
    let branch = opts.branch;
    let meta_workspace_root = opts.meta_workspace_root;
    let kb_root = opts.kb_root;
    let force = opts.force;
    // Compute expected worktree path.
    // For meta repos: <worktree_base>/<kb_basename>/<repo_alias>
    // For plain git:  <worktree_base>/<kb_basename>/<branch>
    // Including branch in the plain-git path prevents collisions when multiple
    // branches are dispatched in the same workspace.
    let worktree_path = match repo {
        Some(r) => worktree_base.join(kb_basename).join(r),
        None => worktree_base.join(kb_basename).join(branch),
    };

    // Collision detection: check if another dispatch is running on this worktree
    let running = registry.find_running_on_worktree(&worktree_path).await?;
    check_worktree_collision(&running, &worktree_path, registry, force).await?;

    // Check if worktree already exists for this branch.
    // In meta workspaces, the meta root may not itself be a git checkout,
    // so we probe the resolved repo directory instead (if it exists).
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
        // Parse porcelain output to find existing worktree for branch
        let mut current_path: Option<String> = None;
        for line in stdout.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                current_path = Some(path.to_string());
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                if b == branch {
                    if let Some(ref existing) = current_path {
                        let reused_path = PathBuf::from(existing);
                        // Re-check collision against the actual on-disk path, which
                        // may differ from the computed worktree_path (e.g. if
                        // worktree_base changed between dispatches).
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
                        // Fetch latest from origin
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
                "git",
                "worktree",
                "create",
                kb_basename,
                "--repo",
                repo_alias,
                "--branch",
                branch,
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
        // Plain git worktree (non-meta repo)
        // Check if the branch already exists locally to avoid `-b` failing
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

/// Print post-dispatch confirmation block.
fn print_dispatch_confirmation(
    task_slug: Option<&str>,
    mode: &Mode,
    id: &str,
    branch: &str,
    worktree_path: &Path,
    session: &str,
    log_file: &Path,
) {
    let slug_display = task_slug.unwrap_or("(none)");
    println!("Dispatched: {}", slug_display);
    println!("  Mode:      {}", mode.as_str());
    println!("  ID:        {}", id);
    println!("  Branch:    {}", branch);
    println!("  Worktree:  {}", worktree_path.display());
    println!("  Session:   {}", session);
    println!("  Log:       {}", log_file.display());
}

/// Execute the full dispatch flow.
#[tracing::instrument(skip(config, registry, executor))]
pub async fn dispatch(
    config: &AtcConfig,
    registry: &dyn Registry,
    executor: &dyn AgentExecutor,
    opts: &DispatchOpts,
) -> Result<DispatchOutcome> {
    let slug = &opts.slug;
    let dispatch_cfg = &config.dispatch;

    // Resolve config paths
    let worktree_base = dispatch_cfg.resolved_worktree_base();
    let log_dir = dispatch_cfg.resolved_log_dir();

    // Resolve meta workspace: config overrides > auto-discovery via `meta` CLI
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let meta = discover_meta(&cwd).await;

    let workspace_root = dispatch_cfg
        .resolved_meta_workspace_root(config.config_dir.as_deref())
        .ok()
        .or_else(|| meta.as_ref().map(|m| m.workspace_root.clone()))
        .unwrap_or_else(|| cwd.clone());
    let kb_root = &workspace_root;

    let repo = match dispatch_cfg.resolved_repo() {
        Some(r) => Some(r.to_string()),
        None => meta.as_ref().map(|m| m.repo.clone()),
    };

    // 1. Resolve mode
    debug!(%slug, "resolving mode");
    let mode = resolve_mode(opts.cli_mode.clone(), slug, kb_root).await?;
    info!(%slug, mode = %mode.as_str(), "mode resolved");

    // Mode-specific validation
    if matches!(mode, Mode::ReviewFix | Mode::PrComments) && opts.pr_url.is_none() {
        anyhow::bail!(
            "{} mode requires a PR URL (--pr-url). Cannot dispatch without it.",
            mode.as_str()
        );
    }

    // Derive branch and dispatch ID
    let branch = derive_branch(slug);

    // Validate branch name
    validate_branch_name(&branch).await?;

    let dispatch_id = build_dispatch_id(&branch, &mode);
    let session_name = dispatch_id.clone();

    // Resolve per-directive budget/turns
    let mode_key = mode.as_str();
    let budget = opts
        .max_budget_override
        .or_else(|| config.modes.get(mode_key).and_then(|m| m.max_budget_usd))
        .unwrap_or(dispatch_cfg.max_budget_usd);
    let turns = opts
        .max_turns_override
        .or_else(|| config.modes.get(mode_key).and_then(|m| m.max_turns))
        .unwrap_or(dispatch_cfg.max_turns);

    // Duplicate session detection
    if !opts.force && tmux_session_alive(&session_name).await {
        anyhow::bail!(
            "tmux session '{}' already exists. Use --force to override.",
            session_name
        );
    }

    // Compute kb_basename once, with proper validation for both dry-run and real paths
    let kb_basename = workspace_root
        .file_name()
        .ok_or_else(|| {
            anyhow::anyhow!("workspace_root has no basename (is it the filesystem root?)")
        })?
        .to_string_lossy()
        .into_owned();

    // Dry-run: print config and exit
    if opts.dry_run {
        let dry_run_worktree = match repo.as_deref() {
            Some(r) => worktree_base.join(&kb_basename).join(r),
            None => worktree_base.join(&kb_basename).join(&branch),
        };
        println!("=== DRY RUN ===");
        println!("Task:        {}", slug);
        println!("Mode:        {}", mode.as_str());
        println!("Branch:      {}", branch);
        println!("ID:          {}", dispatch_id);
        println!("Repo:        {}", repo.as_deref().unwrap_or("(plain git)"));
        println!("Worktree:    {}", dry_run_worktree.display());
        println!("Budget:      ${:.2}", budget);
        println!("Turns:       {}", turns);
        println!(
            "PR URL:      {}",
            opts.pr_url.as_deref().unwrap_or("(none)")
        );
        return Ok(DispatchOutcome {
            id: dispatch_id,
            session: session_name,
            inline_exit_code: Some(0),
        });
    }

    // 2. CAS-claim the task (before worktree creation)
    cas_claim(slug, &session_name, kb_root).await?;

    let wt_opts = WorktreeOpts {
        worktree_base: &worktree_base,
        kb_basename: &kb_basename,
        repo: repo.as_deref(),
        branch: &branch,
        meta_workspace_root: &workspace_root,
        kb_root,
        force: opts.force,
    };
    let wt_result = match ensure_worktree(&wt_opts, registry).await {
        Ok(r) => r,
        Err(e) => {
            unassign_task(slug, kb_root).await;
            return Err(e);
        }
    };
    let wt_created = wt_result.created;
    let wt_is_meta = wt_result.is_meta;
    let worktree_path = wt_result.path;

    // 4. Resolve GH_TOKEN and agent env
    let mut env = HashMap::new();
    env.insert("GITKB_WORKSPACE".to_string(), branch.clone());
    env.insert(
        "GITKB_ROOT".to_string(),
        kb_root.to_string_lossy().into_owned(),
    );

    // GH_TOKEN resolution
    match resolve_gh_token().await {
        Ok(token) => {
            env.insert("GH_TOKEN".to_string(), token);
        }
        Err(e) => {
            warn!(error = %e, "could not resolve GH_TOKEN (non-fatal)");
        }
    }

    // AGENT_ALLOWED_PATHS — include GITKB_ROOT so git-kb reads/writes succeed under sandbox
    let allowed_paths =
        compute_allowed_paths(&worktree_path, &[kb_root.to_string_lossy().into_owned()]);
    env.insert("AGENT_ALLOWED_PATHS".to_string(), allowed_paths);

    // Unset CLAUDECODE in agent environment
    env.insert("CLAUDECODE".to_string(), String::new());

    // TODO(phase-1): Write .envrc to worktree with resolver env_overrides once
    // InputResolver provides per-task overrides.
    // let envrc_vars: HashMap<String, String> = HashMap::new();
    // if let Err(e) = write_envrc(&worktree_path, &envrc_vars).await {
    //     warn!(error = %e, "failed to write .envrc (non-fatal)");
    // }

    // 5. Setup log file
    if let Err(e) = tokio::fs::create_dir_all(&log_dir).await {
        rollback_claim_and_worktree(
            slug,
            kb_root,
            wt_created,
            wt_is_meta,
            &worktree_path,
            &workspace_root,
        )
        .await;
        return Err(e.into());
    }
    let log_file = log_dir.join(format!("{}.jsonl", dispatch_id));

    // Write diagnostic file
    let gh_token_present = env.contains_key("GH_TOKEN") && !env["GH_TOKEN"].is_empty();
    write_diag_file(&log_dir, &dispatch_id, gh_token_present).await;

    // Render system prompt from mode template + config overrides
    let directive = opts.directive.as_deref().unwrap_or("");
    let prompt = match atc_core::prompt_engine::render_prompt(
        &mode,
        slug,
        config,
        directive,
        Some(worktree_path.as_path()),
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            rollback_claim_and_worktree(
                slug,
                kb_root,
                wt_created,
                wt_is_meta,
                &worktree_path,
                &workspace_root,
            )
            .await;
            return Err(e);
        }
    };

    // 6. Build agent opts and spawn
    let agent_opts = AgentOpts {
        slug: slug.to_string(),
        worktree_path: worktree_path.clone(),
        prompt,
        mode: mode.clone(),
        log_file: log_file.clone(),
        env,
        session_name: session_name.clone(),
        dispatch_id: dispatch_id.clone(),
        sandbox: dispatch_cfg.sandbox,
        inline: opts.inline,
        max_turns: turns,
        max_budget_usd: budget,
    };

    let handle = match executor.spawn(&agent_opts).await {
        Ok(h) => h,
        Err(e) => {
            rollback_claim_and_worktree(
                slug,
                kb_root,
                wt_created,
                wt_is_meta,
                &worktree_path,
                &workspace_root,
            )
            .await;
            return Err(e);
        }
    };

    // 7. Insert registry record
    // For inline runs, the agent has already finished — record terminal status.
    let status = match handle.inline_exit_code {
        Some(0) => Status::Done,
        Some(_) => Status::Failed,
        None => Status::Running,
    };
    let now = Utc::now();
    let record = DispatchRecord {
        id: dispatch_id.clone(),
        task_slug: Some(slug.to_string()),
        branch: branch.clone(),
        worktree_path: worktree_path.clone(),
        session: handle.session.clone(),
        log_file: log_file.clone(),
        status,
        mode: mode.clone(),
        retries: opts.retries,
        resolver: "task".to_string(),
        pr_url: opts.pr_url.clone(),
        checks: HealthChecks::default(),
        cost_usd: None,
        num_turns: None,
        duration_ms: None,
        dispatched_at: now,
        updated_at: now,
    };
    if let Err(e) = registry.insert(&record).await {
        // Insert failed — there's a live tmux session with no durable record.
        // Kill it non-fatally so we don't leak resources.
        warn!(id = %dispatch_id, error = %e, "registry insert failed; killing orphan session");
        let _ = tokio::process::Command::new("tmux")
            .args(["kill-session", "-t", &handle.session])
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        unassign_task(slug, kb_root).await;
        return Err(e);
    }

    // "Agent starting" PR comment — posted after spawn + insert so the dispatch
    // is durable before we emit a non-idempotent GitHub write.
    if matches!(mode, Mode::ReviewFix | Mode::PrComments) {
        if let Some(ref url) = opts.pr_url {
            let comment = format!("\u{1f916} Agent starting: {} on {}", mode.as_str(), branch);
            post_pr_comment(url, &comment).await;
        }
    }

    let outcome = DispatchOutcome {
        id: dispatch_id.clone(),
        session: handle.session.clone(),
        inline_exit_code: handle.inline_exit_code,
    };

    // Post-dispatch confirmation
    print_dispatch_confirmation(
        Some(slug),
        &mode,
        &dispatch_id,
        &branch,
        &worktree_path,
        &handle.session,
        &log_file,
    );

    if let Some(exit_code) = handle.inline_exit_code {
        info!(
            %slug,
            session = %handle.session,
            exit_code,
            "dispatch complete (inline)"
        );
    } else {
        info!(
            %slug,
            session = %handle.session,
            "dispatch started (tmux)"
        );
    }

    Ok(outcome)
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
        // Third part is "timestamp-hexrand"
        let ts_rand: Vec<&str> = parts[2].split('-').collect();
        assert_eq!(
            ts_rand.len(),
            2,
            "expected ts-rand format, got: {}",
            parts[2]
        );
        let ts: i64 = ts_rand[0].parse().expect("timestamp should be a number");
        assert!(ts > 0);
        // Hex suffix should be 4 chars
        assert_eq!(ts_rand[1].len(), 4);
        u16::from_str_radix(ts_rand[1], 16).expect("suffix should be valid hex");
    }

    #[test]
    fn test_build_dispatch_id_uniqueness() {
        // Two IDs built in quick succession should differ (random suffix)
        let id1 = build_dispatch_id("tasks--foo", &Mode::Implement);
        let id2 = build_dispatch_id("tasks--foo", &Mode::Implement);
        // They *may* share a timestamp but the full ID should almost certainly differ
        // (same PID but nanos will have advanced)
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
