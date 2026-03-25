use anyhow::Result;
use atc_core::config::{AtcConfig, DirectiveConfig, DispatchConfig};
use atc_core::executor::{AgentExecutor, AgentHandle, AgentOpts};
use atc_core::registry::{Registry, SqliteRegistry, StatusFilter};
use atc_core::types::{Directive, RunOpts, Status};
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
    if [ "$2" = "--json" ]; then
        echo '{"slug":"'"$3"'","title":"Test","directives":["implement"]}'
        exit 0
    fi
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
elif [ "$1" = "show" ]; then
    if [ "$2" = "--json" ]; then
        echo '{"slug":"'"$3"'","title":"Test","directives":["implement"]}'
        exit 0
    fi
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

/// Build a directives map with template_inline for all directives used in tests.
fn test_directives() -> HashMap<String, DirectiveConfig> {
    let mut directives = HashMap::new();
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
        directives.insert(
            key.to_string(),
            DirectiveConfig {
                template_inline: Some(format!("Test prompt for {{{{slug}}}} directive {key}.")),
                ..Default::default()
            },
        );
    }
    directives
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
            project_env: true,
        },
        directives: test_directives(),
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

fn default_run_opts(input: &str, directive: Directive) -> RunOpts {
    RunOpts {
        input: input.to_string(),
        directive: Some(directive),
        params: HashMap::new(),
        pr_url: None,
        inline: true,
        force: false,
        dry_run: false,
        directives: None,
        no_worktree: false,
        max_budget_usd: None,
        max_turns: None,
        retries: 0,
        list: false,
    }
}

/// Helper to create a pipeline and dispatch via it.
async fn dispatch_via_pipeline(
    config: &AtcConfig,
    registry: &dyn Registry,
    executor: &dyn AgentExecutor,
    input: &str,
    opts: &RunOpts,
) -> Result<atc_core::types::DispatchOutcome> {
    let resolvers = atc_cli::resolvers::build_resolvers(config);
    let pipeline = atc_cli::pipeline::DispatchPipeline {
        resolvers,
        config,
        registry,
        executor,
    };
    pipeline.execute(input, opts).await
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

    let opts = default_run_opts("tasks/gitkb-42", Directive::Implement);
    let outcome = dispatch_via_pipeline(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        "tasks/gitkb-42",
        &opts,
    )
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
    assert_eq!(record.directive, Directive::Implement);
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

    let opts = default_run_opts("tasks/gitkb-99", Directive::Implement);
    let result = dispatch_via_pipeline(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        "tasks/gitkb-99",
        &opts,
    )
    .await;

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

    let opts = default_run_opts("tasks/gitkb-fail", Directive::Implement);
    let outcome = dispatch_via_pipeline(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        "tasks/gitkb-fail",
        &opts,
    )
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

/// Create a stub `git-kb` that returns JSON with directives for directive resolution.
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

    // Pass None for directive — should resolve from frontmatter directives
    let opts = RunOpts {
        input: "tasks/gitkb-auto-mode".to_string(),
        directive: None,
        params: HashMap::new(),
        pr_url: None,
        inline: true,
        force: false,
        dry_run: false,
        directives: None,
        no_worktree: false,
        max_budget_usd: None,
        max_turns: None,
        retries: 0,
        list: false,
    };
    let outcome = dispatch_via_pipeline(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        "tasks/gitkb-auto-mode",
        &opts,
    )
    .await
    .expect("dispatch with directive from frontmatter failed");

    assert_eq!(outcome.inline_exit_code, Some(0));

    let record = registry.get(&outcome.id).await.unwrap().unwrap();
    assert_eq!(
        record.directive,
        Directive::Research,
        "directive should be resolved from frontmatter directives"
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

    let opts = default_run_opts("tasks/gitkb-dup", Directive::Implement);

    // First dispatch
    let outcome1 = dispatch_via_pipeline(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        "tasks/gitkb-dup",
        &opts,
    )
    .await
    .expect("first dispatch failed");

    // Small delay to ensure different millisecond timestamp
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Second dispatch of same slug should succeed (different dispatch ID)
    let outcome2 = dispatch_via_pipeline(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        "tasks/gitkb-dup",
        &opts,
    )
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

    let opts = default_run_opts("tasks/gitkb-exec-fail", Directive::Implement);
    let result = dispatch_via_pipeline(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        "tasks/gitkb-exec-fail",
        &opts,
    )
    .await;

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

    let opts = RunOpts {
        input: "tasks/gitkb-directive".to_string(),
        directive: Some(Directive::Implement),
        params: HashMap::new(),
        pr_url: None,
        inline: true,
        force: false,
        dry_run: false,
        directives: Some("focus on error handling".to_string()),
        no_worktree: false,
        max_budget_usd: None,
        max_turns: None,
        retries: 0,
        list: false,
    };
    let outcome = dispatch_via_pipeline(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        "tasks/gitkb-directive",
        &opts,
    )
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

    let opts = RunOpts {
        input: "tasks/gitkb-review".to_string(),
        directive: Some(Directive::ReviewFix),
        params: HashMap::new(),
        pr_url: None, // Missing!
        inline: true,
        force: false,
        dry_run: false,
        directives: None,
        no_worktree: false,
        max_budget_usd: None,
        max_turns: None,
        retries: 0,
        list: false,
    };
    let result = dispatch_via_pipeline(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        "tasks/gitkb-review",
        &opts,
    )
    .await;
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

    let opts = RunOpts {
        input: "tasks/gitkb-dry".to_string(),
        directive: Some(Directive::Implement),
        params: HashMap::new(),
        pr_url: None,
        inline: true,
        force: false,
        dry_run: true,
        directives: None,
        no_worktree: false,
        max_budget_usd: None,
        max_turns: None,
        retries: 0,
        list: false,
    };
    let outcome = dispatch_via_pipeline(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        "tasks/gitkb-dry",
        &opts,
    )
    .await
    .expect("dry run should succeed");

    assert_eq!(outcome.inline_exit_code, Some(0));

    // No registry record should exist (dry run doesn't dispatch)
    let all = registry.list(StatusFilter::All).await.unwrap();
    assert!(all.is_empty(), "dry run should not create registry records");
}

// --- Resolver-specific tests ---

#[tokio::test]
async fn test_prompt_resolver_dispatch() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(StubExecutor { exit_code: 0 });

    // Use a raw prompt string that won't match any task or template
    let opts = RunOpts {
        input: "Fix the auth bug in login.rs".to_string(),
        directive: Some(Directive::Implement),
        params: HashMap::new(),
        pr_url: None,
        inline: true,
        force: false,
        dry_run: false,
        directives: None,
        no_worktree: false,
        max_budget_usd: None,
        max_turns: None,
        retries: 0,
        list: false,
    };

    // Only use prompt resolver (no task resolver since git-kb not configured)
    let mut config = fix.config.clone();
    config.resolvers.task.enabled = false;
    config.resolvers.template.enabled = false;

    let outcome = dispatch_via_pipeline(
        &config,
        registry.as_ref(),
        executor.as_ref(),
        "Fix the auth bug in login.rs",
        &opts,
    )
    .await
    .expect("prompt dispatch failed");

    assert_eq!(outcome.inline_exit_code, Some(0));

    let record = registry.get(&outcome.id).await.unwrap().unwrap();
    assert_eq!(record.resolver, "prompt");
    assert!(record.task_slug.is_none());
    assert_eq!(record.directive, Directive::Implement);
}

#[tokio::test]
async fn test_template_resolver_dispatch() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    // Create a template file
    let tmpl_dir = fix.tmp.path().join("templates");
    std::fs::create_dir_all(&tmpl_dir).unwrap();
    std::fs::write(
        tmpl_dir.join("my-review.md"),
        "---\ndirectives: [review-fix]\n---\nReview template body.",
    )
    .unwrap();

    let partials_dir = fix.tmp.path().join("partials");
    std::fs::create_dir_all(&partials_dir).unwrap();
    let comp_dir = fix.tmp.path().join("components");
    std::fs::create_dir_all(&comp_dir).unwrap();

    let mut config = fix.config.clone();
    config.prompt.templates_dir = "templates".to_string();
    config.prompt.partials_dir = "partials".to_string();
    config.prompt.components_dir = "components".to_string();
    config.resolvers.task.enabled = false; // Skip task resolver

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(StubExecutor { exit_code: 0 });

    let opts = RunOpts {
        input: "my-review".to_string(),
        directive: None,
        params: HashMap::new(),
        pr_url: Some("https://github.com/org/repo/pull/1".to_string()),
        inline: true,
        force: false,
        dry_run: false,
        directives: None,
        no_worktree: false,
        max_budget_usd: None,
        max_turns: None,
        retries: 0,
        list: false,
    };

    let outcome = dispatch_via_pipeline(
        &config,
        registry.as_ref(),
        executor.as_ref(),
        "my-review",
        &opts,
    )
    .await
    .expect("template dispatch failed");

    assert_eq!(outcome.inline_exit_code, Some(0));

    let record = registry.get(&outcome.id).await.unwrap().unwrap();
    assert_eq!(record.resolver, "template");
    assert!(record.task_slug.is_none());
    assert_eq!(record.directive, Directive::ReviewFix);
}
