use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::executor::{AgentExecutor, AgentOpts};
use atc_core::registry::Registry;
use atc_core::types::{DispatchRecord, HealthChecks, Mode, Status};
use chrono::Utc;
use std::collections::HashMap;
use std::path::Path;
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

/// Build session name: `<slug-sanitized>@<mode>@<unix-ts>`.
pub fn build_session_name(slug: &str, mode: &Mode) -> String {
    let ts = Utc::now().timestamp();
    format!("{}@{}@{}", derive_branch(slug), mode.as_str(), ts)
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

/// Create a worktree via `meta git worktree create`.
#[tracing::instrument(skip(meta_workspace_root, kb_root))]
async fn create_worktree(
    worktree_base: &Path,
    kb_basename: &str,
    repo: &str,
    branch: &str,
    meta_workspace_root: &Path,
    kb_root: &Path,
) -> Result<std::path::PathBuf> {
    let output = tokio::process::Command::new("meta")
        .args([
            "git",
            "worktree",
            "create",
            kb_basename,
            "--repo",
            repo,
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

    // Worktree lands at: <worktree_base>/<kb_basename>/<repo>/
    Ok(worktree_base.join(kb_basename).join(repo))
}

/// Execute the full dispatch flow.
#[tracing::instrument(skip(config, registry, executor))]
pub async fn dispatch(
    config: &AtcConfig,
    registry: &dyn Registry,
    executor: &dyn AgentExecutor,
    cli_mode: Option<Mode>,
    slug: &str,
    inline: bool,
) -> Result<()> {
    let dispatch_cfg = &config.dispatch;

    // Resolve config paths
    let repo = dispatch_cfg.resolved_repo()?;
    let meta_workspace_root =
        dispatch_cfg.resolved_meta_workspace_root(config.config_dir.as_deref())?;
    let kb_root = &meta_workspace_root;
    let worktree_base = dispatch_cfg.resolved_worktree_base();
    let log_dir = dispatch_cfg.resolved_log_dir();

    // 1. Resolve mode
    debug!(slug, "resolving mode");
    let mode = resolve_mode(cli_mode, slug, kb_root).await?;
    info!(slug, mode = %mode.as_str(), "mode resolved");

    // Derive branch and session name
    let branch = derive_branch(slug);
    let session_name = build_session_name(slug, &mode);

    // 2. CAS-claim the task (before worktree creation)
    cas_claim(slug, &session_name, kb_root).await?;

    // 3. Create worktree (with unassign-on-failure)
    let kb_basename = meta_workspace_root
        .file_name()
        .ok_or_else(|| {
            anyhow::anyhow!("meta_workspace_root has no basename (is it the filesystem root?)")
        })?
        .to_string_lossy()
        .into_owned();

    let worktree_path = match create_worktree(
        &worktree_base,
        &kb_basename,
        repo,
        &branch,
        &meta_workspace_root,
        kb_root,
    )
    .await
    {
        Ok(path) => path,
        Err(e) => {
            unassign_task(slug, kb_root).await;
            return Err(e);
        }
    };

    // 4. Resolve agent env
    let mut env = HashMap::new();
    env.insert("GITKB_WORKSPACE".to_string(), branch.clone());
    env.insert(
        "GITKB_ROOT".to_string(),
        kb_root.to_string_lossy().into_owned(),
    );

    // 5. Setup log file
    tokio::fs::create_dir_all(&log_dir).await?;
    let log_file = log_dir.join(format!("{}.jsonl", session_name));

    // Render system prompt (placeholder — gitkb-268 provides template rendering)
    let prompt = String::new();

    // 6. Build agent opts and spawn
    let opts = AgentOpts {
        slug: slug.to_string(),
        worktree_path: worktree_path.clone(),
        prompt,
        mode: mode.clone(),
        log_file: log_file.clone(),
        env,
        session_name: session_name.clone(),
        sandbox: dispatch_cfg.sandbox,
        inline,
        max_turns: dispatch_cfg.max_turns,
        max_budget_usd: dispatch_cfg.max_budget_usd,
    };

    let handle = match executor.spawn(&opts).await {
        Ok(h) => h,
        Err(e) => {
            unassign_task(slug, kb_root).await;
            // Best-effort worktree cleanup; ignore errors
            let _ = tokio::process::Command::new("meta")
                .args([
                    "git",
                    "worktree",
                    "remove",
                    "--force",
                    &worktree_path.to_string_lossy(),
                ])
                .current_dir(&meta_workspace_root)
                .status()
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
        slug: slug.to_string(),
        branch,
        worktree_path,
        session: handle.session.clone(),
        log_file,
        status,
        mode,
        retries: 0,
        pr_url: None,
        checks: HealthChecks::default(),
        cost_usd: None,
        num_turns: None,
        duration_ms: None,
        dispatched_at: now,
        updated_at: now,
    };
    registry.insert(&record).await?;

    if let Some(exit_code) = handle.inline_exit_code {
        info!(
            slug,
            session = %handle.session,
            exit_code,
            "dispatch complete (inline)"
        );
    } else {
        info!(
            slug,
            session = %handle.session,
            "dispatch started (tmux)"
        );
    }

    Ok(())
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
        // Empty slug
        assert_eq!(derive_branch(""), "");
        // Trailing slash
        assert_eq!(derive_branch("tasks/"), "tasks--");
        // Leading slash
        assert_eq!(derive_branch("/tasks"), "--tasks");
        // Multiple consecutive slashes
        assert_eq!(derive_branch("a//b"), "a----b");
        // No slashes
        assert_eq!(derive_branch("no-slashes-here"), "no-slashes-here");
    }

    #[test]
    fn test_build_session_name_format() {
        let name = build_session_name("tasks/gitkb-42", &Mode::Implement);
        // Format: <slug-sanitized>@<mode>@<unix-ts>
        let parts: Vec<&str> = name.split('@').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "tasks--gitkb-42");
        assert_eq!(parts[1], "implement");
        // Third part should be a valid unix timestamp
        let ts: i64 = parts[2].parse().expect("timestamp should be a number");
        assert!(ts > 0);
    }

    #[test]
    fn test_build_session_name_different_modes() {
        let name = build_session_name("tasks/gitkb-264", &Mode::Research);
        assert!(name.starts_with("tasks--gitkb-264@research@"));

        let name = build_session_name("tasks/gitkb-264", &Mode::ReviewFix);
        assert!(name.starts_with("tasks--gitkb-264@review-fix@"));
    }
}
