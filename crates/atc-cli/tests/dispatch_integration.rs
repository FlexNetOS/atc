use anyhow::Result;
use atc_core::config::{AtcConfig, DispatchConfig};
use atc_core::executor::{AgentExecutor, AgentHandle, AgentOpts};
use atc_core::registry::{Registry, SqliteRegistry};
use atc_core::types::{Mode, Status};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Guards PATH manipulation so integration tests don't race.
static PATH_MUTEX: std::sync::LazyLock<Mutex<()>> = std::sync::LazyLock::new(|| Mutex::new(()));

/// A stub executor that records the opts it was called with and returns success.
struct StubExecutor {
    exit_code: i32,
}

#[async_trait::async_trait]
impl AgentExecutor for StubExecutor {
    async fn spawn(&self, opts: &AgentOpts) -> Result<AgentHandle> {
        if let Some(parent) = opts.log_file.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&opts.log_file, b"").ok();

        Ok(AgentHandle {
            session: opts.session_name.clone(),
            inline_exit_code: Some(self.exit_code),
        })
    }
}

/// Create a stub `git` script that succeeds on `kb assign` and returns a task doc on `kb show`.
fn write_stub_git_script(dir: &std::path::Path) {
    let script = dir.join("git");
    std::fs::write(
        &script,
        r#"#!/bin/bash
if [ "$1" = "kb" ] && [ "$2" = "assign" ]; then
    exit 0
elif [ "$1" = "kb" ] && [ "$2" = "unassign" ]; then
    exit 0
elif [ "$1" = "kb" ] && [ "$2" = "show" ]; then
    echo "---"
    echo "slug: $4"
    echo "title: Test task"
    echo "type: task"
    echo "status: active"
    echo "directives: [implement]"
    echo "---"
    echo ""
    echo "Test task body."
    exit 0
fi
exec /usr/bin/git "$@"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Create a stub `git` script where `kb assign` fails (already claimed).
fn write_stub_git_assign_fails(dir: &std::path::Path) {
    let script = dir.join("git");
    std::fs::write(
        &script,
        r#"#!/bin/bash
if [ "$1" = "kb" ] && [ "$2" = "assign" ]; then
    echo "error: task already assigned" >&2
    exit 1
fi
exec /usr/bin/git "$@"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Create a stub `meta` script that simulates worktree creation by creating the directory.
fn write_stub_meta_script(dir: &std::path::Path, worktree_base: &std::path::Path) {
    let script = dir.join("meta");
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/bash
if [ "$1" = "git" ] && [ "$2" = "worktree" ] && [ "$3" = "create" ]; then
    KB_BASENAME="$4"
    REPO=""
    shift 4
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --repo) REPO="$2"; shift 2;;
            --branch) shift 2;;
            *) shift;;
        esac
    done
    mkdir -p "{worktree_base}/$KB_BASENAME/$REPO"
    exit 0
fi
exit 1
"#,
            worktree_base = worktree_base.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn make_config(
    tmp: &std::path::Path,
    worktree_base: &std::path::Path,
    bin_dir: &std::path::Path,
) -> AtcConfig {
    AtcConfig {
        config_dir: Some(tmp.to_path_buf()),
        dispatch: DispatchConfig {
            repo: Some("core".to_string()),
            worktree_base: Some(worktree_base.to_path_buf()),
            meta_workspace_root: Some(tmp.to_path_buf()),
            log_dir: Some(tmp.join("logs")),
            claude_bin: Some(bin_dir.join("claude")),
            sandbox: false,
            max_turns: 10_000,
            max_budget_usd: 25.0,
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn test_dispatch_inline_inserts_registry_record() {
    let _guard = PATH_MUTEX.lock().await;

    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();

    let worktree_base = tmp.path().join("worktrees");
    std::fs::create_dir_all(&worktree_base).unwrap();

    write_stub_git_script(&bin_dir);
    write_stub_meta_script(&bin_dir, &worktree_base);

    let original_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{}", bin_dir.display(), original_path));

    let config = make_config(tmp.path(), &worktree_base, &bin_dir);
    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(StubExecutor { exit_code: 0 });

    let result = atc_cli::dispatch::dispatch(
        &config,
        registry.as_ref(),
        executor.as_ref(),
        Some(Mode::Implement),
        "tasks/gitkb-42",
        true,
    )
    .await;

    std::env::set_var("PATH", &original_path);

    assert!(result.is_ok(), "dispatch failed: {:?}", result.err());

    let record = registry.get("tasks/gitkb-42").await.unwrap();
    assert!(record.is_some(), "registry record should exist");
    let record = record.unwrap();
    assert_eq!(record.slug, "tasks/gitkb-42");
    assert_eq!(record.branch, "tasks-gitkb-42");
    assert_eq!(record.status, Status::Running);
    assert_eq!(record.mode, Mode::Implement);
    assert!(record.session.starts_with("tasks-gitkb-42@implement@"));
    assert!(record.log_file.to_string_lossy().ends_with(".jsonl"));
    assert_eq!(record.retries, 0);
}

#[tokio::test]
async fn test_dispatch_cas_claim_failure_no_worktree() {
    let _guard = PATH_MUTEX.lock().await;

    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();

    let worktree_base = tmp.path().join("worktrees");
    std::fs::create_dir_all(&worktree_base).unwrap();

    write_stub_git_assign_fails(&bin_dir);
    write_stub_meta_script(&bin_dir, &worktree_base);

    let original_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{}", bin_dir.display(), original_path));

    let config = make_config(tmp.path(), &worktree_base, &bin_dir);
    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(StubExecutor { exit_code: 0 });

    let result = atc_cli::dispatch::dispatch(
        &config,
        registry.as_ref(),
        executor.as_ref(),
        Some(Mode::Implement),
        "tasks/gitkb-99",
        true,
    )
    .await;

    std::env::set_var("PATH", &original_path);

    // Should fail with CAS claim error
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("already claimed"),
        "unexpected error: {}",
        err_msg
    );

    // No registry record should exist
    let record = registry.get("tasks/gitkb-99").await.unwrap();
    assert!(record.is_none(), "no registry record after CAS failure");
}
