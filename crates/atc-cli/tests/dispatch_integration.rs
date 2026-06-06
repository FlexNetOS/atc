use anyhow::Result;
use atc_core::config::{AtcConfig, DirectiveConfig, DispatchConfig};
use atc_core::executor::{AgentExecutor, AgentHandle, AgentInvocation, AgentOpts};
use atc_core::registry::{Registry, SqliteRegistry, StatusFilter};
use atc_core::types::{
    claude_agent_capabilities, AgentCapabilities, AgentSessionId, Directive, DispatchRecord,
    HealthChecks, RunOpts, Status, TerminalLocator, TerminalLocatorConfidence,
    TerminalLocatorSource, CLAUDE_AGENT_PROVIDER,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

/// Guards PATH manipulation so integration tests don't race.
static PATH_MUTEX: std::sync::LazyLock<Mutex<()>> = std::sync::LazyLock::new(|| Mutex::new(()));

/// A stub executor that records the opts it was called with and returns success.
struct StubExecutor {
    exit_code: i32,
}

#[async_trait::async_trait]
impl AgentExecutor for StubExecutor {
    async fn spawn(&self, opts: &AgentOpts) -> Result<AgentHandle> {
        if let Some(ref log_file) = opts.log_file {
            if let Some(parent) = log_file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(log_file, b"")?;
        }

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

        if let Some(ref log_file) = opts.log_file {
            if let Some(parent) = log_file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(log_file, b"")?;
        }

        Ok(AgentHandle {
            session: opts.session_name.clone(),
            inline_exit_code: Some(0),
        })
    }
}

#[derive(Debug, Clone)]
struct CapturedSpawn {
    worktree_path: PathBuf,
    agent_invocation: AgentInvocation,
    directive: Directive,
    stdin_content: Option<String>,
    max_turns: u32,
    max_budget_usd: f64,
}

/// A stub executor that records execution options and returns success.
struct RecordingOptsExecutor {
    exit_code: i32,
    captures: Mutex<Vec<CapturedSpawn>>,
}

impl RecordingOptsExecutor {
    fn new(exit_code: i32) -> Self {
        Self {
            exit_code,
            captures: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl AgentExecutor for RecordingOptsExecutor {
    async fn spawn(&self, opts: &AgentOpts) -> Result<AgentHandle> {
        self.captures.lock().await.push(CapturedSpawn {
            worktree_path: opts.worktree_path.clone(),
            agent_invocation: opts.agent_invocation,
            directive: opts.directive.clone(),
            stdin_content: opts.stdin_content.clone(),
            max_turns: opts.max_turns,
            max_budget_usd: opts.max_budget_usd,
        });

        if let Some(ref log_file) = opts.log_file {
            if let Some(parent) = log_file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(log_file, b"")?;
        }

        Ok(AgentHandle {
            session: opts.session_name.clone(),
            inline_exit_code: Some(self.exit_code),
        })
    }
}

/// A recording executor that pauses resumed invocations after the registry
/// reservation should exist but before the inline run can complete.
struct BlockingResumeExecutor {
    captures: Mutex<Vec<CapturedSpawn>>,
    resume_started: Notify,
    release_resume: Notify,
}

impl BlockingResumeExecutor {
    fn new() -> Self {
        Self {
            captures: Mutex::new(Vec::new()),
            resume_started: Notify::new(),
            release_resume: Notify::new(),
        }
    }
}

#[async_trait::async_trait]
impl AgentExecutor for BlockingResumeExecutor {
    async fn spawn(&self, opts: &AgentOpts) -> Result<AgentHandle> {
        self.captures.lock().await.push(CapturedSpawn {
            worktree_path: opts.worktree_path.clone(),
            agent_invocation: opts.agent_invocation,
            directive: opts.directive.clone(),
            stdin_content: opts.stdin_content.clone(),
            max_turns: opts.max_turns,
            max_budget_usd: opts.max_budget_usd,
        });

        if matches!(opts.agent_invocation, AgentInvocation::Resume(_)) {
            self.resume_started.notify_one();
            self.release_resume.notified().await;
        }

        if let Some(ref log_file) = opts.log_file {
            if let Some(parent) = log_file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(log_file, b"")?;
        }

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
        repos: vec![],
        inline: true,
        force: false,
        dry_run: false,
        directives: None,
        no_worktree: false,
        max_budget_usd: None,
        max_turns: None,
        resume: None,
        retries: 0,
        list: false,
        ephemeral: false,
        timeout: None,
        json: false,
    }
}

fn session_id(value: &str) -> AgentSessionId {
    AgentSessionId::parse_str(value).unwrap()
}

fn count_diag_files(log_dir: &Path) -> usize {
    std::fs::read_dir(log_dir)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.flatten())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "diag"))
        .count()
}

fn count_jsonl_files(log_dir: &Path) -> usize {
    std::fs::read_dir(log_dir)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.flatten())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .count()
}

fn dispatch_record_fixture(
    id: &str,
    status: Status,
    transcript_cwd: PathBuf,
    session_id: Option<AgentSessionId>,
    capabilities: Option<AgentCapabilities>,
) -> DispatchRecord {
    let now = chrono::Utc::now();
    DispatchRecord {
        id: id.to_string(),
        task_slug: Some(format!("tasks/{id}")),
        branch: format!("tasks--{id}"),
        worktree_path: transcript_cwd.clone(),
        session: id.to_string(),
        log_file: transcript_cwd.join(format!("{id}.jsonl")),
        status,
        directive: Directive::Implement,
        retries: 0,
        resolver: "task".to_string(),
        pr_urls: vec![],
        no_worktree: false,
        original_input: Some(format!("tasks/{id}")),
        checks: HealthChecks::default(),
        kb_root: None,
        cost_usd: None,
        num_turns: None,
        duration_ms: None,
        artifacts: None,
        work_unit_id: None,
        agent_provider: CLAUDE_AGENT_PROVIDER.to_string(),
        agent_session_id: session_id,
        agent_transcript_cwd: Some(transcript_cwd),
        resume_of_dispatch_id: None,
        agent_capabilities: capabilities,
        terminal_locator: None,
        dispatched_at: now,
        updated_at: now,
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

async fn dispatch_source_task(
    config: &AtcConfig,
    registry: &dyn Registry,
    executor: &dyn AgentExecutor,
    slug: &str,
) -> DispatchRecord {
    let opts = default_run_opts(slug, Directive::Implement);
    let outcome = dispatch_via_pipeline(config, registry, executor, slug, &opts)
        .await
        .expect("source dispatch failed");
    registry
        .get(&outcome.id)
        .await
        .unwrap()
        .expect("source dispatch should be recorded")
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
    assert_eq!(record.agent_provider, "claude");
    let agent_session_id = record
        .agent_session_id
        .as_ref()
        .map(ToString::to_string)
        .expect("new dispatch should persist an agent session id");
    uuid::Uuid::parse_str(&agent_session_id).expect("agent_session_id should be a valid UUID");
    assert_ne!(record.session, agent_session_id);
    assert_eq!(
        record.agent_transcript_cwd.as_deref(),
        Some(record.worktree_path.as_path())
    );
    assert_eq!(
        record
            .agent_capabilities
            .as_ref()
            .map(|capabilities| capabilities.supports_resume_by_session_id),
        Some(true)
    );
    assert!(
        record.terminal_locator.is_none(),
        "inline dispatch should not fabricate a terminal locator"
    );
}

#[tokio::test]
async fn test_dispatch_non_inline_persists_tmux_terminal_locator() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_script(&fix.bin_dir());
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(StubExecutor { exit_code: 0 });

    let mut opts = default_run_opts("tasks/gitkb-locator", Directive::Implement);
    opts.inline = false;
    let outcome = dispatch_via_pipeline(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        "tasks/gitkb-locator",
        &opts,
    )
    .await
    .expect("dispatch failed");

    let record = registry
        .get(&outcome.id)
        .await
        .unwrap()
        .expect("registry record should exist");
    let locator = record
        .terminal_locator
        .expect("non-inline dispatch should persist terminal locator");

    match locator {
        TerminalLocator::Tmux(tmux) => {
            assert_eq!(tmux.session, record.session);
            assert_eq!(tmux.cwd.as_deref(), Some(record.worktree_path.as_path()));
            assert_eq!(tmux.source, TerminalLocatorSource::AtcDispatch);
            assert_eq!(tmux.confidence, TerminalLocatorConfidence::Exact);
        }
        TerminalLocator::Cloud(_) => panic!("local dispatch should not produce a cloud locator"),
    }
}

#[tokio::test]
async fn test_dispatch_resume_prompt_uses_source_session_and_transcript_cwd() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_script(&fix.bin_dir());
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(RecordingOptsExecutor::new(0));

    let source_opts = default_run_opts("tasks/gitkb-resume", Directive::Implement);
    let source_outcome = dispatch_via_pipeline(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        "tasks/gitkb-resume",
        &source_opts,
    )
    .await
    .expect("source dispatch failed");

    let source = registry
        .get(&source_outcome.id)
        .await
        .unwrap()
        .expect("source dispatch should be recorded");
    let source_session_id = source
        .agent_session_id
        .expect("source should persist an agent session id");
    let source_transcript_cwd = std::fs::canonicalize(
        source
            .agent_transcript_cwd
            .clone()
            .expect("source should persist transcript cwd"),
    )
    .expect("source transcript cwd should canonicalize");
    assert!(
        source_transcript_cwd.is_dir(),
        "source transcript cwd should exist"
    );

    let mut prompt_config = fix.config.clone();
    prompt_config.resolvers.task.enabled = false;

    let mut resume_opts = default_run_opts("Follow up on the previous work", Directive::Implement);
    resume_opts.resume = Some(source.id.clone());
    resume_opts.max_turns = Some(77);
    resume_opts.max_budget_usd = Some(4.25);
    resume_opts.repos = vec!["custom-repo".to_string()];
    let resumed_outcome = dispatch_via_pipeline(
        &prompt_config,
        registry.as_ref(),
        executor.as_ref(),
        "Follow up on the previous work",
        &resume_opts,
    )
    .await
    .expect("resume dispatch failed");

    assert_ne!(source_outcome.id, resumed_outcome.id);
    let resumed = registry
        .get(&resumed_outcome.id)
        .await
        .unwrap()
        .expect("resume dispatch should be recorded");
    assert_eq!(resumed.resolver, "prompt");
    assert_eq!(
        resumed.resume_of_dispatch_id.as_deref(),
        Some(source.id.as_str())
    );
    assert_eq!(resumed.agent_session_id, Some(source_session_id));
    assert_eq!(
        resumed.agent_transcript_cwd.as_deref(),
        Some(source_transcript_cwd.as_path())
    );
    assert_eq!(resumed.worktree_path, source_transcript_cwd);
    let work_unit_id = resumed
        .work_unit_id
        .as_deref()
        .expect("resume dispatch should attach to a work unit");
    let work_unit = registry
        .get_work_unit(work_unit_id)
        .await
        .unwrap()
        .expect("resume work unit should exist");
    assert!(
        work_unit.repos.iter().any(|repo| repo == "custom-repo"),
        "explicit --repo context should be stored on the resume work unit: {:?}",
        work_unit.repos
    );

    let captures = executor.captures.lock().await.clone();
    assert_eq!(captures.len(), 2);
    assert_eq!(
        captures[0].agent_invocation,
        AgentInvocation::Fresh(source_session_id)
    );
    assert_eq!(
        captures[1].agent_invocation,
        AgentInvocation::Resume(source_session_id)
    );
    assert_eq!(captures[1].worktree_path, resumed.worktree_path);
    assert_eq!(captures[1].max_turns, 77);
    assert_eq!(captures[1].max_budget_usd, 4.25);
}

#[tokio::test]
async fn test_dispatch_resume_task_input_uses_source_session() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_script(&fix.bin_dir());
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(RecordingOptsExecutor::new(0));

    let source = dispatch_source_task(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        "tasks/gitkb-task-source",
    )
    .await;
    let source_session_id = source.agent_session_id.unwrap();
    let source_transcript_cwd =
        std::fs::canonicalize(source.agent_transcript_cwd.clone().unwrap()).unwrap();

    let mut resume_opts = default_run_opts("tasks/gitkb-task-followup", Directive::Implement);
    resume_opts.resume = Some(source.id.clone());
    let resumed_outcome = dispatch_via_pipeline(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        "tasks/gitkb-task-followup",
        &resume_opts,
    )
    .await
    .expect("task-input resume dispatch failed");

    let resumed = registry.get(&resumed_outcome.id).await.unwrap().unwrap();
    assert_eq!(resumed.resolver, "task");
    assert_eq!(
        resumed.task_slug.as_deref(),
        Some("tasks/gitkb-task-followup")
    );
    assert_eq!(resumed.directive, Directive::Implement);
    assert_eq!(
        resumed.resume_of_dispatch_id.as_deref(),
        Some(source.id.as_str())
    );
    assert_eq!(resumed.agent_session_id, Some(source_session_id));
    assert_eq!(resumed.worktree_path, source_transcript_cwd);

    let captures = executor.captures.lock().await.clone();
    let resume_capture = captures.last().expect("resume dispatch should spawn");
    assert_eq!(
        resume_capture.agent_invocation,
        AgentInvocation::Resume(source_session_id)
    );
    assert_eq!(resume_capture.worktree_path, source_transcript_cwd);
    assert!(
        resume_capture.stdin_content.is_none(),
        "task resume should keep the task-doc stdin path"
    );
}

#[tokio::test]
async fn test_dispatch_resume_template_with_params_uses_source_session() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_script(&fix.bin_dir());
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(RecordingOptsExecutor::new(0));

    let source = dispatch_source_task(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        "tasks/gitkb-template-resume",
    )
    .await;
    let source_session_id = source.agent_session_id.unwrap();
    let source_transcript_cwd =
        std::fs::canonicalize(source.agent_transcript_cwd.clone().unwrap()).unwrap();

    let tmpl_dir = fix.tmp.path().join("templates");
    std::fs::create_dir_all(&tmpl_dir).unwrap();
    std::fs::write(
        tmpl_dir.join("resume-template.md"),
        "---\ndirective: implement\nrequired_params: [topic]\n---\nResume {{topic}} work.",
    )
    .unwrap();
    std::fs::create_dir_all(fix.tmp.path().join("partials")).unwrap();
    std::fs::create_dir_all(fix.tmp.path().join("components")).unwrap();

    let mut template_config = fix.config.clone();
    template_config.prompt.templates_dir = "templates".to_string();
    template_config.prompt.partials_dir = "partials".to_string();
    template_config.prompt.components_dir = "components".to_string();
    template_config.resolvers.task.enabled = false;

    let mut resume_opts = default_run_opts("resume-template", Directive::Implement);
    resume_opts.directive = None;
    resume_opts.resume = Some(source.id.clone());
    resume_opts
        .params
        .insert("topic".to_string(), "auth".to_string());

    let resumed_outcome = dispatch_via_pipeline(
        &template_config,
        registry.as_ref(),
        executor.as_ref(),
        "resume-template",
        &resume_opts,
    )
    .await
    .expect("template resume dispatch failed");

    let resumed = registry.get(&resumed_outcome.id).await.unwrap().unwrap();
    assert_eq!(resumed.resolver, "template");
    assert_eq!(
        resumed.resume_of_dispatch_id.as_deref(),
        Some(source.id.as_str())
    );
    assert_eq!(resumed.agent_session_id, Some(source_session_id));
    assert_eq!(
        resumed.agent_transcript_cwd.as_deref(),
        Some(source_transcript_cwd.as_path())
    );

    let captures = executor.captures.lock().await.clone();
    let resume_capture = captures.last().expect("resume dispatch should spawn");
    assert_eq!(
        resume_capture.agent_invocation,
        AgentInvocation::Resume(source_session_id)
    );
    assert_eq!(resume_capture.worktree_path, source_transcript_cwd);
    assert!(
        resume_capture
            .stdin_content
            .as_deref()
            .is_some_and(|content| content.contains("Resume auth work.")),
        "template params should render into stdin content"
    );
}

#[tokio::test]
async fn test_dispatch_resume_review_fix_with_pr_url_uses_source_session() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_script(&fix.bin_dir());
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(RecordingOptsExecutor::new(0));

    let source = dispatch_source_task(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        "tasks/gitkb-pr-resume",
    )
    .await;
    let source_session_id = source.agent_session_id.unwrap();
    let source_transcript_cwd =
        std::fs::canonicalize(source.agent_transcript_cwd.clone().unwrap()).unwrap();

    let mut prompt_config = fix.config.clone();
    prompt_config.resolvers.task.enabled = false;
    prompt_config.resolvers.template.enabled = false;

    let pr_url = "https://github.com/org/repo/pull/123".to_string();
    let mut resume_opts = default_run_opts("Address the review feedback", Directive::ReviewFix);
    resume_opts.resume = Some(source.id.clone());
    resume_opts.pr_url = Some(pr_url.clone());

    let resumed_outcome = dispatch_via_pipeline(
        &prompt_config,
        registry.as_ref(),
        executor.as_ref(),
        "Address the review feedback",
        &resume_opts,
    )
    .await
    .expect("review-fix resume dispatch failed");

    let resumed = registry.get(&resumed_outcome.id).await.unwrap().unwrap();
    assert_eq!(resumed.resolver, "prompt");
    assert_eq!(resumed.directive, Directive::ReviewFix);
    assert_eq!(resumed.pr_urls, vec![pr_url]);
    assert_eq!(
        resumed.resume_of_dispatch_id.as_deref(),
        Some(source.id.as_str())
    );
    assert_eq!(resumed.agent_session_id, Some(source_session_id));
    assert_eq!(
        resumed.agent_transcript_cwd.as_deref(),
        Some(source_transcript_cwd.as_path())
    );

    let captures = executor.captures.lock().await.clone();
    let resume_capture = captures.last().expect("resume dispatch should spawn");
    assert_eq!(resume_capture.directive, Directive::ReviewFix);
    assert_eq!(
        resume_capture.agent_invocation,
        AgentInvocation::Resume(source_session_id)
    );
    assert_eq!(resume_capture.worktree_path, source_transcript_cwd);
}

#[tokio::test]
async fn test_dispatch_resume_refuses_active_session_unless_forced() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_script(&fix.bin_dir());
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(RecordingOptsExecutor::new(0));

    let source_opts = default_run_opts("tasks/gitkb-active-resume", Directive::Implement);
    let source_outcome = dispatch_via_pipeline(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        "tasks/gitkb-active-resume",
        &source_opts,
    )
    .await
    .expect("source dispatch failed");
    registry
        .update_status(&source_outcome.id, Status::Running)
        .await
        .unwrap();

    let mut prompt_config = fix.config.clone();
    prompt_config.resolvers.task.enabled = false;

    let mut resume_opts =
        default_run_opts("Follow up despite active session", Directive::Implement);
    resume_opts.resume = Some(source_outcome.id.clone());
    let err = dispatch_via_pipeline(
        &prompt_config,
        registry.as_ref(),
        executor.as_ref(),
        "Follow up despite active session",
        &resume_opts,
    )
    .await
    .expect_err("active source session should be rejected");
    assert!(
        err.to_string().contains("already active"),
        "unexpected resume collision error: {err}"
    );

    resume_opts.force = true;
    let forced_outcome = dispatch_via_pipeline(
        &prompt_config,
        registry.as_ref(),
        executor.as_ref(),
        "Follow up despite active session",
        &resume_opts,
    )
    .await
    .expect("--force should allow resuming an active provider session");
    let forced = registry.get(&forced_outcome.id).await.unwrap().unwrap();
    assert_eq!(
        forced.resume_of_dispatch_id.as_deref(),
        Some(source_outcome.id.as_str())
    );
}

#[tokio::test]
async fn test_dispatch_resume_dry_run_has_no_registry_log_or_diag_side_effects() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_script(&fix.bin_dir());
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(RecordingOptsExecutor::new(0));
    let source = dispatch_source_task(
        &fix.config,
        registry.as_ref(),
        executor.as_ref(),
        "tasks/gitkb-dry-run-source",
    )
    .await;

    let records_before = registry.list(StatusFilter::All).await.unwrap().len();
    let log_dir = fix.config.dispatch.resolved_log_dir();
    let diag_before = count_diag_files(&log_dir);
    let jsonl_before = count_jsonl_files(&log_dir);
    let captures_before = executor.captures.lock().await.len();

    let mut prompt_config = fix.config.clone();
    prompt_config.resolvers.task.enabled = false;
    let mut opts = default_run_opts("Dry-run follow up", Directive::Implement);
    opts.resume = Some(source.id.clone());
    opts.dry_run = true;
    let outcome = dispatch_via_pipeline(
        &prompt_config,
        registry.as_ref(),
        executor.as_ref(),
        "Dry-run follow up",
        &opts,
    )
    .await
    .expect("resume dry-run should succeed");
    assert_eq!(outcome.inline_exit_code, Some(0));

    assert_eq!(
        registry.list(StatusFilter::All).await.unwrap().len(),
        records_before,
        "resume dry-run must not insert a registry record"
    );
    assert_eq!(
        count_diag_files(&log_dir),
        diag_before,
        "resume dry-run must not write a diagnostic file"
    );
    assert_eq!(
        count_jsonl_files(&log_dir),
        jsonl_before,
        "resume dry-run must not create a log file"
    );
    assert_eq!(
        executor.captures.lock().await.len(),
        captures_before,
        "resume dry-run must not spawn the executor"
    );
}

#[tokio::test]
async fn test_dispatch_resume_rejects_malformed_source_metadata() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(StubExecutor { exit_code: 0 });
    let mut prompt_config = fix.config.clone();
    prompt_config.resolvers.task.enabled = false;

    let valid_cwd = fix.worktree_base().join("source-cwd");
    std::fs::create_dir_all(&valid_cwd).unwrap();
    let transcript_file = fix.worktree_base().join("transcript-file");
    std::fs::write(&transcript_file, b"not a directory").unwrap();
    let mismatch_cwd = fix.worktree_base().join("mismatch-cwd");
    let mismatch_worktree = fix.worktree_base().join("mismatch-worktree");
    std::fs::create_dir_all(&mismatch_cwd).unwrap();
    std::fs::create_dir_all(&mismatch_worktree).unwrap();
    let unrelated_dot_worktrees_root = tempfile::tempdir().unwrap();
    let unrelated_dot_worktree_cwd = unrelated_dot_worktrees_root
        .path()
        .join(".worktrees")
        .join("source-cwd");
    std::fs::create_dir_all(&unrelated_dot_worktree_cwd).unwrap();
    let mut unsupported = claude_agent_capabilities();
    unsupported.supports_resume_by_session_id = false;

    let mut unsupported_provider = dispatch_record_fixture(
        "unsupported-provider",
        Status::Done,
        valid_cwd.clone(),
        Some(session_id("00000000-0000-4000-8000-000000000500")),
        Some(claude_agent_capabilities()),
    );
    unsupported_provider.agent_provider = "codex".to_string();

    let mut missing_transcript_field = dispatch_record_fixture(
        "missing-transcript-field",
        Status::Done,
        valid_cwd.clone(),
        Some(session_id("00000000-0000-4000-8000-000000000503")),
        Some(claude_agent_capabilities()),
    );
    missing_transcript_field.agent_transcript_cwd = None;

    let mut mismatched_worktree = dispatch_record_fixture(
        "mismatched-worktree",
        Status::Done,
        mismatch_cwd,
        Some(session_id("00000000-0000-4000-8000-000000000507")),
        Some(claude_agent_capabilities()),
    );
    mismatched_worktree.worktree_path = mismatch_worktree;

    let records = vec![
        dispatch_record_fixture(
            "missing-session",
            Status::Done,
            valid_cwd.clone(),
            None,
            Some(claude_agent_capabilities()),
        ),
        unsupported_provider,
        dispatch_record_fixture(
            "unsupported-resume",
            Status::Done,
            valid_cwd.clone(),
            Some(session_id("00000000-0000-4000-8000-000000000501")),
            Some(unsupported),
        ),
        dispatch_record_fixture(
            "missing-transcript-cwd",
            Status::Done,
            fix.worktree_base().join("missing-cwd"),
            Some(session_id("00000000-0000-4000-8000-000000000502")),
            Some(claude_agent_capabilities()),
        ),
        missing_transcript_field,
        dispatch_record_fixture(
            "transcript-file",
            Status::Done,
            transcript_file,
            Some(session_id("00000000-0000-4000-8000-000000000504")),
            Some(claude_agent_capabilities()),
        ),
        dispatch_record_fixture(
            "unrelated-dot-worktrees",
            Status::Done,
            unrelated_dot_worktree_cwd,
            Some(session_id("00000000-0000-4000-8000-000000000508")),
            Some(claude_agent_capabilities()),
        ),
        dispatch_record_fixture(
            "unsafe-root",
            Status::Done,
            PathBuf::from("/"),
            Some(session_id("00000000-0000-4000-8000-000000000505")),
            Some(claude_agent_capabilities()),
        ),
        mismatched_worktree,
    ];
    for record in records {
        registry.insert(&record).await.unwrap();
    }

    for (target, expected) in [
        ("missing-session", "missing agent_session_id"),
        ("unsupported-provider", "provider 'codex' is not supported"),
        (
            "unsupported-resume",
            "does not support resume by session id",
        ),
        ("missing-transcript-cwd", "is not accessible"),
        ("missing-transcript-field", "missing agent_transcript_cwd"),
        ("transcript-file", "is not a directory"),
        ("unrelated-dot-worktrees", "is not under an ATC workspace"),
        ("unsafe-root", "unsafe transcript cwd"),
        ("mismatched-worktree", "does not match source worktree_path"),
    ] {
        let mut opts = default_run_opts("Resume validation probe", Directive::Implement);
        opts.dry_run = true;
        opts.resume = Some(target.to_string());

        let err = dispatch_via_pipeline(
            &prompt_config,
            registry.as_ref(),
            executor.as_ref(),
            "Resume validation probe",
            &opts,
        )
        .await
        .expect_err("malformed resume source should fail before dispatch");
        assert!(
            err.to_string().contains(expected),
            "expected {expected:?} for {target}, got: {err}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn test_dispatch_resume_uses_canonical_transcript_cwd_for_symlink_source() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(RecordingOptsExecutor::new(0));
    let mut prompt_config = fix.config.clone();
    prompt_config.resolvers.task.enabled = false;
    prompt_config.resolvers.template.enabled = false;

    let real_cwd = fix.worktree_base().join("real-symlink-source");
    let link_cwd = fix.worktree_base().join("link-symlink-source");
    std::fs::create_dir_all(&real_cwd).unwrap();
    std::os::unix::fs::symlink(&real_cwd, &link_cwd).unwrap();
    let canonical_cwd = std::fs::canonicalize(&real_cwd).unwrap();

    let source = dispatch_record_fixture(
        "symlink-source",
        Status::Done,
        link_cwd.clone(),
        Some(session_id("00000000-0000-4000-8000-000000000509")),
        Some(claude_agent_capabilities()),
    );
    registry.insert(&source).await.unwrap();

    let mut opts = default_run_opts("Continue from symlinked source", Directive::Implement);
    opts.resume = Some(source.id.clone());
    let outcome = dispatch_via_pipeline(
        &prompt_config,
        registry.as_ref(),
        executor.as_ref(),
        "Continue from symlinked source",
        &opts,
    )
    .await
    .expect("resume should canonicalize and dispatch");

    let resumed = registry.get(&outcome.id).await.unwrap().unwrap();
    assert_eq!(resumed.worktree_path, canonical_cwd);
    assert_eq!(
        resumed.agent_transcript_cwd.as_deref(),
        Some(canonical_cwd.as_path())
    );

    let captures = executor.captures.lock().await.clone();
    let resume_capture = captures.last().expect("resume dispatch should spawn");
    assert_eq!(resume_capture.worktree_path, canonical_cwd);
}

#[cfg(unix)]
#[tokio::test]
async fn test_dispatch_resume_rejects_symlink_transcript_cwd_outside_workspace_roots() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(StubExecutor { exit_code: 0 });
    let mut prompt_config = fix.config.clone();
    prompt_config.resolvers.task.enabled = false;
    prompt_config.resolvers.template.enabled = false;

    let outside = tempfile::tempdir().unwrap();
    let outside_cwd = outside.path().join("source-cwd");
    std::fs::create_dir_all(&outside_cwd).unwrap();
    let link_cwd = fix.worktree_base().join("outside-symlink-source");
    std::os::unix::fs::symlink(&outside_cwd, &link_cwd).unwrap();

    let source = dispatch_record_fixture(
        "outside-symlink-source",
        Status::Done,
        link_cwd,
        Some(session_id("00000000-0000-4000-8000-000000000510")),
        Some(claude_agent_capabilities()),
    );
    registry.insert(&source).await.unwrap();

    let mut opts = default_run_opts("Reject outside symlink", Directive::Implement);
    opts.dry_run = true;
    opts.resume = Some(source.id.clone());
    let err = dispatch_via_pipeline(
        &prompt_config,
        registry.as_ref(),
        executor.as_ref(),
        "Reject outside symlink",
        &opts,
    )
    .await
    .expect_err("resume symlink target outside configured roots should fail");
    assert!(
        err.to_string().contains("is not under an ATC workspace"),
        "unexpected symlink safety error: {err}"
    );
}

#[tokio::test]
async fn test_dispatch_resume_reserves_provider_session_before_spawn_completes() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_script(&fix.bin_dir());
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let source_executor = Arc::new(StubExecutor { exit_code: 0 });
    let source = dispatch_source_task(
        &fix.config,
        registry.as_ref(),
        source_executor.as_ref(),
        "tasks/gitkb-concurrent-resume",
    )
    .await;
    let source_transcript_cwd = source.agent_transcript_cwd.clone().unwrap();
    let mut active_units_before: Vec<String> = registry
        .list_active_work_units()
        .await
        .unwrap()
        .into_iter()
        .map(|unit| unit.id)
        .collect();
    active_units_before.sort();

    let mut prompt_config = fix.config.clone();
    prompt_config.resolvers.task.enabled = false;
    let executor = Arc::new(BlockingResumeExecutor::new());

    let mut first_opts = default_run_opts("Concurrent resume one", Directive::Implement);
    first_opts.resume = Some(source.id.clone());
    let first_config = prompt_config.clone();
    let first_registry = registry.clone();
    let first_executor = executor.clone();
    let first = tokio::spawn(async move {
        dispatch_via_pipeline(
            &first_config,
            first_registry.as_ref(),
            first_executor.as_ref(),
            "Concurrent resume one",
            &first_opts,
        )
        .await
    });

    executor.resume_started.notified().await;
    let diag_count_before_rejected_resume =
        count_diag_files(&prompt_config.dispatch.resolved_log_dir());

    let mut second_opts = default_run_opts("Concurrent resume two", Directive::Implement);
    second_opts.resume = Some(source.id.clone());
    second_opts.pr_url = Some("https://github.com/org/repo/pull/77".to_string());
    let err = dispatch_via_pipeline(
        &prompt_config,
        registry.as_ref(),
        executor.as_ref(),
        "Concurrent resume two",
        &second_opts,
    )
    .await
    .expect_err("second concurrent resume should be rejected");
    assert!(
        err.to_string().contains("already active"),
        "unexpected concurrent resume error: {err}"
    );

    let captures = executor.captures.lock().await.clone();
    assert_eq!(
        captures.len(),
        1,
        "rejected concurrent resume must not reach executor"
    );
    assert!(
        !source_transcript_cwd.join(".dispatch-prefetch").exists(),
        "rejected concurrent resume must not write provider output"
    );
    let mut active_units_after: Vec<String> = registry
        .list_active_work_units()
        .await
        .unwrap()
        .into_iter()
        .map(|unit| unit.id)
        .collect();
    active_units_after.sort();
    assert_eq!(
        active_units_after, active_units_before,
        "rejected concurrent resume must not create a work unit"
    );
    assert_eq!(
        count_diag_files(&prompt_config.dispatch.resolved_log_dir()),
        diag_count_before_rejected_resume,
        "rejected concurrent resume must not write a diagnostic file"
    );

    executor.release_resume.notify_waiters();
    first
        .await
        .expect("first resume task panicked")
        .expect("first resume should complete");
}

#[tokio::test]
async fn test_dispatch_resume_spawn_failure_marks_pre_spawn_reservation_failed() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_script(&fix.bin_dir());
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let source_executor = Arc::new(StubExecutor { exit_code: 0 });
    let source = dispatch_source_task(
        &fix.config,
        registry.as_ref(),
        source_executor.as_ref(),
        "tasks/gitkb-resume-spawn-failure",
    )
    .await;

    let mut prompt_config = fix.config.clone();
    prompt_config.resolvers.task.enabled = false;
    let mut opts = default_run_opts("Resume should fail before spawn", Directive::Implement);
    opts.resume = Some(source.id.clone());

    let err = dispatch_via_pipeline(
        &prompt_config,
        registry.as_ref(),
        &FailingExecutor,
        "Resume should fail before spawn",
        &opts,
    )
    .await
    .expect_err("failing executor should fail dispatch");
    assert!(
        err.to_string().contains("executor spawn failed"),
        "unexpected spawn error: {err}"
    );

    let records = registry.list(StatusFilter::All).await.unwrap();
    let failed = records
        .iter()
        .find(|record| record.resume_of_dispatch_id.as_deref() == Some(source.id.as_str()))
        .expect("resume reservation should remain recorded");
    assert_eq!(failed.status, Status::Failed);
    assert!(
        failed.session.is_empty(),
        "failed pre-spawn resume reservation should not retain a tmux session"
    );
    assert!(
        failed.terminal_locator.is_none(),
        "failed pre-spawn resume reservation should not retain a terminal locator"
    );
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
async fn test_dispatch_resolves_directive_from_frontmatter() {
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
        repos: vec![],
        inline: true,
        force: false,
        dry_run: false,
        directives: None,
        no_worktree: false,
        max_budget_usd: None,
        max_turns: None,
        resume: None,
        retries: 0,
        list: false,
        ephemeral: false,
        timeout: None,
        json: false,
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
        repos: vec![],
        inline: true,
        force: false,
        dry_run: false,
        directives: Some("focus on error handling".to_string()),
        no_worktree: false,
        max_budget_usd: None,
        max_turns: None,
        resume: None,
        retries: 0,
        list: false,
        ephemeral: false,
        timeout: None,
        json: false,
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
        repos: vec![],
        inline: true,
        force: false,
        dry_run: false,
        directives: None,
        no_worktree: false,
        max_budget_usd: None,
        max_turns: None,
        resume: None,
        retries: 0,
        list: false,
        ephemeral: false,
        timeout: None,
        json: false,
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
        repos: vec![],
        inline: true,
        force: false,
        dry_run: true,
        directives: None,
        no_worktree: false,
        max_budget_usd: None,
        max_turns: None,
        resume: None,
        retries: 0,
        list: false,
        ephemeral: false,
        timeout: None,
        json: false,
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

#[tokio::test]
async fn test_dispatch_dry_run_document_policy_rejects_unsafe_slug() {
    let _guard = PATH_MUTEX.lock().await;

    let fix = TestFixture::new();
    write_stub_git_bin(&fix.bin_dir());
    write_stub_meta_script(&fix.bin_dir(), &fix.worktree_base());

    let tmpl_dir = fix.tmp.path().join("templates");
    std::fs::create_dir_all(&tmpl_dir).unwrap();
    std::fs::write(
        tmpl_dir.join("doc-close.md"),
        "---\ndirective: close\nworktree: document\nrequired_params: [task]\n---\nClose {{task}}.",
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
    config.resolvers.task.enabled = false;

    let registry = Arc::new(SqliteRegistry::in_memory().await.unwrap());
    let executor = Arc::new(StubExecutor { exit_code: 0 });

    let mut opts = default_run_opts("doc-close", Directive::Close);
    opts.directive = None;
    opts.dry_run = true;
    opts.params
        .insert("task".to_string(), "../tasks/bad".to_string());

    let err = dispatch_via_pipeline(
        &config,
        registry.as_ref(),
        executor.as_ref(),
        "doc-close",
        &opts,
    )
    .await
    .expect_err("dry-run document policy should reject unsafe slug");

    assert!(
        err.to_string().contains("invalid slug for document policy"),
        "unexpected dry-run document-policy error: {err}"
    );
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
        repos: vec![],
        inline: true,
        force: false,
        dry_run: false,
        directives: None,
        no_worktree: false,
        max_budget_usd: None,
        max_turns: None,
        resume: None,
        retries: 0,
        list: false,
        ephemeral: false,
        timeout: None,
        json: false,
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
        repos: vec![],
        inline: true,
        force: false,
        dry_run: false,
        directives: None,
        no_worktree: false,
        max_budget_usd: None,
        max_turns: None,
        resume: None,
        retries: 0,
        list: false,
        ephemeral: false,
        timeout: None,
        json: false,
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
