use anyhow::Result;
use atc_core::config::{AtcConfig, DispatchConfig, ModeConfig};
use atc_core::executor::{AgentExecutor, AgentHandle, AgentOpts};
use atc_core::registry::{Registry, SqliteRegistry, StatusFilter};
use atc_core::types::{DispatchOpts, Mode, Status};
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

/// A stub executor that records the prompt it was called with and returns success.
struct RecordingExecutor {
    prompt: Mutex<Option<String>>,
}

impl RecordingExecutor {
    fn new() -> Self {
        Self {
            prompt: Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl AgentExecutor for RecordingExecutor {
    async fn spawn(&self, opts: &AgentOpts) -> Result<AgentHandle> {
        *self.prompt.lock().await = Some(opts.prompt.clone());

        if let Some(parent) = opts.log_file.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&opts.log_file, b"").ok();

        Ok(AgentHandle {
            session: opts.session_name.clone(),
            inline_exit_code: Some(0),
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
elif [ "$1" = "git" ] && [ "$2" = "worktree" ] && [ "$3" = "remove" ]; then
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

/// Create a stub `git` script that accepts check-ref-format and worktree list.
fn write_stub_git_bin(dir: &std::path::Path) {
    let script = dir.join("git");
    std::fs::write(
        &script,
        r#"#!/bin/bash
if [ "$1" = "check-ref-format" ]; then
    exit 0
elif [ "$1" = "worktree" ] && [ "$2" = "list" ]; then
    echo ""
    exit 0
fi
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    make_executable(&script);
}

/// Build a modes map with template_inline for all modes used in tests.
fn test_modes() -> HashMap<String, ModeConfig> {
    let mut modes = HashMap::new();
    for key in [
        "implement",
        "research",
        "review-fix",
        "kb-update",
        "pr-comments",
        "refine",
        "create-task",
        "close",
    ] {
        modes.insert(
            key.to_string(),
            ModeConfig {
                template_path: None,
                template_inline: Some(format!("Test prompt for {{{{slug}}}} mode {key}.")),
                max_budget_usd: None,
                max_turns: None,
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
            max_retries: 3,
        },
        modes: test_modes(),
        ..Default::default()
    }
}

/// Common test fixture: tempdir, bin_dir, worktree_base, PATH override, and config.
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

fn default_dispatch_opts(slug: &str, mode: Mode) -> DispatchOpts {
    DispatchOpts {
        slug: slug.to_string(),
        cli_mode: Some(mode),
        directive: None,
        pr_url: None,
        inline: true,
        force: false,
        dry_run: false,
        max_budget_override: None,
        max_turns_override: None,
        retries: 0,
    }
}

#[tokio::test]
async fn test_dispatch_inline_inserts_registry_record() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_script(&fix.bin_dir());
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(StubExecutor { exit_code: 0 });

    let opts = default_dispatch_opts("tasks/gitkb-42", Mode::Implement);
    let outcome =
        atc_cli::dispatch::dispatch(&fix.config, registry.as_ref(), executor.as_ref(), &opts)
            .await
            .expect("dispatch failed");

    assert_eq!(outcome.inline_exit_code, Some(0));

    // Record should be findable by ID
    let record = registry.get(&outcome.id).await.unwrap();
    assert!(record.is_some(), "registry record should exist");
    let record = record.unwrap();
    assert_eq!(record.task_slug.as_deref(), Some("tasks/gitkb-42"));
    assert_eq!(record.branch, "tasks--gitkb-42");
    assert_eq!(record.status, Status::Done);
    assert_eq!(record.mode, Mode::Implement);
    assert_eq!(record.resolver, "task");
    assert!(record.session.starts_with("tasks--gitkb-42@implement@"));
}

#[tokio::test]
async fn test_dispatch_cas_claim_failure_no_worktree() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_assign_fails(&fix.bin_dir());
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(StubExecutor { exit_code: 0 });

    let opts = default_dispatch_opts("tasks/gitkb-99", Mode::Implement);
    let result =
        atc_cli::dispatch::dispatch(&fix.config, registry.as_ref(), executor.as_ref(), &opts).await;

    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("already claimed"),
        "unexpected error: {}",
        err_msg
    );

    // No registry records should exist
    let all = registry.list(StatusFilter::All).await.unwrap();
    assert!(all.is_empty(), "no registry record after CAS failure");
}

#[tokio::test]
async fn test_dispatch_inline_failed_exit_code_produces_failed_status() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_script(&fix.bin_dir());
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(StubExecutor { exit_code: 1 });

    let opts = default_dispatch_opts("tasks/gitkb-fail", Mode::Implement);
    let outcome =
        atc_cli::dispatch::dispatch(&fix.config, registry.as_ref(), executor.as_ref(), &opts)
            .await
            .expect("dispatch should succeed even with non-zero exit");

    assert_eq!(outcome.inline_exit_code, Some(1));

    let record = registry.get(&outcome.id).await.unwrap();
    assert!(record.is_some(), "registry record should exist");
    let record = record.unwrap();
    assert_eq!(
        record.status,
        Status::Failed,
        "non-zero exit code should produce Failed status"
    );
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
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(StubExecutor { exit_code: 0 });

    // Pass None for mode — should resolve from frontmatter directives
    let opts = DispatchOpts {
        slug: "tasks/gitkb-auto-mode".to_string(),
        cli_mode: None,
        directive: None,
        pr_url: None,
        inline: true,
        force: false,
        dry_run: false,
        max_budget_override: None,
        max_turns_override: None,
        retries: 0,
    };
    let outcome =
        atc_cli::dispatch::dispatch(&fix.config, registry.as_ref(), executor.as_ref(), &opts)
            .await
            .expect("dispatch with mode from frontmatter failed");

    assert_eq!(outcome.inline_exit_code, Some(0));

    let record = registry.get(&outcome.id).await.unwrap().unwrap();
    assert_eq!(
        record.mode,
        Mode::Research,
        "mode should be resolved from frontmatter directives"
    );
}

#[tokio::test]
async fn test_dispatch_multiple_dispatches_same_task() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_script(&fix.bin_dir());
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(StubExecutor { exit_code: 0 });

    // First dispatch
    let opts = default_dispatch_opts("tasks/gitkb-dup", Mode::Implement);
    let outcome1 =
        atc_cli::dispatch::dispatch(&fix.config, registry.as_ref(), executor.as_ref(), &opts)
            .await
            .expect("first dispatch failed");

    // Small delay to ensure different millisecond timestamp
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Second dispatch of same slug should succeed (different dispatch ID)
    let outcome2 =
        atc_cli::dispatch::dispatch(&fix.config, registry.as_ref(), executor.as_ref(), &opts)
            .await
            .expect("second dispatch should succeed with new ID");

    assert_ne!(outcome1.id, outcome2.id, "dispatch IDs should differ");

    // Both records should exist
    let all = registry.list(StatusFilter::All).await.unwrap();
    assert_eq!(all.len(), 2, "both dispatch records should exist");

    // find_by_task_slug should return both
    let by_task = registry.find_by_task_slug("tasks/gitkb-dup").await.unwrap();
    assert_eq!(by_task.len(), 2);
}

#[tokio::test]
async fn test_dispatch_executor_failure_triggers_cleanup() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_script(&fix.bin_dir());
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(FailingExecutor);

    let opts = default_dispatch_opts("tasks/gitkb-exec-fail", Mode::Implement);
    let result =
        atc_cli::dispatch::dispatch(&fix.config, registry.as_ref(), executor.as_ref(), &opts).await;

    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("simulated error"),
        "unexpected error: {}",
        err_msg
    );

    // No registry record should exist
    let all = registry.list(StatusFilter::All).await.unwrap();
    assert!(all.is_empty(), "no registry record after executor failure");
}

#[tokio::test]
async fn test_dispatch_directive_survives_into_rendered_prompt() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_script(&fix.bin_dir());
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(RecordingExecutor::new());

    let opts = DispatchOpts {
        slug: "tasks/gitkb-directive".to_string(),
        cli_mode: Some(Mode::Implement),
        directive: Some("focus on error handling".to_string()),
        pr_url: None,
        inline: true,
        force: false,
        dry_run: false,
        max_budget_override: None,
        max_turns_override: None,
        retries: 0,
    };
    let outcome =
        atc_cli::dispatch::dispatch(&fix.config, registry.as_ref(), executor.as_ref(), &opts)
            .await
            .expect("dispatch failed");

    assert_eq!(outcome.inline_exit_code, Some(0));

    let prompt = executor.prompt.lock().await;
    let prompt = prompt
        .as_ref()
        .expect("executor should have recorded a prompt");
    assert!(
        prompt.contains("focus on error handling"),
        "directive should survive into rendered prompt, got: {prompt}"
    );
}

#[tokio::test]
async fn test_dispatch_review_fix_requires_pr_url() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_script(&fix.bin_dir());
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(StubExecutor { exit_code: 0 });

    let opts = DispatchOpts {
        slug: "tasks/gitkb-review".to_string(),
        cli_mode: Some(Mode::ReviewFix),
        directive: None,
        pr_url: None, // Missing!
        inline: true,
        force: false,
        dry_run: false,
        max_budget_override: None,
        max_turns_override: None,
        retries: 0,
    };
    let result =
        atc_cli::dispatch::dispatch(&fix.config, registry.as_ref(), executor.as_ref(), &opts).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("requires a PR URL"),
        "expected PR URL error, got: {err}"
    );
}

#[tokio::test]
async fn test_dispatch_dry_run() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_script(&fix.bin_dir());
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(StubExecutor { exit_code: 0 });

    let opts = DispatchOpts {
        slug: "tasks/gitkb-dry".to_string(),
        cli_mode: Some(Mode::Implement),
        directive: None,
        pr_url: None,
        inline: true,
        force: false,
        dry_run: true,
        max_budget_override: None,
        max_turns_override: None,
        retries: 0,
    };
    let outcome =
        atc_cli::dispatch::dispatch(&fix.config, registry.as_ref(), executor.as_ref(), &opts)
            .await
            .expect("dry run should succeed");

    assert_eq!(outcome.inline_exit_code, Some(0));

    // No registry record should exist (dry run doesn't dispatch)
    let all = registry.list(StatusFilter::All).await.unwrap();
    assert!(all.is_empty(), "dry run should not create registry records");
}
