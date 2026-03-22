//! Post-completion pipeline: artifact extraction, registry update, notifications,
//! worktree cleanup.
//!
//! Runs after an agent session completes — either automatically via the tmux
//! pipeline or manually via `atc post-complete`.

use crate::config::AtcConfig;
use crate::registry::Registry;
use crate::stream_json::{self, Artifacts};
use crate::types::{Mode, Status};
use anyhow::Result;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Input for the post-completion pipeline.
pub struct PostCompleteInput {
    pub dispatch_id: String,
    pub exit_code: Option<i32>,
    pub log_file: Option<PathBuf>,
}

/// Result of the post-completion pipeline.
#[derive(Debug)]
pub struct PostCompleteResult {
    pub status: Status,
    pub artifacts: Artifacts,
    pub pr_url: Option<String>,
}

/// Run the full post-completion pipeline for a dispatch.
///
/// 1. Resolve log file from registry if not provided
/// 2. Extract artifacts from stream-json log
/// 3. Determine status (done/failed)
/// 4. Update registry with cost, status, PR URL, artifacts
/// 5. Log cost threshold warning
/// 6. Save review artifact if applicable
/// 7. Send notifications
/// 8. Clean up worktree if PR merged/closed
pub async fn run_post_completion(
    input: &PostCompleteInput,
    registry: &dyn Registry,
    config: &AtcConfig,
) -> Result<PostCompleteResult> {
    // 1. Look up the dispatch record
    let record = registry
        .get(&input.dispatch_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("dispatch not found: {}", input.dispatch_id))?;

    // 2. Resolve log file path
    let log_file = input.log_file.as_deref().unwrap_or(&record.log_file);

    // 3. Extract artifacts from the log
    let artifacts = stream_json::extract_artifacts(log_file);

    // 4. Determine exit code — use provided value, or infer from result event
    let exit_code = input.exit_code.unwrap_or_else(|| {
        match &artifacts.result {
            Some(r) if r.subtype == "success" => 0,
            Some(_) => 1,
            None => 1, // no result event → assume failure
        }
    });

    // 5. Determine status
    let derived_status = if exit_code == 0
        && artifacts
            .result
            .as_ref()
            .is_some_and(|r| r.subtype == "success")
    {
        Status::Done
    } else {
        Status::Failed
    };

    // If the record is already in a terminal state (e.g. stopped by user, needs-human),
    // preserve that outcome — don't overwrite it with a derived done/failed.
    let status = if record.status.is_terminal() {
        info!(
            id = %input.dispatch_id,
            existing = %record.status,
            derived = %derived_status,
            "preserving existing terminal status"
        );
        record.status
    } else {
        derived_status
    };

    // 6. Update registry with cost/turns/duration
    if let Some(ref result) = artifacts.result {
        if let (Some(cost), Some(turns), Some(duration)) =
            (result.total_cost_usd, result.num_turns, result.duration_ms)
        {
            if let Err(e) = registry
                .update_cost(&input.dispatch_id, cost, turns, duration)
                .await
            {
                warn!(id = %input.dispatch_id, error = %e, "failed to update cost");
            }
        }
    }

    // 7. Update status (only transitions non-terminal → terminal)
    if !record.status.is_terminal() {
        registry.update_status(&input.dispatch_id, status).await?;
    }

    // 8. Store PR URL
    let pr_url = artifacts.pr_urls.first().cloned();
    if let Some(ref url) = pr_url {
        if let Err(e) = registry.set_pr_url(&input.dispatch_id, url).await {
            warn!(id = %input.dispatch_id, error = %e, "failed to set PR URL");
        }
    }

    // 9. Store artifacts as JSON blob (always store — artifacts are additive metadata)
    let json = serde_json::to_string(&artifacts)?;
    registry.set_artifacts(&input.dispatch_id, &json).await?;

    // 10. Cost threshold warning
    let threshold = config.watch.cost_threshold;
    if let Some(cost) = artifacts.result.as_ref().and_then(|r| r.total_cost_usd) {
        if cost > threshold {
            warn!(
                id = %input.dispatch_id,
                cost_usd = cost,
                "⚠ Dispatch {} cost ${:.2} (exceeds ${:.2} threshold)",
                input.dispatch_id,
                cost,
                threshold
            );
        }
    }

    // 11. Save review artifact if ReviewFix or PrComments mode
    if matches!(record.mode, Mode::ReviewFix | Mode::PrComments) {
        if let Some(log_dir) = log_file.parent() {
            if let Err(e) =
                save_review_artifact(log_dir, &input.dispatch_id, &artifacts, &record.mode)
            {
                warn!(id = %input.dispatch_id, error = %e, "failed to save review artifact");
            }
        }
    }

    // 12. Send notifications
    let task_label = record.task_slug.as_deref().unwrap_or(&input.dispatch_id);

    send_macos_notification(task_label, status, &pr_url, &artifacts, config);

    if let Ok(webhook_url) = std::env::var("ATC_NOTIFY_WEBHOOK") {
        if !webhook_url.is_empty() {
            send_webhook(&webhook_url, task_label, status, &artifacts, &pr_url);
        }
    } else if let Some(ref notif_cfg) = config.notifications {
        if let Some(ref url) = notif_cfg.webhook_url {
            if !url.is_empty() {
                send_webhook(url, task_label, status, &artifacts, &pr_url);
            }
        }
    }

    // 13. Worktree cleanup if PR merged/closed
    if let Some(ref url) = pr_url {
        let worktree_base = config.dispatch.resolved_worktree_base();
        cleanup_if_pr_done(url, &record.worktree_path, &worktree_base).await;
    }

    info!(
        id = %input.dispatch_id,
        status = %status,
        pr_url = ?pr_url,
        "post-completion finished"
    );

    Ok(PostCompleteResult {
        status,
        artifacts,
        pr_url,
    })
}

/// Save a structured JSON review artifact for cross-run continuity.
fn save_review_artifact(
    log_dir: &Path,
    dispatch_id: &str,
    artifacts: &Artifacts,
    _mode: &Mode,
) -> Result<()> {
    let head_commit = artifacts.commits.last().cloned().unwrap_or_default();

    let pr_url = artifacts.pr_urls.first().cloned().unwrap_or_default();

    // Extract comments_resolved and files_reviewed from artifacts
    // These would be parsed from tool_use blocks in a more complete implementation
    let review_artifact = serde_json::json!({
        "run_id": dispatch_id,
        "pr_url": pr_url,
        "head_commit": head_commit,
        "ended_at": chrono::Utc::now().to_rfc3339(),
        "comments_resolved": [],
        "files_reviewed": [],
        "summary": artifacts.summary.as_deref().unwrap_or(""),
    });

    let path = log_dir.join(format!("{dispatch_id}-review-artifact.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&review_artifact)?)?;
    info!(path = %path.display(), "saved review artifact");
    Ok(())
}

/// Send a macOS notification via osascript.
fn send_macos_notification(
    task_label: &str,
    status: Status,
    pr_url: &Option<String>,
    artifacts: &Artifacts,
    config: &AtcConfig,
) {
    // Check config — default to enabled
    if let Some(ref notif) = config.notifications {
        if !notif.macos {
            return;
        }
    }

    let status_icon = match status {
        Status::Done => "✅",
        Status::Failed => "❌",
        _ => "⚙️",
    };

    let title = format!("atc: {} {}", task_label, status_icon);

    let body = match status {
        Status::Done => {
            if let Some(ref url) = pr_url {
                let cost = artifacts
                    .result
                    .as_ref()
                    .and_then(|r| r.total_cost_usd)
                    .map(|c| format!("${c:.2}"))
                    .unwrap_or_else(|| "-".to_string());
                let duration = artifacts
                    .result
                    .as_ref()
                    .and_then(|r| r.duration_ms)
                    .map(|ms| format!("{}s", ms / 1000))
                    .unwrap_or_else(|| "-".to_string());
                format!("PR: {url} | {cost} | {duration}")
            } else {
                artifacts
                    .summary
                    .clone()
                    .unwrap_or_else(|| "Completed".to_string())
            }
        }
        Status::Failed => {
            let subtype = artifacts
                .result
                .as_ref()
                .map(|r| r.subtype.clone())
                .unwrap_or_else(|| "unknown".to_string());
            let cost = artifacts
                .result
                .as_ref()
                .and_then(|r| r.total_cost_usd)
                .map(|c| format!(" | ${c:.2}"))
                .unwrap_or_default();
            format!("{subtype}{cost}")
        }
        _ => "Status update".to_string(),
    };

    let script =
        "on run argv\n  display notification (item 1 of argv) with title (item 2 of argv)\nend run";
    let _ = std::process::Command::new("osascript")
        .args(["-e", script])
        .arg(&body)
        .arg(&title)
        .spawn(); // fire and forget
}

/// POST a webhook notification via curl.
fn send_webhook(
    webhook_url: &str,
    task_label: &str,
    status: Status,
    artifacts: &Artifacts,
    pr_url: &Option<String>,
) {
    let payload = serde_json::json!({
        "slug": task_label,
        "status": status.as_str(),
        "subtype": artifacts.result.as_ref().map(|r| r.subtype.as_str()).unwrap_or("unknown"),
        "cost_usd": artifacts.result.as_ref().and_then(|r| r.total_cost_usd),
        "num_turns": artifacts.result.as_ref().and_then(|r| r.num_turns),
        "duration_ms": artifacts.result.as_ref().and_then(|r| r.duration_ms),
        "artifacts": {
            "pr_url": pr_url,
            "summary": artifacts.summary,
        }
    });

    let payload_str = serde_json::to_string(&payload).unwrap_or_default();

    let _ = std::process::Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            &payload_str,
            webhook_url,
        ])
        .spawn(); // fire and forget
}

/// Check PR state and clean up worktree if merged or closed.
pub async fn cleanup_if_pr_done(pr_url: &str, worktree_path: &Path, worktree_base: &Path) {
    let state = match get_pr_state(pr_url).await {
        Some(s) => s,
        None => return,
    };

    if state == "MERGED" || state == "CLOSED" {
        cleanup_worktree(worktree_path, worktree_base).await;
    }
}

/// Query PR state via `gh pr view`.
async fn get_pr_state(pr_url: &str) -> Option<String> {
    let output = tokio::process::Command::new("gh")
        .args(["pr", "view", pr_url, "--json", "state", "-q", ".state"])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if state.is_empty() {
        None
    } else {
        Some(state)
    }
}

/// Remove a git worktree and its empty parent directory.
/// Safety: only removes if path matches known worktree patterns.
/// Uses a 30-second timeout to prevent a hung `git worktree remove` from
/// blocking the async pipeline indefinitely.
pub async fn cleanup_worktree(worktree_path: &Path, worktree_base: &Path) {
    // Canonicalize to resolve symlinks and ".." before safety checks
    let canonical = match worktree_path.canonicalize() {
        Ok(p) => p,
        Err(_) => return, // path doesn't exist, nothing to clean
    };
    let path_str = canonical.to_string_lossy();

    // Safety check: only remove paths under the configured worktree base or known patterns
    let is_safe = canonical.starts_with(worktree_base) || path_str.contains("/.worktrees/");

    if !is_safe {
        warn!(
            path = %path_str,
            "skipping worktree cleanup: path does not match safe patterns"
        );
        return;
    }

    // Find repo root by walking up from worktree path to find .git
    let repo_root = find_repo_root(&canonical);

    if let Some(root) = repo_root {
        let mut child = match tokio::process::Command::new("git")
            .args([
                "-C",
                &root.to_string_lossy(),
                "worktree",
                "remove",
                "--force",
                &path_str,
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                warn!(path = %path_str, error = %e, "failed to spawn git worktree remove");
                return;
            }
        };

        const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
        match tokio::time::timeout(TIMEOUT, child.wait()).await {
            Ok(Ok(exit_status)) if exit_status.success() => {
                info!(path = %path_str, "removed worktree");
                // Remove empty parent dir
                if let Some(parent) = canonical.parent() {
                    let _ = std::fs::remove_dir(parent); // only succeeds if empty
                }
            }
            Ok(Ok(_)) => {
                warn!(path = %path_str, "git worktree remove failed (non-zero exit)");
            }
            Ok(Err(e)) => {
                warn!(path = %path_str, error = %e, "git worktree remove I/O error");
            }
            Err(_) => {
                warn!(path = %path_str, "git worktree remove timed out after 30s, killing");
                let _ = child.kill().await;
            }
        }
    } else {
        warn!(path = %path_str, "skipping worktree cleanup: could not resolve repo root");
    }
}

/// Walk up from a worktree path to find the main repo root.
fn find_repo_root(worktree_path: &Path) -> Option<PathBuf> {
    // Read .git file in worktree to find the main repo
    let git_file = worktree_path.join(".git");
    if git_file.is_file() {
        if let Ok(content) = std::fs::read_to_string(&git_file) {
            // Format: "gitdir: /path/to/main/.git/worktrees/<name>"
            if let Some(gitdir) = content.strip_prefix("gitdir: ") {
                let gitdir = gitdir.trim();
                // Navigate up from .git/worktrees/<name> to the repo root
                let p = PathBuf::from(gitdir);
                if let Some(git_root) = p.parent().and_then(|p| p.parent()).and_then(|p| p.parent())
                {
                    return Some(git_root.to_path_buf());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream_json::{Artifacts, ResultEvent};

    #[test]
    fn test_status_determination_success() {
        // success + exit 0 → Done
        let artifacts = Artifacts {
            result: Some(ResultEvent {
                subtype: "success".to_string(),
                total_cost_usd: Some(1.0),
                num_turns: Some(5),
                duration_ms: Some(30000),
            }),
            ..Default::default()
        };
        let exit_code = 0;
        let status = if exit_code == 0
            && artifacts
                .result
                .as_ref()
                .is_some_and(|r| r.subtype == "success")
        {
            Status::Done
        } else {
            Status::Failed
        };
        assert_eq!(status, Status::Done);
    }

    #[test]
    fn test_status_determination_failure_exit_code() {
        let artifacts = Artifacts {
            result: Some(ResultEvent {
                subtype: "success".to_string(),
                total_cost_usd: Some(1.0),
                num_turns: Some(5),
                duration_ms: Some(30000),
            }),
            ..Default::default()
        };
        let exit_code = 1;
        let status = if exit_code == 0
            && artifacts
                .result
                .as_ref()
                .is_some_and(|r| r.subtype == "success")
        {
            Status::Done
        } else {
            Status::Failed
        };
        assert_eq!(status, Status::Failed);
    }

    #[test]
    fn test_status_determination_error_subtype() {
        let artifacts = Artifacts {
            result: Some(ResultEvent {
                subtype: "error_max_turns".to_string(),
                total_cost_usd: Some(5.0),
                num_turns: Some(100),
                duration_ms: Some(300000),
            }),
            ..Default::default()
        };
        let exit_code = 0;
        let status = if exit_code == 0
            && artifacts
                .result
                .as_ref()
                .is_some_and(|r| r.subtype == "success")
        {
            Status::Done
        } else {
            Status::Failed
        };
        assert_eq!(status, Status::Failed);
    }

    #[tokio::test]
    async fn test_cleanup_worktree_safety_check() {
        let base = Path::new("/tmp/worktrees");
        // Should not panic or remove a non-worktree path
        cleanup_worktree(Path::new("/usr/local/bin"), base).await;
        // Should not panic — path doesn't exist but matches pattern
        cleanup_worktree(Path::new("/tmp/worktrees/test/nonexistent"), base).await;
    }

    #[test]
    fn test_find_repo_root_no_git_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_repo_root(dir.path()).is_none());
    }

    #[test]
    fn test_save_review_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts = Artifacts {
            pr_urls: vec!["https://github.com/org/repo/pull/42".to_string()],
            commits: vec![
                "abc1234".to_string(),
                "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string(),
            ],
            summary: Some("Fixed the bug".to_string()),
            ..Default::default()
        };
        save_review_artifact(dir.path(), "test-dispatch", &artifacts, &Mode::ReviewFix).unwrap();

        let artifact_path = dir.path().join("test-dispatch-review-artifact.json");
        assert!(artifact_path.exists());

        let content: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&artifact_path).unwrap()).unwrap();
        assert_eq!(content["pr_url"], "https://github.com/org/repo/pull/42");
        // Should pick the last (most recent) commit SHA
        assert_eq!(
            content["head_commit"],
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
        );
        assert_eq!(content["summary"], "Fixed the bug");
    }
}
