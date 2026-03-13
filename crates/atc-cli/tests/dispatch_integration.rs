use anyhow::Result;
use atc_core::config::{AtcConfig, DispatchConfig, ModeConfig};
use atc_core::executor::{AgentExecutor, AgentHandle, AgentOpts};
use atc_core::registry::{Registry, SqliteRegistry};
use atc_core::types::{Mode, Status};
use std::collections::HashMap;
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

/// A stub executor that always returns an error.
struct FailingExecutor;

#[async_trait::async_trait]
impl AgentExecutor for FailingExecutor {
    async fn spawn(&self, _opts: &AgentOpts) -> Result<AgentHandle> {
        anyhow::bail!("executor spawn failed: simulated error")
    }
}

/// Make a file executable (unix only).
#[cfg(unix)]
fn make_executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Create a stub `git-kb` script that succeeds on `assign` and returns a task doc on `show`.
fn write_stub_git_script(dir: &std::path::Path) {
    let script = dir.join("git-kb");
    std::fs::write(
        &script,
        r#"#!/bin/bash
if [ "$1" = "assign" ]; then
    exit 0
elif [ "$1" = "unassign" ]; then
    exit 0
elif [ "$1" = "show" ]; then
    echo "---"
    echo "slug: $3"
    echo "title: Test task"
    echo "type: task"
    echo "status: active"
    echo "directives: [implement]"
    echo "---"
    echo ""
    echo "Test task body."
    exit 0
fi
exit 1
"#,
    )
    .unwrap();
    #[cfg(unix)]
    make_executable(&script);
}

/// Create a stub `git-kb` script where `assign` fails (already claimed).
fn write_stub_git_assign_fails(dir: &std::path::Path) {
    let script = dir.join("git-kb");
    std::fs::write(
        &script,
        r#"#!/bin/bash
if [ "$1" = "assign" ]; then
    echo "error: task already assigned" >&2
    exit 1
fi
exit 1
"#,
    )
    .unwrap();
    #[cfg(unix)]
    make_executable(&script);
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
    make_executable(&script);
}

/// Build a modes map with template_inline for all modes used in tests.
fn test_modes() -> HashMap<String, ModeConfig> {
    let mut modes = HashMap::new();
    for key in ["implement", "research", "review-fix", "kb-update", "pr-comments", "refine", "create-task"] {
        modes.insert(
            key.to_string(),
            ModeConfig {
                template_path: None,
                template_inline: Some(format!("Test prompt for {{{{slug}}}} mode {key}.")),
            },
        );
    }
    modes
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
        modes: test_modes(),
        ..Default::default()
    }
}

/// Common test fixture: tempdir, bin_dir, worktree_base, PATH override, and config.
/// Returns (tmp, original_path, config, bin_dir, worktree_base).
struct TestFixture {
    tmp: tempfile::TempDir,
    original_path: String,
    config: AtcConfig,
}

impl TestFixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let worktree_base = tmp.path().join("worktrees");
        std::fs::create_dir_all(&worktree_base).unwrap();

        let original_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{}", bin_dir.display(), original_path));

        let config = make_config(tmp.path(), &worktree_base, &bin_dir);
        Self {
            tmp,
            original_path,
            config,
        }
    }

    fn bin_dir(&self) -> std::path::PathBuf {
        self.tmp.path().join("bin")
    }

    fn worktree_base(&self) -> std::path::PathBuf {
        self.tmp.path().join("worktrees")
    }
}

impl Drop for TestFixture {
    fn drop(&mut self) {
        std::env::set_var("PATH", &self.original_path);
    }
}

#[tokio::test]
async fn test_dispatch_inline_inserts_registry_record() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_script(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(StubExecutor { exit_code: 0 });

    let result = atc_cli::dispatch::dispatch(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        Some(Mode::Implement),
        "tasks/gitkb-42",
        None,
        true,
    )
    .await;

    assert!(result.is_ok(), "dispatch failed: {:?}", result.err());

    let record = registry.get("tasks/gitkb-42").await.unwrap();
    assert!(record.is_some(), "registry record should exist");
    let record = record.unwrap();
    assert_eq!(record.slug, "tasks/gitkb-42");
    assert_eq!(record.branch, "tasks--gitkb-42");
    assert_eq!(record.status, Status::Done);
    assert_eq!(record.mode, Mode::Implement);
    assert!(record.session.starts_with("tasks--gitkb-42@implement@"));
    assert!(record.log_file.to_string_lossy().ends_with(".jsonl"));
    assert_eq!(record.retries, 0);
}

#[tokio::test]
async fn test_dispatch_cas_claim_failure_no_worktree() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_assign_fails(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(StubExecutor { exit_code: 0 });

    let result = atc_cli::dispatch::dispatch(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        Some(Mode::Implement),
        "tasks/gitkb-99",
        None,
        true,
    )
    .await;

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

#[tokio::test]
async fn test_dispatch_inline_failed_exit_code_produces_failed_status() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_script(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(StubExecutor { exit_code: 1 });

    let result = atc_cli::dispatch::dispatch(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        Some(Mode::Implement),
        "tasks/gitkb-fail",
        None,
        true,
    )
    .await;

    assert!(
        result.is_ok(),
        "dispatch should succeed even with non-zero exit: {:?}",
        result.err()
    );

    let record = registry.get("tasks/gitkb-fail").await.unwrap();
    assert!(record.is_some(), "registry record should exist");
    let record = record.unwrap();
    assert_eq!(
        record.status,
        Status::Failed,
        "non-zero exit code should produce Failed status"
    );
    assert_eq!(record.slug, "tasks/gitkb-fail");
}

/// Create a stub `git-kb` that returns JSON with directives for mode resolution.
fn write_stub_git_show_json(dir: &std::path::Path) {
    let script = dir.join("git-kb");
    std::fs::write(
        &script,
        r#"#!/bin/bash
if [ "$1" = "assign" ]; then
    exit 0
elif [ "$1" = "unassign" ]; then
    exit 0
elif [ "$1" = "show" ]; then
    if [ "$2" = "--json" ]; then
        echo '{"slug":"'"$3"'","title":"Test","directives":["research"]}'
        exit 0
    fi
    echo "---"
    echo "slug: $3"
    echo "title: Test task"
    echo "type: task"
    echo "status: active"
    echo "directives: [research]"
    echo "---"
    echo ""
    echo "Test task body."
    exit 0
fi
exit 1
"#,
    )
    .unwrap();
    #[cfg(unix)]
    make_executable(&script);
}

#[tokio::test]
async fn test_dispatch_resolves_mode_from_frontmatter() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_show_json(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(StubExecutor { exit_code: 0 });

    // Pass None for mode — should resolve from frontmatter directives
    let result = atc_cli::dispatch::dispatch(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        None,
        "tasks/gitkb-auto-mode",
        None,
        true,
    )
    .await;

    assert!(
        result.is_ok(),
        "dispatch with mode from frontmatter failed: {:?}",
        result.err()
    );

    let record = registry.get("tasks/gitkb-auto-mode").await.unwrap();
    assert!(record.is_some(), "registry record should exist");
    let record = record.unwrap();
    assert_eq!(
        record.mode,
        Mode::Research,
        "mode should be resolved from frontmatter directives"
    );
    assert!(
        record.session.contains("@research@"),
        "session name should contain resolved mode"
    );
}

#[tokio::test]
async fn test_dispatch_duplicate_slug_fails_unique_constraint() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_script(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(StubExecutor { exit_code: 0 });

    // First dispatch should succeed
    let result = atc_cli::dispatch::dispatch(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        Some(Mode::Implement),
        "tasks/gitkb-dup",
        None,
        true,
    )
    .await;
    assert!(result.is_ok(), "first dispatch failed: {:?}", result.err());

    // Second dispatch of same slug should fail on UNIQUE constraint
    let result = atc_cli::dispatch::dispatch(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        Some(Mode::Implement),
        "tasks/gitkb-dup",
        None,
        true,
    )
    .await;
    assert!(result.is_err(), "duplicate dispatch should fail");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("UNIQUE constraint failed"),
        "expected UNIQUE constraint error, got: {}",
        err_msg
    );
}

#[tokio::test]
async fn test_dispatch_executor_failure_triggers_cleanup() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_script(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(FailingExecutor);

    let result = atc_cli::dispatch::dispatch(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        Some(Mode::Implement),
        "tasks/gitkb-exec-fail",
        None,
        true,
    )
    .await;

    // Should propagate the executor error
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("simulated error"),
        "unexpected error: {}",
        err_msg
    );

    // No registry record should exist (dispatch didn't complete)
    let record = registry.get("tasks/gitkb-exec-fail").await.unwrap();
    assert!(
        record.is_none(),
        "no registry record after executor failure"
    );
}
