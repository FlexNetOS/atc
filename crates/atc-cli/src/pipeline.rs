use anyhow::{Context, Result};
use atc_core::config::AtcConfig;
use atc_core::executor::{AgentExecutor, AgentInvocation, AgentOpts};
use atc_core::registry::Registry;
use atc_core::resolver::{InputResolver, ResolvedInput};
use atc_core::types::{
    claude_agent_provider, AgentCapabilities, AgentSessionMetadata, Directive, DispatchOutcome,
    DispatchRecord, HealthChecks, RunOpts, Status, TerminalLocator, WorktreePolicy,
    CLAUDE_AGENT_PROVIDER,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use atc_core::types::{WorkUnit, WorkUnitStatus};

use crate::dispatch::{
    auto_checkout_to_main, compute_allowed_paths, derive_pr_url_from_comment, discover_meta,
    ensure_worktree, parse_comment_url, resolve_document_workspace, resolve_gh_token,
    resolve_pr_repo_path, tmux_session_alive, validate_branch_name, write_diag_file, MetaDiscovery,
    WorktreeOpts,
};
use crate::output_schema::SCHEMA_VERSION;
use crate::shell_text::shell_display_arg;
use atc_core::terminal_text::{display_text, terminal_safe_json_pretty};

fn agent_invocation_from_metadata(metadata: &AgentSessionMetadata) -> AgentInvocation {
    match metadata.session_id {
        Some(session_id) if metadata.resume_of_dispatch_id.is_some() => {
            AgentInvocation::Resume(session_id)
        }
        Some(session_id) => AgentInvocation::Fresh(session_id),
        None => AgentInvocation::None,
    }
}

/// JSON envelope shared by `atc run --json`. v1 schema; future fields are additive.
///
/// `kind` is `"dispatch"` for both real and dry-run dispatches and `"error"` when
/// the dispatch could not be created. Consumers should switch on `kind` and
/// ignore unknown fields. Dry runs are tagged via `data.is_dry_run = true` and
/// `data.status = "preview"`.
#[derive(Debug, Serialize)]
pub struct RunOutputV1<T: Serialize> {
    pub schema_version: u32,
    pub kind: &'static str,
    pub data: T,
}

/// Successful or dry-run dispatch payload. All fields are populated whenever
/// available; missing data is omitted (e.g. `log_file` is `None` for dry runs
/// and ephemeral dispatches that do not write a log file).
#[derive(Debug, Serialize)]
pub struct DispatchEnvelope<'a> {
    pub dispatch_id: &'a str,
    pub task_slug: Option<&'a str>,
    pub branch: &'a str,
    pub session: &'a str,
    pub directive: &'a str,
    pub worktree_path: String,
    pub worktree_policy: &'static str,
    pub status: &'static str,
    pub resolver: &'a str,
    pub pr_urls: Vec<&'a str>,
    pub log_file: Option<String>,
    pub agent_provider: &'a str,
    pub agent_session_id: Option<String>,
    pub agent_transcript_cwd: Option<String>,
    pub resume_of_dispatch_id: Option<&'a str>,
    pub agent_capabilities: Option<&'a AgentCapabilities>,
    pub is_dry_run: bool,
    pub inline_exit_code: Option<i32>,
    pub dispatched_at: DateTime<Utc>,
}

/// Error payload emitted on stdout when `--json` is set and the dispatch
/// fails before reaching the registry. `code` is a coarse category — v1 keeps
/// it as a single `dispatch_error` so consumers don't take a hard dependency
/// on a category set that hasn't stabilized yet.
#[derive(Debug, Serialize)]
pub struct ErrorEnvelope {
    pub code: &'static str,
    pub message: String,
}

/// Format the full error chain as a colon-separated string.
pub fn format_error_chain(err: &anyhow::Error) -> String {
    let mut chain = Vec::new();
    chain.push(format!("{err}"));
    let mut cause = err.source();
    while let Some(c) = cause {
        chain.push(format!("{c}"));
        cause = c.source();
    }
    chain.join(": ")
}

/// Emit the `kind: "error"` envelope on stdout. Caller is responsible for
/// exiting non-zero. The error message includes the full anyhow chain so
/// programmatic consumers and humans both have enough context to act.
pub fn emit_run_error_envelope(err: &anyhow::Error) {
    let envelope = RunOutputV1 {
        schema_version: SCHEMA_VERSION,
        kind: "error",
        data: ErrorEnvelope {
            code: "dispatch_error",
            message: format_error_chain(err),
        },
    };
    match terminal_safe_json_pretty(&envelope) {
        Ok(s) => println!("{s}"),
        Err(e) => {
            // Serialization is essentially infallible for these owned types;
            // if it ever does fail, fall back to a hand-written envelope so
            // consumers still see structured output rather than nothing.
            eprintln!("warning: failed to serialize error envelope: {e}");
            println!(
                "{{\"schema_version\":{SCHEMA_VERSION},\"kind\":\"error\",\"data\":{{\"code\":\"dispatch_error\",\"message\":\"<unserializable>\"}}}}"
            );
        }
    }
}

fn document_policy_slug<'a>(
    resolved: &'a ResolvedInput,
    params: &'a HashMap<String, String>,
) -> Option<&'a str> {
    resolved
        .task_slug
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            params
                .get("task")
                .map(|s| s.as_str())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            params
                .get("slug")
                .map(|s| s.as_str())
                .filter(|s| !s.is_empty())
        })
}

fn validate_document_policy_slug(slug: &str) -> Result<()> {
    let slug_path = Path::new(slug);
    let unsafe_slug = slug_path.is_absolute()
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
        });
    anyhow::ensure!(
        !unsafe_slug,
        "invalid slug for document policy: unsafe path '{}'",
        display_text(slug)
    );
    Ok(())
}

fn canonicalize_existing_dir(path: &Path, source_id: &str, label: &str) -> Result<PathBuf> {
    let source_id_display = display_text(source_id);
    let display_path = display_text(&path.display().to_string());
    let canonical = std::fs::canonicalize(path).with_context(|| {
        format!(
            "cannot resume dispatch {}: {} '{}' is not accessible",
            source_id_display, label, display_path
        )
    })?;
    anyhow::ensure!(
        canonical.is_dir(),
        "cannot resume dispatch {}: {} '{}' is not a directory",
        source_id_display,
        label,
        display_path
    );
    Ok(canonical)
}

fn canonical_if_exists(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

fn is_home_dir(path: &Path) -> bool {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| canonical_if_exists(&home))
        .is_some_and(|home| path == home)
}

#[allow(clippy::too_many_arguments)]
fn build_dispatch_record(
    resolved: &ResolvedInput,
    opts: &RunOpts,
    original_input: &str,
    resolver_name: &str,
    pr_url: Option<&str>,
    worktree_path: &Path,
    session_name: &str,
    log_file: &Path,
    agent_metadata: &AgentSessionMetadata,
    now: DateTime<Utc>,
) -> DispatchRecord {
    let terminal_locator = dispatch_terminal_locator(opts, session_name, worktree_path, now);
    DispatchRecord {
        id: resolved.dispatch_id.clone(),
        task_slug: resolved.task_slug.clone(),
        branch: resolved.branch.clone(),
        worktree_path: worktree_path.to_path_buf(),
        session: session_name.to_string(),
        log_file: log_file.to_path_buf(),
        status: Status::Running,
        directive: resolved.directive.clone(),
        retries: opts.retries,
        resolver: resolver_name.to_string(),
        pr_urls: pr_url.into_iter().map(str::to_string).collect(),
        no_worktree: opts.no_worktree,
        original_input: Some(original_input.to_string()),
        checks: HealthChecks::default(),
        kb_root: resolved.kb_root.clone(),
        cost_usd: None,
        num_turns: None,
        duration_ms: None,
        artifacts: None,
        work_unit_id: None,
        agent_provider: agent_metadata.provider.clone(),
        agent_session_id: agent_metadata.session_id,
        agent_transcript_cwd: agent_metadata.transcript_cwd.clone(),
        resume_of_dispatch_id: agent_metadata.resume_of_dispatch_id.clone(),
        agent_capabilities: agent_metadata.capabilities,
        terminal_locator,
        dispatched_at: now,
        updated_at: now,
    }
}

fn dispatch_terminal_locator(
    opts: &RunOpts,
    session_name: &str,
    worktree_path: &Path,
    detected_at: DateTime<Utc>,
) -> Option<TerminalLocator> {
    if opts.inline || session_name.trim().is_empty() {
        None
    } else {
        Some(TerminalLocator::atc_tmux(
            session_name,
            Some(worktree_path.to_path_buf()),
            detected_at,
        ))
    }
}

/// The unified dispatch pipeline. All resolvers feed into this to dispatch agents.
pub struct DispatchPipeline<'a> {
    pub resolvers: Vec<Box<dyn InputResolver>>,
    pub config: &'a AtcConfig,
    pub registry: &'a dyn Registry,
    pub executor: &'a dyn AgentExecutor,
}

impl<'a> DispatchPipeline<'a> {
    /// Execute the dispatch pipeline for the given input.
    pub async fn execute(&self, input: &str, opts: &RunOpts) -> Result<DispatchOutcome> {
        // 0. Ephemeral guard
        if opts.ephemeral && !opts.inline {
            anyhow::bail!("--ephemeral requires --inline");
        }
        if opts.ephemeral && opts.resume.is_some() {
            anyhow::bail!("--resume is not supported with --ephemeral");
        }

        // 1. Find first resolver that can handle input
        let resolver = self.find_resolver(input).await?;
        info!(resolver = resolver.name(), "selected resolver");

        // 1b. Auto-derive PR URL from comment URL *before* resolve() so that
        // template resolvers see the `pr` param for branch selection and
        // `required_params: [pr]` validation.
        let mut effective_params = opts.params.clone();
        let (comment_id, comment_type) = if let Some(comment_url) = effective_params
            .get("comment")
            .cloned()
            .filter(|s| !s.is_empty())
        {
            // Auto-derive PR URL from comment URL
            if !effective_params.contains_key("pr") || effective_params["pr"].is_empty() {
                if let Some(pr_url) = derive_pr_url_from_comment(&comment_url) {
                    info!(
                        comment_url = %display_text(&comment_url),
                        pr_url = %display_text(&pr_url),
                        "auto-derived PR URL from comment URL"
                    );
                    effective_params.insert("pr".to_string(), pr_url);
                }
            }
            parse_comment_url(&comment_url)
        } else {
            (None, None)
        };

        // 2. Resolve → ResolvedInput (with enriched params so resolvers see `pr`)
        let mut resolved = if effective_params != opts.params {
            let mut patched = opts.clone();
            patched.params = effective_params.clone();
            resolver.resolve(input, &patched, self.config).await?
        } else {
            resolver.resolve(input, opts, self.config).await?
        };

        // 3. Validate

        // PR URL can come from --pr-url or from template --param pr=<url>.
        // Filter out blank values — Some("") must be treated as None.
        let effective_pr_url = opts
            .pr_url
            .clone()
            .or_else(|| effective_params.get("pr").cloned())
            .filter(|s| !s.is_empty());
        if matches!(
            resolved.directive,
            Directive::ReviewFix | Directive::PrComments
        ) && effective_pr_url.is_none()
        {
            // Rollback resolver state
            let tmp_record = self.make_tmp_record(&resolved, opts, resolver.name());
            resolver.on_cleanup(&tmp_record, self.config, None).await;
            anyhow::bail!(
                "{} directive requires a PR URL (--pr-url or --param pr=<url>). Cannot dispatch without it.\n\
                 hint: pass `--pr-url <url>` or `--param pr=<url>` — never as a positional arg.",
                resolved.directive.as_str()
            );
        }

        // Validate branch name
        if let Err(e) = validate_branch_name(&resolved.branch).await {
            let tmp_record = self.make_tmp_record(&resolved, opts, resolver.name());
            resolver.on_cleanup(&tmp_record, self.config, None).await;
            return Err(e);
        }

        // Resolve per-directive budget/turns
        let dispatch_cfg = &self.config.dispatch;
        let directive_key = resolved.directive.as_str();
        let budget = opts
            .max_budget_usd
            .or_else(|| {
                self.config
                    .directives
                    .get(directive_key)
                    .and_then(|m| m.max_budget_usd)
            })
            .unwrap_or(dispatch_cfg.max_budget_usd);
        let turns = opts
            .max_turns
            .or(resolved.max_turns)
            .or_else(|| {
                self.config
                    .directives
                    .get(directive_key)
                    .and_then(|m| m.max_turns)
            })
            .unwrap_or(dispatch_cfg.max_turns);

        // Duplicate session detection
        let session_name = resolved.dispatch_id.clone();
        if !opts.force && tmux_session_alive(&session_name).await {
            let tmp_record = self.make_tmp_record(&resolved, opts, resolver.name());
            resolver.on_cleanup(&tmp_record, self.config, None).await;
            anyhow::bail!(
                "tmux session '{}' already exists. Use --force to override.\n\
                 hint: `atc info {}` shows the existing dispatch; \
                 `atc stop` and `atc cleanup` end it cleanly.",
                display_text(&session_name),
                shell_display_arg(&session_name)
            );
        }

        // Determine effective worktree policy early so dry_run can display it.
        let worktree_policy = if opts.no_worktree {
            WorktreePolicy::Current
        } else {
            resolved.worktree_policy.unwrap_or(WorktreePolicy::Branch)
        };

        let resume_agent_metadata = match self
            .resolve_resume_metadata(opts.resume.as_deref(), opts.force)
            .await
        {
            Ok(metadata) => metadata,
            Err(e) => {
                let tmp_record = self.make_tmp_record(&resolved, opts, resolver.name());
                resolver.on_cleanup(&tmp_record, self.config, None).await;
                return Err(e);
            }
        };

        // 4. Dry run — no resolver state was mutated (resolve() skips CAS claim
        //    when dry_run is set), so we can return immediately. Resolve repo
        //    context up-front so the preview matches what dispatch will use.
        if opts.dry_run {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let meta = discover_meta(&cwd).await;
            let workspace_root = dispatch_cfg
                .resolved_meta_workspace_root(self.config.config_dir.as_deref())
                .ok()
                .or_else(|| meta.as_ref().map(|m| m.workspace_root.clone()))
                .unwrap_or_else(|| cwd.clone());
            let dry_repos = resolve_base_repos(
                opts,
                effective_pr_url.as_deref(),
                &workspace_root,
                meta.as_ref(),
                dispatch_cfg,
            )
            .await;

            // Resolve document workspace so the preview path matches dispatch.
            let effective_workspace_root = if let Some(resume_metadata) = &resume_agent_metadata {
                resume_metadata
                    .transcript_cwd
                    .clone()
                    .unwrap_or_else(|| workspace_root.clone())
            } else if worktree_policy == WorktreePolicy::Document {
                let slug = document_policy_slug(&resolved, &effective_params).ok_or_else(|| {
                    anyhow::anyhow!(
                        "worktree: document requires a task or slug parameter to resolve \
                         the document workspace (set --param task=<slug> or use a task dispatch)"
                    )
                })?;
                validate_document_policy_slug(slug)?;
                let kb_root = resolved.kb_root.as_deref().unwrap_or(&workspace_root);
                let worktree_base = dispatch_cfg.resolved_worktree_base();
                match resolve_document_workspace(slug, kb_root, &worktree_base, &workspace_root)
                    .await?
                {
                    Some(doc_ws) => doc_ws.cwd,
                    None => workspace_root.clone(),
                }
            } else {
                workspace_root.clone()
            };

            // Compute providers for display
            let mut dry_providers =
                atc_core::providers::providers_for_directive(self.config, &resolved.directive);
            if (effective_params.contains_key("pr") || effective_pr_url.is_some())
                && !dry_providers.iter().any(|p| p.name() == "pr-context")
            {
                if let Some(p) = atc_core::providers::make_provider("pr-context") {
                    dry_providers.insert(0, p);
                }
            }
            let provider_names: Vec<&str> = dry_providers.iter().map(|p| p.name()).collect();
            return self.dry_run(
                &resolved,
                effective_pr_url.as_deref(),
                budget,
                turns,
                resolver.name(),
                &provider_names,
                opts.ephemeral,
                worktree_policy,
                &dry_repos,
                &effective_workspace_root,
                opts.json,
                resume_agent_metadata.as_ref(),
            );
        }

        // 4b. Ephemeral fast path — skip worktree, log, diag, providers, system prompt, registry, work unit
        if opts.ephemeral {
            // Ephemeral requires a rendered template body (template resolver only).
            // Task and prompt resolvers don't produce template_body, so reject them.
            let stdin_content = resolved.template_body.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "--ephemeral requires a template dispatch (template_body is None). \
                     Use `atc quick <template>` or `atc run <template> --ephemeral --inline`."
                )
            })?;

            // Honor worktree_policy for CWD selection even in ephemeral mode.
            let process_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let cwd = match worktree_policy {
                WorktreePolicy::None => {
                    // Use workspace root instead of CWD.
                    dispatch_cfg
                        .resolved_meta_workspace_root(self.config.config_dir.as_deref())
                        .ok()
                        .unwrap_or_else(|| process_cwd.clone())
                }
                WorktreePolicy::Document => {
                    // Resolve CWD from document location when a slug is available.
                    let slug =
                        document_policy_slug(&resolved, &effective_params).map(str::to_string);
                    if let Some(slug) = slug.as_deref() {
                        if let Err(e) = validate_document_policy_slug(slug) {
                            let tmp_record = self.make_tmp_record(&resolved, opts, resolver.name());
                            resolver.on_cleanup(&tmp_record, self.config, None).await;
                            return Err(e);
                        }
                        let workspace_root = dispatch_cfg
                            .resolved_meta_workspace_root(self.config.config_dir.as_deref())
                            .ok()
                            .unwrap_or_else(|| process_cwd.clone());
                        let kb_root = resolved.kb_root.as_deref().unwrap_or(&workspace_root);
                        let worktree_base = dispatch_cfg.resolved_worktree_base();
                        match resolve_document_workspace(
                            slug,
                            kb_root,
                            &worktree_base,
                            &workspace_root,
                        )
                        .await
                        {
                            Ok(Some(doc_ws)) => {
                                resolved.env_overrides.insert(
                                    "GITKB_WORKSPACE".to_string(),
                                    doc_ws.workspace_branch.clone(),
                                );
                                doc_ws.cwd
                            }
                            Ok(None) => process_cwd.clone(),
                            Err(e) => {
                                warn!(
                                    slug = %display_text(slug),
                                    error = %display_text(&e.to_string()),
                                    "ephemeral document workspace resolution failed, using CWD"
                                );
                                process_cwd.clone()
                            }
                        }
                    } else {
                        process_cwd.clone()
                    }
                }
                // Current | Branch — use process CWD.
                _ => process_cwd.clone(),
            };
            let slug_for_agent = resolved.task_slug.as_deref().unwrap_or(&resolved.branch);
            let agent_metadata = claude_agent_provider().ephemeral_session_metadata();

            // Security invariants (mirrors step 8 in the normal path):
            // clear CLAUDECODE to prevent recursive agent-spawning,
            // and set AGENT_ALLOWED_PATHS anchored to CWD.
            let mut env = resolved.env_overrides.clone();
            env.remove("CLAUDECODE");
            env.remove("AGENT_ALLOWED_PATHS");
            env.insert("CLAUDECODE".to_string(), String::new());
            let allowed_paths = compute_allowed_paths(&cwd, &[]);
            env.insert("AGENT_ALLOWED_PATHS".to_string(), allowed_paths);

            let agent_opts = AgentOpts {
                slug: slug_for_agent.to_string(),
                worktree_path: cwd,
                prompt: String::new(), // no system prompt in ephemeral mode
                directive: resolved.directive.clone(),
                log_file: None, // ephemeral: no log file
                env,
                session_name: resolved.dispatch_id.clone(),
                dispatch_id: resolved.dispatch_id.clone(),
                agent_invocation: agent_invocation_from_metadata(&agent_metadata),
                sandbox: false,
                inline: true,
                max_turns: turns,
                max_budget_usd: budget,
                stdin_content: Some(stdin_content),
                ephemeral: true,
                timeout: opts.timeout,
            };

            let handle = match self.executor.spawn(&agent_opts).await {
                Ok(h) => h,
                Err(e) => {
                    let tmp_record = self.make_tmp_record(&resolved, opts, resolver.name());
                    resolver.on_cleanup(&tmp_record, self.config, None).await;
                    return Err(e);
                }
            };
            // Cleanup resolver state on success (ephemeral has no registry record to reference)
            let tmp_record = self.make_tmp_record(&resolved, opts, resolver.name());
            resolver.on_cleanup(&tmp_record, self.config, None).await;

            if opts.json {
                let status = match handle.inline_exit_code {
                    Some(0) => Status::Done,
                    Some(_) => Status::Failed,
                    None => Status::Running,
                };
                let pr_urls: Vec<&str> = effective_pr_url.iter().map(String::as_str).collect();
                let envelope = RunOutputV1 {
                    schema_version: SCHEMA_VERSION,
                    kind: "dispatch",
                    data: DispatchEnvelope {
                        dispatch_id: &resolved.dispatch_id,
                        task_slug: resolved.task_slug.as_deref(),
                        branch: &resolved.branch,
                        session: &handle.session,
                        directive: resolved.directive.as_str(),
                        worktree_path: agent_opts.worktree_path.to_string_lossy().into_owned(),
                        worktree_policy: worktree_policy.as_str(),
                        status: status.as_str(),
                        resolver: resolver.name(),
                        pr_urls,
                        log_file: None,
                        agent_provider: &agent_metadata.provider,
                        agent_session_id: agent_metadata.session_id.map(|id| id.to_string()),
                        agent_transcript_cwd: agent_metadata
                            .transcript_cwd
                            .as_ref()
                            .map(|p| p.to_string_lossy().into_owned()),
                        resume_of_dispatch_id: agent_metadata.resume_of_dispatch_id.as_deref(),
                        agent_capabilities: agent_metadata.capabilities.as_ref(),
                        is_dry_run: false,
                        inline_exit_code: handle.inline_exit_code,
                        dispatched_at: Utc::now(),
                    },
                };
                println!("{}", terminal_safe_json_pretty(&envelope)?);
            }

            return Ok(DispatchOutcome {
                id: resolved.dispatch_id.clone(),
                session: handle.session.clone(),
                inline_exit_code: handle.inline_exit_code,
            });
        }

        // 5. Ensure worktree — policy-aware routing
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let meta = discover_meta(&cwd).await;

        let workspace_root = dispatch_cfg
            .resolved_meta_workspace_root(self.config.config_dir.as_deref())
            .ok()
            .or_else(|| meta.as_ref().map(|m| m.workspace_root.clone()))
            .unwrap_or_else(|| cwd.clone());
        // Use resolver-discovered KB root when available (e.g. multi-KB discovery),
        // falling back to workspace_root for resolvers that don't set it.
        let kb_root = resolved.kb_root.as_deref().unwrap_or(&workspace_root);

        // Resolve target repos early so all policy arms can use them.
        // This mirrors the Branch arm's repo selection logic but runs before
        // worktree creation so Current/None/Document policies still carry
        // repo context through to work unit resolution and meta preamble.
        let base_repos = resolve_base_repos(
            opts,
            effective_pr_url.as_deref(),
            &workspace_root,
            meta.as_ref(),
            dispatch_cfg,
        )
        .await;
        let is_meta = meta.is_some();

        let (worktree_path, wt_created, wt_is_meta, repos_for_context) = match resume_agent_metadata
            .as_ref()
        {
            Some(resume_metadata) => {
                let transcript_cwd = resume_metadata
                    .transcript_cwd
                    .clone()
                    .expect("validated resume metadata must include transcript cwd");
                let resume_meta = discover_meta(&transcript_cwd).await;
                // Ensure GITKB_WORKSPACE is populated so providers and the
                // final env block use the correct workspace identity for the
                // resumed session.  The task resolver already sets this for
                // task dispatches; this covers template/prompt resumes where
                // the resolver does not populate env_overrides.
                resolved
                    .env_overrides
                    .entry("GITKB_WORKSPACE".to_string())
                    .or_insert_with(|| crate::dispatch::sanitize_slashes(&resolved.branch));
                (
                    transcript_cwd,
                    false,
                    resume_meta.is_some(),
                    base_repos.clone(),
                )
            }
            None => match worktree_policy {
                WorktreePolicy::Current => {
                    // Run in CWD. No worktree creation, no document resolution.
                    (cwd.clone(), false, is_meta, base_repos.clone())
                }
                WorktreePolicy::None => {
                    // Run in canonical repo root. No worktree creation.
                    (workspace_root.clone(), false, is_meta, base_repos.clone())
                }
                WorktreePolicy::Document => {
                    // Resolve CWD from document location.
                    let slug =
                        document_policy_slug(&resolved, &effective_params).map(str::to_string);
                    match slug.as_deref() {
                        Some(slug) => {
                            if let Err(e) = validate_document_policy_slug(slug) {
                                let tmp_record =
                                    self.make_tmp_record(&resolved, opts, resolver.name());
                                resolver.on_cleanup(&tmp_record, self.config, None).await;
                                return Err(e);
                            }
                            let worktree_base = dispatch_cfg.resolved_worktree_base();
                            match resolve_document_workspace(
                                slug,
                                kb_root,
                                &worktree_base,
                                &workspace_root,
                            )
                            .await
                            {
                                Ok(Some(doc_ws)) => {
                                    // Set GITKB_WORKSPACE for the document's branch
                                    resolved.env_overrides.insert(
                                        "GITKB_WORKSPACE".to_string(),
                                        doc_ws.workspace_branch.clone(),
                                    );
                                    info!(
                                        slug = %display_text(slug),
                                        cwd = %display_text(&doc_ws.cwd.display().to_string()),
                                        workspace_branch = %display_text(&doc_ws.workspace_branch),
                                        "document policy: resolved workspace"
                                    );
                                    (doc_ws.cwd, false, is_meta, base_repos.clone())
                                }
                                Ok(None) => {
                                    // Auto-checkout to main, use workspace_root
                                    if let Err(e) = auto_checkout_to_main(slug, kb_root).await {
                                        warn!(
                                            slug = %display_text(slug),
                                            error = %display_text(&e.to_string()),
                                            "auto-checkout failed (non-fatal)"
                                        );
                                    }
                                    resolved
                                        .env_overrides
                                        .insert("GITKB_WORKSPACE".to_string(), "main".to_string());
                                    (workspace_root.clone(), false, is_meta, base_repos.clone())
                                }
                                Err(e) => {
                                    let tmp_record =
                                        self.make_tmp_record(&resolved, opts, resolver.name());
                                    resolver.on_cleanup(&tmp_record, self.config, None).await;
                                    return Err(e);
                                }
                            }
                        }
                        None => {
                            let tmp_record = self.make_tmp_record(&resolved, opts, resolver.name());
                            resolver.on_cleanup(&tmp_record, self.config, None).await;
                            anyhow::bail!(
                                "worktree: document requires a task or slug parameter to resolve \
                                 the document workspace (set --param task=<slug> or use a task dispatch)"
                            );
                        }
                    }
                }
                WorktreePolicy::Branch => {
                    // Current behavior: create/reuse worktree by branch name.
                    // Repo selection was already computed in base_repos above.
                    let worktree_base = dispatch_cfg.resolved_worktree_base();
                    let repo_refs: Vec<&str> = base_repos.iter().map(|s| s.as_str()).collect();
                    let wt_opts = WorktreeOpts {
                        worktree_base: &worktree_base,
                        repos: repo_refs,
                        branch: &resolved.branch,
                        meta_workspace_root: &workspace_root,
                        kb_root,
                        force: opts.force,
                    };
                    let wt_result = match ensure_worktree(&wt_opts, self.registry).await {
                        Ok(r) => r,
                        Err(e) => {
                            let tmp_record = self.make_tmp_record(&resolved, opts, resolver.name());
                            resolver.on_cleanup(&tmp_record, self.config, None).await;
                            return Err(e);
                        }
                    };
                    (
                        wt_result.path,
                        wt_result.created,
                        wt_result.is_meta,
                        base_repos,
                    )
                }
            },
        };

        // 5b. Load per-project .dispatch/env (after worktree exists, before env setup)
        let project_env = if dispatch_cfg.project_env {
            let env_path = worktree_path.join(".dispatch").join("env");
            if env_path.is_file() {
                // Canonicalize both paths and verify env_path is within the worktree
                // to prevent symlink-based path traversal attacks.
                let wt_canon = match std::fs::canonicalize(&worktree_path) {
                    Ok(p) => p,
                    Err(e) => {
                        let tmp_record = self.make_tmp_record(&resolved, opts, resolver.name());
                        if wt_created {
                            rollback_worktree(wt_is_meta, &worktree_path, &workspace_root).await;
                        }
                        resolver.on_cleanup(&tmp_record, self.config, None).await;
                        return Err(anyhow::anyhow!(
                            "failed to canonicalize worktree path: {}",
                            e
                        ));
                    }
                };
                let env_canon = match std::fs::canonicalize(&env_path) {
                    Ok(p) => p,
                    Err(e) => {
                        let tmp_record = self.make_tmp_record(&resolved, opts, resolver.name());
                        if wt_created {
                            rollback_worktree(wt_is_meta, &worktree_path, &workspace_root).await;
                        }
                        resolver.on_cleanup(&tmp_record, self.config, None).await;
                        return Err(anyhow::anyhow!("failed to canonicalize env path: {}", e));
                    }
                };
                if !env_canon.starts_with(&wt_canon) {
                    let tmp_record = self.make_tmp_record(&resolved, opts, resolver.name());
                    if wt_created {
                        rollback_worktree(wt_is_meta, &worktree_path, &workspace_root).await;
                    }
                    resolver.on_cleanup(&tmp_record, self.config, None).await;
                    return Err(anyhow::anyhow!(
                        ".dispatch/env path escapes worktree: {}",
                        display_text(&env_canon.display().to_string())
                    ));
                }
                match atc_core::project_env::parse_env_file(&env_canon) {
                    Ok(penv) => {
                        tracing::debug!(
                            path = %display_text(&env_path.display().to_string()),
                            count = penv.len(),
                            "loaded project env"
                        );
                        penv
                    }
                    Err(e) => {
                        let tmp_record = self.make_tmp_record(&resolved, opts, resolver.name());
                        if wt_created {
                            rollback_worktree(wt_is_meta, &worktree_path, &workspace_root).await;
                        }
                        resolver.on_cleanup(&tmp_record, self.config, None).await;
                        return Err(e);
                    }
                }
            } else {
                std::collections::HashMap::new()
            }
        } else {
            std::collections::HashMap::new()
        };

        // 6. Set up environment
        // Merge order: project env → resolver env → GH_TOKEN default → provider env.
        // Security invariants (AGENT_ALLOWED_PATHS, CLAUDECODE) are asserted
        // unconditionally *after* all merging in step 8, so no source can override them.
        let mut env = project_env;
        env.extend(resolved.env_overrides.clone());

        // 7. Setup log file
        let log_dir = dispatch_cfg.resolved_log_dir();
        let log_file = log_dir.join(format!("{}.jsonl", resolved.dispatch_id));

        // 7b. Run context providers (non-fatal errors logged, dispatch continues)
        let mut providers =
            atc_core::providers::providers_for_directive(self.config, &resolved.directive);

        // Unconditional pr-context: if `pr` param exists or --pr-url is set,
        // ensure pr-context provider runs regardless of directive config.
        if (effective_params.contains_key("pr") || effective_pr_url.is_some())
            && !providers.iter().any(|p| p.name() == "pr-context")
        {
            if let Some(p) = atc_core::providers::make_provider("pr-context") {
                providers.insert(0, p);
            }
        }

        // For template dispatches, render the system prompt from the directive config
        // (components, template_path, or template_inline). The rendered template becomes
        // stdin/user prompt.
        let mut rendered_prompt = if resolved.is_template {
            let slug_for_prompt = resolved.task_slug.as_deref().unwrap_or(&resolved.branch);
            atc_core::prompt_engine::render_prompt(
                &resolved.directive,
                slug_for_prompt,
                self.config,
                "",
                Some(&worktree_path),
            )
            .await
            .with_context(|| {
                format!(
                    "failed to render system prompt for template dispatch on directive '{}'",
                    resolved.directive.as_str()
                )
            })?
        } else {
            resolved.system_prompt.clone()
        };

        let agent_metadata = resume_agent_metadata.clone().unwrap_or_else(|| {
            claude_agent_provider().durable_session_metadata(worktree_path.clone())
        });
        let reserved_before_spawn = agent_metadata.resume_of_dispatch_id.is_some();
        let now = Utc::now();
        let mut record = build_dispatch_record(
            &resolved,
            opts,
            input,
            resolver.name(),
            effective_pr_url.as_deref(),
            &worktree_path,
            &session_name,
            &log_file,
            &agent_metadata,
            now,
        );
        info!(
            id = %display_text(&resolved.dispatch_id),
            agent_provider = %display_text(&record.agent_provider),
            agent_session_id = record.agent_session_id.map(|id| display_text(&id.to_string())),
            resume_of_dispatch_id = record.resume_of_dispatch_id.as_deref().map(display_text),
            agent_resume = record.resume_of_dispatch_id.is_some(),
            "registered agent session metadata"
        );

        if reserved_before_spawn {
            if let Err(e) = self
                .registry
                .insert_resume_reservation(&record, opts.force)
                .await
            {
                let tmp_record = self.make_tmp_record(&resolved, opts, resolver.name());
                if wt_created {
                    rollback_worktree(wt_is_meta, &worktree_path, &workspace_root).await;
                }
                resolver.on_cleanup(&tmp_record, self.config, None).await;
                return Err(e);
            }
        }

        // GH_TOKEN resolution and diagnostics happen after resume reservation
        // so a rejected concurrent resume does not shell out or leave orphan
        // per-dispatch files for a dispatch that never starts.
        if let Err(e) = tokio::fs::create_dir_all(&log_dir).await {
            if reserved_before_spawn {
                self.mark_resume_reservation_failed(&mut record, "log directory error")
                    .await;
                resolver
                    .on_cleanup(&record, self.config, Some(self.registry))
                    .await;
            } else {
                let tmp_record = self.make_tmp_record(&resolved, opts, resolver.name());
                resolver.on_cleanup(&tmp_record, self.config, None).await;
            }
            if wt_created {
                rollback_worktree(wt_is_meta, &worktree_path, &workspace_root).await;
            }
            return Err(e.into());
        }

        if !env.contains_key("GH_TOKEN") {
            match resolve_gh_token().await {
                Ok(token) => {
                    env.insert("GH_TOKEN".to_string(), token);
                }
                Err(e) => {
                    warn!(
                        error = %display_text(&e.to_string()),
                        "could not resolve GH_TOKEN (non-fatal)"
                    );
                }
            }
        }
        let gh_token_present = env.contains_key("GH_TOKEN") && !env["GH_TOKEN"].is_empty();
        write_diag_file(&log_dir, &resolved.dispatch_id, gh_token_present).await;

        if !providers.is_empty() {
            // Compute the KB workspace for providers. Document policy sets it in
            // env_overrides during step 5; for None/Current we must derive it here
            // since the final env block (step 8) hasn't run yet.
            let kb_workspace = match worktree_policy {
                WorktreePolicy::Document | WorktreePolicy::Branch => {
                    resolved.env_overrides.get("GITKB_WORKSPACE").cloned()
                }
                WorktreePolicy::None => Some("main".to_string()),
                WorktreePolicy::Current => {
                    Some(crate::dispatch::sanitize_slashes(&resolved.branch))
                }
            };
            let dispatch_ctx = atc_core::providers::DispatchContext {
                dispatch_id: resolved.dispatch_id.clone(),
                task_slug: resolved.task_slug.clone(),
                branch: resolved.branch.clone(),
                worktree_path: worktree_path.clone(),
                directive: resolved.directive.clone(),
                pr_url: effective_pr_url.clone(),
                params: effective_params.clone(),
                kb_root: kb_root.to_path_buf(),
                log_dir: log_dir.clone(),
                config: std::sync::Arc::new(self.config.clone()),
                comment_id: comment_id.clone(),
                comment_type: comment_type.clone(),
                kb_workspace,
            };

            let provider_output =
                atc_core::providers::run_providers(&providers, &dispatch_ctx).await;

            // Apply template_vars to rendered prompt AND template_body. Provider
            // vars were rendered as deferred placeholders (__ATC_DEFER_<var>__)
            // by the template engine; replace those now. Also replace raw
            // {{var}} for backward compatibility with non-template dispatches.
            for (key, value) in &provider_output.template_vars {
                let deferred = atc_core::prompt_engine::deferred_placeholder(key);
                rendered_prompt = rendered_prompt.replace(&deferred, value);
                // Backward compat: also replace raw {{key}} (e.g. component-assembled prompts)
                let raw_placeholder = format!("{{{{{}}}}}", key);
                rendered_prompt = rendered_prompt.replace(&raw_placeholder, value);
            }

            // Substitute deferred placeholders in template_body too — for
            // template dispatches, provider vars like {{prefetch}} appear in the
            // rendered template body which becomes stdin/user prompt.
            if let Some(ref mut body) = resolved.template_body {
                for (key, value) in &provider_output.template_vars {
                    let deferred = atc_core::prompt_engine::deferred_placeholder(key);
                    *body = body.replace(&deferred, value);
                    let raw_placeholder = format!("{{{{{}}}}}", key);
                    *body = body.replace(&raw_placeholder, value);
                }
            }

            // Write provider output files to worktree
            for (rel_path, content) in &provider_output.files {
                // Reject absolute or parent-traversal paths to prevent writes outside worktree
                if rel_path.is_absolute()
                    || rel_path
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    warn!(
                        path = %display_text(&rel_path.display().to_string()),
                        "skipping provider output file with unsafe path"
                    );
                    continue;
                }
                let abs_path = worktree_path.join(rel_path);
                if let Some(parent) = abs_path.parent() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }
                if let Err(e) = tokio::fs::write(&abs_path, content).await {
                    warn!(
                        path = %display_text(&abs_path.display().to_string()),
                        error = %display_text(&e.to_string()),
                        "failed to write provider output file (non-fatal)"
                    );
                }
            }

            // Merge provider env vars
            env.extend(provider_output.env);

            // Prepend preamble sections to the rendered prompt
            if !provider_output.preamble_sections.is_empty() {
                let preamble = provider_output.preamble_sections.join("\n\n---\n\n");
                rendered_prompt = format!("{}\n\n---\n\n{}", preamble, rendered_prompt);
            }
        }

        // 7c. Post-process: pipeline-level builtins and meta context injection
        //
        // {{worktree}} and {{default_branch}} are resolved here as fallbacks.
        // Providers may have already substituted them in step 7b (e.g. rebase
        // exports default_branch). This pass catches any remaining placeholders
        // so templates work regardless of which providers are configured.
        {
            let wt_path_str = worktree_path.to_string_lossy();
            let deferred_wt = atc_core::prompt_engine::deferred_placeholder("worktree");
            let raw_wt = "{{worktree}}";

            rendered_prompt = rendered_prompt.replace(raw_wt, &wt_path_str);
            rendered_prompt = rendered_prompt.replace(&deferred_wt, &wt_path_str);

            if let Some(ref mut body) = resolved.template_body {
                *body = body.replace(raw_wt, &wt_path_str);
                *body = body.replace(&deferred_wt, &wt_path_str);
            }

            // Fallback default_branch resolution — if the rebase provider didn't
            // run (directive config omits it), resolve from git so templates that
            // reference {{default_branch}} still work.
            let deferred_db = atc_core::prompt_engine::deferred_placeholder("default_branch");
            if rendered_prompt.contains(&deferred_db)
                || resolved
                    .template_body
                    .as_deref()
                    .is_some_and(|b| b.contains(&deferred_db))
            {
                let default_branch =
                    atc_core::providers::rebase::resolve_default_branch(&worktree_path).await;
                let raw_db = "{{default_branch}}";

                rendered_prompt = rendered_prompt.replace(&deferred_db, &default_branch);
                rendered_prompt = rendered_prompt.replace(raw_db, &default_branch);

                if let Some(ref mut body) = resolved.template_body {
                    *body = body.replace(&deferred_db, &default_branch);
                    *body = body.replace(raw_db, &default_branch);
                }
            }

            // Inject meta workspace context for meta worktrees
            if wt_is_meta && !repos_for_context.is_empty() {
                let repos_display = repos_for_context.join("`, `");
                let context_line = format!(
                    "\n**Meta worktree context:** The target repo(s): `{}` within the worktree at `{}`.\n",
                    repos_display, wt_path_str
                );
                rendered_prompt.push_str(&context_line);
            }
        }

        // 8. Assert security invariants — these MUST come after all env merging
        // (resolver, project, provider) so no source can override them.

        // Strip security-invariant keys that may have been injected by
        // project env, resolver env, or provider env before we compute them.
        env.remove("AGENT_ALLOWED_PATHS");
        env.remove("CLAUDECODE");
        env.remove("GITKB_ROOT");

        // AGENT_ALLOWED_PATHS: always compute the worktree-anchored base paths.
        // Use the resolver-validated `kb_root` (never env["GITKB_ROOT"]) so that
        // a malicious `.dispatch/env` cannot expand the sandbox by setting
        // GITKB_ROOT to an arbitrary path outside the worktree.
        {
            let extra_paths: Vec<String> = (kb_root != worktree_path.as_path())
                .then(|| kb_root.to_string_lossy().into_owned())
                .into_iter()
                .collect();
            let allowed_paths = compute_allowed_paths(&worktree_path, &extra_paths);
            env.insert("AGENT_ALLOWED_PATHS".to_string(), allowed_paths);
        }

        // GITKB_ROOT: always set so the agent can use git-kb commands
        // (show, search, graph, log, board, etc.) regardless of dispatch type.
        // Re-assert from the resolver-validated kb_root so that project env
        // or provider env cannot redirect git-kb to an arbitrary path.
        env.insert(
            "GITKB_ROOT".to_string(),
            kb_root.to_string_lossy().into_owned(),
        );

        // GITKB_WORKSPACE per policy — re-assert after all env merging so that
        // provider env cannot override the policy-derived workspace identity.
        match worktree_policy {
            WorktreePolicy::None => {
                env.insert("GITKB_WORKSPACE".to_string(), "main".to_string());
            }
            WorktreePolicy::Current => {
                // Reuse the branch already resolved by the template resolver
                // instead of spawning another git subprocess.
                env.insert(
                    "GITKB_WORKSPACE".to_string(),
                    crate::dispatch::sanitize_slashes(&resolved.branch),
                );
            }
            WorktreePolicy::Document => {
                // Re-assert the workspace that was resolved in step 5 routing.
                // Provider env (merged in step 7b) could have overwritten it.
                if let Some(ws) = resolved.env_overrides.get("GITKB_WORKSPACE") {
                    env.insert("GITKB_WORKSPACE".to_string(), ws.clone());
                }
            }
            WorktreePolicy::Branch => {
                // Branch policy: GITKB_WORKSPACE is set by the task resolver or
                // derived from the worktree branch name. Re-assert from resolved
                // env_overrides if present.
                if let Some(ws) = resolved.env_overrides.get("GITKB_WORKSPACE") {
                    env.insert("GITKB_WORKSPACE".to_string(), ws.clone());
                }
            }
        }

        // CLAUDECODE: always clear to prevent recursive agent-spawning.
        env.insert("CLAUDECODE".to_string(), String::new());

        // 9. Build agent opts and registry record
        let slug_for_agent = resolved.task_slug.as_deref().unwrap_or(&resolved.branch);
        // For non-task dispatches (prompt/template), provide the resolved system
        // prompt as stdin content so the executor doesn't call `git kb show`.
        let stdin_content = if resolved.task_slug.is_some() {
            None // task dispatches: executor fetches from git-kb
        } else if let Some(ref template_body) = resolved.template_body {
            // Template dispatches: rendered template as user prompt / stdin
            Some(template_body.clone())
        } else {
            // Non-task, non-template dispatches (raw prompts): short context marker
            Some(format!(
                "Non-task dispatch ({}). All instructions are in the system prompt.",
                resolved.branch
            ))
        };
        let agent_opts = AgentOpts {
            slug: slug_for_agent.to_string(),
            worktree_path: worktree_path.clone(),
            prompt: rendered_prompt,
            directive: resolved.directive.clone(),
            log_file: Some(log_file.clone()),
            env,
            session_name: session_name.clone(),
            dispatch_id: resolved.dispatch_id.clone(),
            agent_invocation: agent_invocation_from_metadata(&agent_metadata),
            sandbox: dispatch_cfg.sandbox,
            inline: opts.inline,
            max_turns: turns,
            max_budget_usd: budget,
            stdin_content,
            ephemeral: opts.ephemeral,
            timeout: opts.timeout,
        };

        let handle = match self.executor.spawn(&agent_opts).await {
            Ok(h) => h,
            Err(e) => {
                if reserved_before_spawn {
                    self.mark_resume_reservation_failed(&mut record, "spawn error")
                        .await;
                    resolver
                        .on_cleanup(&record, self.config, Some(self.registry))
                        .await;
                } else {
                    let tmp_record = self.make_tmp_record(&resolved, opts, resolver.name());
                    resolver.on_cleanup(&tmp_record, self.config, None).await;
                }
                if wt_created {
                    rollback_worktree(wt_is_meta, &worktree_path, &workspace_root).await;
                }
                return Err(e);
            }
        };

        let status = match handle.inline_exit_code {
            Some(0) => Status::Done,
            Some(_) => Status::Failed,
            None => Status::Running,
        };
        record.session = handle.session.clone();
        record.terminal_locator =
            dispatch_terminal_locator(opts, &handle.session, &worktree_path, Utc::now());
        record.status = status;
        record.updated_at = Utc::now();

        if reserved_before_spawn {
            record.work_unit_id = resolve_dispatch_work_unit(
                self.registry,
                resolved.task_slug.as_deref(),
                &resolved.branch,
                &effective_params,
                &repos_for_context,
            )
            .await;
            if let Some(ref work_unit_id) = record.work_unit_id {
                if let Err(e) = self
                    .registry
                    .update_dispatch_work_unit(&record.id, Some(work_unit_id))
                    .await
                {
                    warn!(
                        id = %display_text(&record.id),
                        work_unit_id = %display_text(work_unit_id),
                        error = %display_text(&e.to_string()),
                        "failed to attach work unit to resume dispatch (non-fatal)"
                    );
                    record.work_unit_id = None;
                }
            }
            if let Err(e) = self.registry.update_status(&record.id, status).await {
                warn!(
                    id = %display_text(&resolved.dispatch_id),
                    error = %display_text(&e.to_string()),
                    "resume reservation status update failed"
                );
                return Err(e);
            }
            if let Err(e) = self
                .registry
                .update_session_locator(
                    &record.id,
                    &record.session,
                    record.terminal_locator.as_ref(),
                )
                .await
            {
                warn!(
                    id = %display_text(&resolved.dispatch_id),
                    error = %display_text(&e.to_string()),
                    "resume reservation terminal locator update failed"
                );
                return Err(e);
            }
        } else {
            record.work_unit_id = resolve_dispatch_work_unit(
                self.registry,
                resolved.task_slug.as_deref(),
                &resolved.branch,
                &effective_params,
                &repos_for_context,
            )
            .await;
            if let Err(e) = self.registry.insert(&record).await {
                warn!(
                    id = %display_text(&resolved.dispatch_id),
                    error = %display_text(&e.to_string()),
                    "registry insert failed; killing orphan session"
                );
                let session_killed = crate::kb::kill_tmux_session(&handle.session).await;
                if session_killed {
                    resolver
                        .on_cleanup(&record, self.config, Some(self.registry))
                        .await;
                } else {
                    warn!(
                        id = %display_text(&resolved.dispatch_id),
                        session = %display_text(&handle.session),
                        "tmux kill inconclusive after registry insert failure; skipping on_cleanup to avoid orphaned agent"
                    );
                }
                return Err(e);
            }
        }

        // "Agent starting" PR comment — actionable context for humans
        if matches!(
            resolved.directive,
            Directive::ReviewFix | Directive::PrComments
        ) {
            if let Some(ref url) = effective_pr_url {
                let comment = render_pr_start_comment(
                    resolved.directive.as_str(),
                    resolved.task_slug.as_deref(),
                    &resolved.branch,
                    &worktree_path.to_string_lossy(),
                    &handle.session,
                );
                post_pr_comment(url, &comment).await;
            }
        }

        let outcome = DispatchOutcome {
            id: resolved.dispatch_id.clone(),
            session: handle.session.clone(),
            inline_exit_code: handle.inline_exit_code,
        };

        if opts.json {
            // Stable v1 envelope. Mirrors the human-readable confirmation but
            // adds machine-parseable fields (status, dispatched_at, log_file)
            // so consumers can wire up follow-on actions (e.g. "Run with ATC"
            // citation insertion) without scraping text.
            let pr_urls: Vec<&str> = record.pr_urls.iter().map(String::as_str).collect();
            let envelope = RunOutputV1 {
                schema_version: SCHEMA_VERSION,
                kind: "dispatch",
                data: DispatchEnvelope {
                    dispatch_id: &resolved.dispatch_id,
                    task_slug: resolved.task_slug.as_deref(),
                    branch: &resolved.branch,
                    session: &handle.session,
                    directive: resolved.directive.as_str(),
                    worktree_path: worktree_path.to_string_lossy().into_owned(),
                    worktree_policy: worktree_policy.as_str(),
                    status: status.as_str(),
                    resolver: resolver.name(),
                    pr_urls,
                    log_file: Some(log_file.to_string_lossy().into_owned()),
                    agent_provider: &record.agent_provider,
                    agent_session_id: record.agent_session_id.map(|id| id.to_string()),
                    agent_transcript_cwd: record
                        .agent_transcript_cwd
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    resume_of_dispatch_id: record.resume_of_dispatch_id.as_deref(),
                    agent_capabilities: record.agent_capabilities.as_ref(),
                    is_dry_run: false,
                    inline_exit_code: handle.inline_exit_code,
                    dispatched_at: now,
                },
            };
            println!("{}", terminal_safe_json_pretty(&envelope)?);
        } else {
            print_dispatch_confirmation(
                resolved.task_slug.as_deref(),
                &resolved.directive,
                &resolved.dispatch_id,
                &resolved.branch,
                &worktree_path,
                &handle.session,
                &log_file,
                resolver.name(),
                worktree_policy,
                repos_for_context.first().map(String::as_str),
            );
        }

        if let Some(exit_code) = handle.inline_exit_code {
            info!(
                resolver = resolver.name(),
                session = %handle.session,
                exit_code,
                "dispatch complete (inline)"
            );
        } else {
            info!(
                resolver = resolver.name(),
                session = %handle.session,
                "dispatch started (tmux)"
            );
        }

        Ok(outcome)
    }

    /// Find the first resolver that can handle the given input.
    async fn find_resolver(&self, input: &str) -> Result<&dyn InputResolver> {
        for resolver in &self.resolvers {
            if resolver.can_resolve(input, self.config).await {
                return Ok(resolver.as_ref());
            }
        }
        anyhow::bail!(
            "no resolver can handle the provided input. \
             Check that resolvers are enabled in [resolvers] config."
        );
    }

    async fn resolve_resume_metadata(
        &self,
        resume_target: Option<&str>,
        force: bool,
    ) -> Result<Option<AgentSessionMetadata>> {
        let Some(resume_target) = resume_target else {
            return Ok(None);
        };

        let source = crate::resolve::resolve_record(self.registry, resume_target)
            .await
            .with_context(|| {
                format!(
                    "failed to resolve resume target '{}'",
                    display_text(resume_target)
                )
            })?;
        let source_id = display_text(&source.id);
        let source_provider = display_text(&source.agent_provider);

        anyhow::ensure!(
            source.agent_provider == CLAUDE_AGENT_PROVIDER,
            "cannot resume dispatch {}: provider '{}' is not supported by this slice",
            source_id,
            source_provider
        );

        let capabilities = source.agent_capabilities.ok_or_else(|| {
            anyhow::anyhow!(
                "cannot resume dispatch {}: provider '{}' did not record capabilities",
                source_id,
                source_provider
            )
        })?;
        anyhow::ensure!(
            capabilities.supports_resume_by_session_id,
            "cannot resume dispatch {}: provider '{}' does not support resume by session id",
            source_id,
            source_provider
        );

        let session_id = source.agent_session_id.ok_or_else(|| {
            anyhow::anyhow!(
                "cannot resume dispatch {}: missing agent_session_id",
                source_id
            )
        })?;

        let transcript_cwd = self.validate_resume_transcript_cwd(&source)?;

        if !force {
            if let Some(conflict) = self
                .registry
                .find_active_by_agent_session(&source.agent_provider, session_id)
                .await?
            {
                anyhow::bail!(
                    "cannot resume dispatch {}: provider session {} is already active in dispatch {} (status {}). Use --force to override.",
                    source_id,
                    display_text(&session_id.to_string()),
                    display_text(&conflict.id),
                    display_text(&conflict.status.to_string())
                );
            }
        }

        Ok(Some(claude_agent_provider().resume_session_metadata(
            transcript_cwd,
            session_id,
            source.id,
        )))
    }

    async fn mark_resume_reservation_failed(&self, record: &mut DispatchRecord, reason: &str) {
        if let Err(update_err) = self
            .registry
            .update_status(&record.id, Status::Failed)
            .await
        {
            warn!(
                id = %display_text(&record.id),
                reason = %display_text(reason),
                error = %display_text(&update_err.to_string()),
                "failed to mark resume reservation failed"
            );
        }
        if let Err(update_err) = self
            .registry
            .update_session_locator(&record.id, "", None)
            .await
        {
            warn!(
                id = %display_text(&record.id),
                reason = %display_text(reason),
                error = %display_text(&update_err.to_string()),
                "failed to clear failed resume reservation terminal locator"
            );
        }
        record.status = Status::Failed;
        record.session.clear();
        record.terminal_locator = None;
        record.updated_at = Utc::now();
    }

    fn validate_resume_transcript_cwd(&self, source: &DispatchRecord) -> Result<PathBuf> {
        let source_id = display_text(&source.id);
        let stored_transcript_cwd = source.agent_transcript_cwd.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "cannot resume dispatch {}: missing agent_transcript_cwd",
                source_id
            )
        })?;
        let transcript_cwd =
            canonicalize_existing_dir(stored_transcript_cwd, &source.id, "transcript cwd")?;
        let source_worktree =
            canonicalize_existing_dir(&source.worktree_path, &source.id, "source worktree_path")?;
        let transcript_cwd_display = display_text(&transcript_cwd.display().to_string());
        let source_worktree_display = display_text(&source_worktree.display().to_string());

        anyhow::ensure!(
            transcript_cwd == source_worktree,
            "cannot resume dispatch {}: transcript cwd '{}' does not match source worktree_path '{}'",
            source_id,
            transcript_cwd_display,
            source_worktree_display
        );

        anyhow::ensure!(
            !transcript_cwd.as_os_str().is_empty()
                && transcript_cwd.parent().is_some()
                && !is_home_dir(&transcript_cwd)
                && transcript_cwd
                    .file_name()
                    .is_some_and(|name| name != ".worktrees"),
            "cannot resume dispatch {}: unsafe transcript cwd '{}'",
            source_id,
            transcript_cwd_display
        );

        let dispatch_cfg = &self.config.dispatch;
        let meta_workspace_root = dispatch_cfg
            .resolved_meta_workspace_root(self.config.config_dir.as_deref())
            .ok()
            .and_then(|path| canonical_if_exists(&path));
        let worktree_base = canonical_if_exists(&dispatch_cfg.resolved_worktree_base());

        let under_meta_workspace = meta_workspace_root
            .as_ref()
            .is_some_and(|root| transcript_cwd.starts_with(root));
        let under_worktree_base = worktree_base
            .as_ref()
            .is_some_and(|base| transcript_cwd.starts_with(base) && transcript_cwd != *base);

        anyhow::ensure!(
            under_meta_workspace || under_worktree_base,
            "cannot resume dispatch {}: transcript cwd '{}' is not under an ATC workspace or worktree root",
            source_id,
            transcript_cwd_display
        );

        Ok(transcript_cwd)
    }

    /// Build a temporary DispatchRecord for resolver cleanup before registry insertion.
    fn make_tmp_record(
        &self,
        resolved: &atc_core::resolver::ResolvedInput,
        opts: &RunOpts,
        resolver_name: &str,
    ) -> DispatchRecord {
        let now = Utc::now();
        DispatchRecord {
            id: resolved.dispatch_id.clone(),
            task_slug: resolved.task_slug.clone(),
            branch: resolved.branch.clone(),
            worktree_path: PathBuf::new(),
            session: String::new(),
            log_file: PathBuf::new(),
            status: Status::Failed,
            directive: resolved.directive.clone(),
            retries: opts.retries,
            resolver: resolver_name.to_string(),
            pr_urls: opts.pr_url.iter().cloned().collect(),
            no_worktree: opts.no_worktree,
            original_input: None,
            checks: HealthChecks::default(),
            kb_root: resolved.kb_root.clone(),
            cost_usd: None,
            num_turns: None,
            duration_ms: None,
            artifacts: None,
            work_unit_id: None,
            agent_provider: claude_agent_provider()
                .ephemeral_session_metadata()
                .provider,
            agent_session_id: None,
            agent_transcript_cwd: None,
            resume_of_dispatch_id: None,
            agent_capabilities: None,
            terminal_locator: None,
            dispatched_at: now,
            updated_at: now,
        }
    }

    /// Execute a dry-run: print config and return without dispatching.
    #[allow(clippy::too_many_arguments)]
    fn dry_run(
        &self,
        resolved: &atc_core::resolver::ResolvedInput,
        pr_url: Option<&str>,
        budget: f64,
        turns: u32,
        resolver_name: &str,
        providers: &[&str],
        ephemeral: bool,
        worktree_policy: WorktreePolicy,
        repos: &[String],
        workspace_root: &Path,
        json: bool,
        resume_metadata: Option<&AgentSessionMetadata>,
    ) -> Result<DispatchOutcome> {
        let primary_repo = repos.first().map(String::as_str);
        let (policy_label, mut resolved_path, mut hint) = describe_worktree(
            self.config,
            &resolved.branch,
            primary_repo,
            worktree_policy,
            workspace_root,
        );
        if let Some(metadata) = resume_metadata {
            if let Some(transcript_cwd) = &metadata.transcript_cwd {
                resolved_path = transcript_cwd.clone();
                hint = Some(
                    "resume uses the source dispatch transcript cwd instead of creating a new worktree"
                        .to_string(),
                );
            }
        }

        if json {
            let agent_metadata = resume_metadata
                .cloned()
                .unwrap_or_else(|| claude_agent_provider().ephemeral_session_metadata());
            // Dry-run JSON mirrors the success envelope so consumers parse a
            // single shape: `is_dry_run = true` and `status = "preview"` are
            // the discriminators. `dispatch_id` is still populated (the
            // resolver produced one) so consumers can correlate previews
            // with subsequent real runs if they choose to.
            let pr_urls: Vec<&str> = pr_url.into_iter().collect();
            let envelope = RunOutputV1 {
                schema_version: SCHEMA_VERSION,
                kind: "dispatch",
                data: DispatchEnvelope {
                    dispatch_id: &resolved.dispatch_id,
                    task_slug: resolved.task_slug.as_deref(),
                    branch: &resolved.branch,
                    session: &resolved.dispatch_id,
                    directive: resolved.directive.as_str(),
                    worktree_path: resolved_path.to_string_lossy().into_owned(),
                    worktree_policy: worktree_policy.as_str(),
                    status: "preview",
                    resolver: resolver_name,
                    pr_urls,
                    log_file: None,
                    agent_provider: &agent_metadata.provider,
                    agent_session_id: agent_metadata.session_id.map(|id| id.to_string()),
                    agent_transcript_cwd: agent_metadata
                        .transcript_cwd
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    resume_of_dispatch_id: agent_metadata.resume_of_dispatch_id.as_deref(),
                    agent_capabilities: agent_metadata.capabilities.as_ref(),
                    is_dry_run: true,
                    inline_exit_code: None,
                    dispatched_at: Utc::now(),
                },
            };
            println!("{}", terminal_safe_json_pretty(&envelope)?);
            // Suppress unused-arg warnings while keeping the same arg list as
            // the human path (so future fields can flow into both branches).
            let _ = (budget, turns, providers, hint, ephemeral, policy_label);
            return Ok(DispatchOutcome {
                id: resolved.dispatch_id.clone(),
                session: resolved.dispatch_id.clone(),
                inline_exit_code: Some(0),
            });
        }

        if ephemeral {
            println!("=== DRY RUN (ephemeral) ===");
        } else {
            println!("=== DRY RUN ===");
        }
        println!(
            "Input:       {}",
            display_text(resolved.task_slug.as_deref().unwrap_or(&resolved.branch))
        );
        println!("Resolver:    {}", display_text(resolver_name));
        println!("Directive:   {}", resolved.directive.as_str());
        println!("Branch:      {}", display_text(&resolved.branch));
        println!("ID:          {}", display_text(&resolved.dispatch_id));
        println!("Budget:      ${:.2}", budget);
        println!("Turns:       {}", turns);
        println!("PR URL:      {}", display_text(pr_url.unwrap_or("(none)")));
        if let Some(metadata) = resume_metadata {
            if let Some(resume_of) = metadata.resume_of_dispatch_id.as_deref() {
                println!("Resume:     {}", display_text(resume_of));
            }
            if let Some(session_id) = metadata.session_id {
                println!("Agent ID:   {}", display_text(&session_id.to_string()));
            }
        }

        println!(
            "Worktree:    {} ({})",
            worktree_policy.as_str(),
            policy_label
        );
        println!(
            "Path:        {}",
            display_text(&resolved_path.display().to_string())
        );
        if !repos.is_empty() {
            println!("Repo:        {}", display_text(&repos.join(", ")));
        }
        if let Some(h) = hint {
            println!("Hint:        {}", display_text(&h));
        }

        if ephemeral {
            println!("Providers:   (skipped — ephemeral)");
            println!("System:      (skipped — ephemeral)");
        } else {
            println!("Providers:   {:?}", providers);
        }
        if resolved.is_template {
            println!("Template:    yes (system prompt from directive config)");
        }
        Ok(DispatchOutcome {
            id: resolved.dispatch_id.clone(),
            session: resolved.dispatch_id.clone(),
            inline_exit_code: Some(0),
        })
    }
}

/// Human-readable description of what a worktree policy actually does.
/// Single source of truth shared by dry-run preview and post-dispatch confirmation.
fn worktree_policy_label(policy: WorktreePolicy) -> &'static str {
    match policy {
        WorktreePolicy::Branch => "create or reuse a worktree by branch name",
        WorktreePolicy::Document => "use the document workspace path",
        WorktreePolicy::None => "no worktree — run in the canonical repo root",
        WorktreePolicy::Current => "no worktree — run in the current working directory",
    }
}

/// Describe the resolved worktree location for a dispatch policy.
///
/// Returns `(policy_label, resolved_path, optional_hint)`. The path is
/// computed using the same logic that `ensure_worktree` uses for `Branch`
/// policy, but does not actually create anything on disk.
///
/// `primary_repo` should be the same value the dispatch path resolves via
/// [`resolve_base_repos`] so the dry-run preview matches execution.
/// `workspace_root` is the resolved workspace root from the dispatch path
/// (config → meta discovery → cwd fallback) so `Document`/`None` previews
/// match what dispatch actually uses.
fn describe_worktree(
    config: &AtcConfig,
    branch: &str,
    primary_repo: Option<&str>,
    policy: WorktreePolicy,
    workspace_root: &Path,
) -> (&'static str, PathBuf, Option<String>) {
    use crate::dispatch::sanitize_slashes;

    let label = worktree_policy_label(policy);
    match policy {
        WorktreePolicy::Branch => {
            let worktree_base = config.dispatch.resolved_worktree_base();
            let sanitized = sanitize_slashes(branch);
            let path = match primary_repo {
                Some(r) => worktree_base.join(&sanitized).join(r),
                None => worktree_base.join(&sanitized),
            };
            let hint = if config.dispatch.worktree_base.is_none() {
                Some(format!(
                    "worktree_base is unset; using default {}. Set [dispatch] worktree_base in .atc/config.toml to override.",
                    worktree_base.display()
                ))
            } else {
                None
            };
            (label, path, hint)
        }
        // Without resolving the document we can only show the workspace root.
        WorktreePolicy::Document => (label, workspace_root.to_path_buf(), None),
        WorktreePolicy::None => (label, workspace_root.to_path_buf(), None),
        WorktreePolicy::Current => {
            let process_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            (label, process_cwd, None)
        }
    }
}

/// Resolve the target repo list using the same fallback chain the main
/// dispatch path uses. Shared between dry-run preview and actual dispatch
/// so the printed `Repo:` and worktree path stay aligned with execution.
///
/// Order: explicit `--repo` args → `DISPATCH_WORKTREE_REPO` env var → PR URL
/// resolution → config (`dispatch.repo`) → meta discovery.
async fn resolve_base_repos(
    opts: &RunOpts,
    effective_pr_url: Option<&str>,
    workspace_root: &Path,
    meta: Option<&MetaDiscovery>,
    dispatch_cfg: &atc_core::config::DispatchConfig,
) -> Vec<String> {
    if !opts.repos.is_empty() {
        return opts.repos.clone();
    }
    if let Ok(env_repo) = std::env::var("DISPATCH_WORKTREE_REPO") {
        let env_repo = env_repo.trim();
        if !env_repo.is_empty() {
            info!(
                repo = %display_text(env_repo),
                "using DISPATCH_WORKTREE_REPO env var"
            );
            return vec![env_repo.to_string()];
        }
        // Blank env var behaves like "unset" so fallback chain continues.
    }
    if let Some(pr_url) = effective_pr_url {
        match resolve_pr_repo_path(pr_url, workspace_root).await {
            Ok(Some(r)) => {
                info!(
                    pr_url = %display_text(pr_url),
                    repo = %display_text(&r),
                    "resolved PR repo to local path"
                );
                return vec![r];
            }
            Ok(None) => {
                info!(
                    pr_url = %display_text(pr_url),
                    "could not resolve PR repo to local path, using config/discovery fallback"
                );
            }
            Err(e) => {
                warn!(
                    pr_url = %display_text(pr_url),
                    error = %display_text(&e.to_string()),
                    "PR repo resolution failed, using config/discovery fallback"
                );
            }
        }
    }
    match dispatch_cfg.resolved_repo() {
        Some(r) => vec![r.to_string()],
        None => meta.map(|m| vec![m.repo.clone()]).unwrap_or_default(),
    }
}

/// Instantiate a resolver by name for use in stop/cleanup/close/retry.
/// Delegates to the centralized factory in `resolvers::make_resolver`.
pub fn resolver_by_name(name: &str) -> Option<Box<dyn InputResolver>> {
    crate::resolvers::make_resolver(name)
}

/// Print post-dispatch confirmation block.
#[allow(clippy::too_many_arguments)]
fn build_dispatch_confirmation(
    task_slug: Option<&str>,
    directive: &Directive,
    id: &str,
    branch: &str,
    worktree_path: &Path,
    session: &str,
    log_file: &Path,
    resolver_name: &str,
    worktree_policy: WorktreePolicy,
    primary_repo: Option<&str>,
) -> String {
    let slug_display = task_slug.unwrap_or("(none)");
    let policy_label = worktree_policy_label(worktree_policy);
    let mut lines = Vec::new();
    lines.push(format!("Dispatched: {}", display_text(slug_display)));
    lines.push(format!("  Resolver:  {}", display_text(resolver_name)));
    lines.push(format!("  Directive: {}", directive.as_str()));
    lines.push(format!("  ID:        {}", display_text(id)));
    lines.push(format!("  Branch:    {}", display_text(branch)));
    lines.push(format!(
        "  Worktree:  {} ({})",
        worktree_policy.as_str(),
        policy_label
    ));
    lines.push(format!(
        "  Path:      {}",
        display_text(&worktree_path.display().to_string())
    ));
    if let Some(repo) = primary_repo {
        lines.push(format!("  Repo:      {}", display_text(repo)));
    }
    lines.push(format!("  Session:   {}", display_text(session)));
    lines.push(format!(
        "  Log:       {}",
        display_text(&log_file.display().to_string())
    ));
    lines.push(String::new());
    lines.push("  Next steps:".to_string());
    if let Some(slug) = task_slug {
        lines.push(format!("    atc logs {}", shell_display_arg(slug)));
    }
    lines.push(format!("    atc watch --id {}", shell_display_arg(id)));
    lines.push(format!(
        "    atc watch --id {} --pretty",
        shell_display_arg(id)
    ));
    lines.push("    atc status --flat --json".to_string());
    if let Some(slug) = task_slug {
        lines.push(format!(
            "    atc redirect {} [message]",
            shell_display_arg(slug)
        ));
    }
    lines.join("\n")
}

/// Print post-dispatch confirmation block.
#[allow(clippy::too_many_arguments)]
fn print_dispatch_confirmation(
    task_slug: Option<&str>,
    directive: &Directive,
    id: &str,
    branch: &str,
    worktree_path: &Path,
    session: &str,
    log_file: &Path,
    resolver_name: &str,
    worktree_policy: WorktreePolicy,
    primary_repo: Option<&str>,
) {
    println!(
        "{}",
        build_dispatch_confirmation(
            task_slug,
            directive,
            id,
            branch,
            worktree_path,
            session,
            log_file,
            resolver_name,
            worktree_policy,
            primary_repo
        )
    );
}

/// Embedded template for PR start comments. Editable without touching Rust code.
const PR_START_COMMENT_TEMPLATE: &str = include_str!("../defaults/pr-start-comment.md");

/// Render the PR start comment from the embedded template.
fn render_pr_start_comment(
    directive: &str,
    task: Option<&str>,
    branch: &str,
    worktree: &str,
    session: &str,
) -> String {
    let mut out = PR_START_COMMENT_TEMPLATE.to_string();
    out = out.replace("{{directive}}", directive);
    out = out.replace("{{branch}}", branch);
    out = out.replace("{{worktree}}", worktree);
    out = out.replace("{{session}}", session);
    // Handle conditional {{#if task}} block
    if let Some(task) = task {
        out = out.replace("{{#if task}}", "");
        out = out.replace("{{/if}}", "");
        out = out.replace("{{task}}", task);
    } else {
        // Remove the entire {{#if task}}...{{/if}} block
        while let Some(start) = out.find("{{#if task}}") {
            if let Some(rel_end) = out[start..].find("{{/if}}") {
                let end = start + rel_end;
                out.replace_range(start..end + "{{/if}}".len(), "");
            } else {
                break;
            }
        }
    }
    // Clean up any double blank lines
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    out.trim().to_string()
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
            warn!(
                pr_url = %display_text(pr_url),
                "gh pr comment failed (non-fatal)"
            );
        }
        Err(e) => {
            warn!(
                pr_url = %display_text(pr_url),
                error = %display_text(&e.to_string()),
                "gh pr comment failed (non-fatal)"
            );
        }
        _ => {}
    }
}

/// Resolve or create a work unit for this dispatch.
///
/// Priority: task slug > branch > create new (orphan).
/// Only active work units are matched — non-active ones cause a new unit.
async fn resolve_work_unit(
    registry: &dyn Registry,
    task_slug: Option<&str>,
    branch: &str,
    params: &std::collections::HashMap<String, String>,
    repos: &[String],
) -> Result<WorkUnit> {
    // 1. Try task slug (from resolved input or --param task=...)
    let effective_task = task_slug.or_else(|| params.get("task").map(|s| s.as_str()));
    if let Some(slug) = effective_task {
        if let Some(unit) = registry.find_work_unit_by_task(slug).await? {
            return Ok(unit);
        }
    }

    // 2. Try branch
    if let Some(mut unit) = registry.find_work_unit_by_branch(branch).await? {
        // Promote branch-only unit to task-associated if we now know the task slug.
        // Without this, find_work_unit_by_task* lookups would miss this unit.
        if unit.task_slug.is_none() {
            if let Some(slug) = effective_task {
                registry.update_work_unit_task_slug(&unit.id, slug).await?;
                unit.task_slug = Some(slug.to_string());
            }
        }
        return Ok(unit);
    }

    // 3. Create new work unit (INSERT OR IGNORE to handle concurrent races)
    let now = chrono::Utc::now();
    let id = generate_work_unit_id();
    let unit = WorkUnit {
        id,
        task_slug: effective_task.map(|s| s.to_string()),
        branch: Some(branch.to_string()),
        repos: repos.to_vec(),
        pr_urls: Vec::new(),
        status: WorkUnitStatus::Active,
        created_at: now,
        updated_at: now,
    };
    registry.insert_work_unit(&unit).await?;

    // Re-query to return the winning row (ours or a concurrent insert that won the race).
    // The unique partial indexes guarantee at most one active unit per task/branch.
    if let Some(slug) = effective_task {
        if let Some(existing) = registry.find_work_unit_by_task(slug).await? {
            info!(id = %existing.id, task = ?effective_task, branch = %branch, "resolved work unit");
            return Ok(existing);
        }
    }
    if let Some(existing) = registry.find_work_unit_by_branch(branch).await? {
        info!(id = %existing.id, task = ?effective_task, branch = %branch, "resolved work unit");
        return Ok(existing);
    }

    // Shouldn't happen, but fall back to the unit we tried to insert
    info!(id = %unit.id, task = ?effective_task, branch = %branch, "created new work unit");
    Ok(unit)
}

async fn resolve_dispatch_work_unit(
    registry: &dyn Registry,
    task_slug: Option<&str>,
    branch: &str,
    params: &std::collections::HashMap<String, String>,
    repos: &[String],
) -> Option<String> {
    let work_unit = resolve_work_unit(registry, task_slug, branch, params, repos).await;
    let work_unit_id = match &work_unit {
        Ok(wu) => Some(wu.id.clone()),
        Err(e) => {
            warn!(
                error = %display_text(&e.to_string()),
                "work unit resolution failed (non-fatal)"
            );
            None
        }
    };

    if let Ok(ref wu) = work_unit {
        for repo in repos {
            if !wu.repos.contains(repo) {
                if let Err(e) = registry.add_work_unit_repo(&wu.id, repo).await {
                    warn!(
                        error = %display_text(&e.to_string()),
                        "failed to add repo to work unit (non-fatal)"
                    );
                }
            }
        }
    }

    work_unit_id
}

/// Generate a ULID-like ID for work units.
fn generate_work_unit_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ts = chrono::Utc::now().timestamp_millis();
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mix = count.wrapping_mul(0x517cc1b727220a95) ^ (std::process::id() as u64);
    format!("wu-{:013x}-{:08x}", ts, (mix & 0xFFFF_FFFF) as u32)
}

/// Rollback a newly created worktree (without resolver cleanup).
async fn rollback_worktree(is_meta: bool, worktree_path: &Path, workspace_root: &Path) {
    let cmd = if is_meta { "meta" } else { "git" };
    let mut args: Vec<&str> = Vec::new();
    if is_meta {
        args.extend(["git", "worktree", "remove", "--force"]);
    } else {
        args.extend(["worktree", "remove", "--force"]);
    }
    let wt_str = worktree_path.to_string_lossy();
    args.push(&wt_str);
    let timeout = std::time::Duration::from_secs(30);
    match tokio::process::Command::new(cmd)
        .args(&args)
        .current_dir(workspace_root)
        .kill_on_drop(true)
        .spawn()
    {
        Ok(mut child) => match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) if !status.success() => {
                warn!(
                    worktree = %display_text(&worktree_path.display().to_string()),
                    "rollback worktree remove exited with {status}"
                );
            }
            Ok(Err(e)) => {
                warn!(
                    worktree = %display_text(&worktree_path.display().to_string()),
                    error = %display_text(&e.to_string()),
                    "rollback worktree remove failed"
                );
            }
            Err(_) => {
                let _ = child.kill().await;
                warn!(
                    worktree = %display_text(&worktree_path.display().to_string()),
                    "rollback worktree remove timed out"
                );
            }
            _ => {}
        },
        Err(e) => {
            warn!(
                worktree = %display_text(&worktree_path.display().to_string()),
                error = %display_text(&e.to_string()),
                "failed to spawn worktree remove"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved_input_for_slug(task_slug: Option<&str>) -> ResolvedInput {
        ResolvedInput {
            system_prompt: String::new(),
            directive: Directive::Implement,
            task_slug: task_slug.map(str::to_string),
            branch: "test-branch".to_string(),
            dispatch_id: "test-dispatch".to_string(),
            env_overrides: HashMap::new(),
            kb_root: None,
            is_template: false,
            template_body: None,
            max_turns: None,
            worktree_policy: None,
        }
    }

    #[test]
    fn test_document_policy_slug_prefers_task_slug_then_params() {
        let mut params = HashMap::new();
        params.insert("task".to_string(), "tasks/from-param".to_string());
        params.insert("slug".to_string(), "tasks/from-slug".to_string());

        let resolved = resolved_input_for_slug(Some("tasks/from-record"));
        assert_eq!(
            document_policy_slug(&resolved, &params),
            Some("tasks/from-record")
        );

        let resolved = resolved_input_for_slug(None);
        assert_eq!(
            document_policy_slug(&resolved, &params),
            Some("tasks/from-param")
        );

        params.insert("task".to_string(), String::new());
        assert_eq!(
            document_policy_slug(&resolved, &params),
            Some("tasks/from-slug")
        );
    }

    #[test]
    fn test_validate_document_policy_slug_rejects_unsafe_paths() {
        for slug in [
            "../tasks/foo",
            "./tasks/foo",
            "/tmp/tasks/foo",
            "tasks\\foo",
            "tasks/foo\0bar",
        ] {
            let err = validate_document_policy_slug(slug).unwrap_err();
            assert!(
                err.to_string().contains("invalid slug for document policy"),
                "unexpected error for {slug:?}: {err}"
            );
        }
        let hostile = "tasks/bad\x1b[2J\u{202e}gpj/../secret";
        let hostile_err = validate_document_policy_slug(hostile)
            .unwrap_err()
            .to_string();
        assert!(hostile_err.contains("tasks/bad\\x1b[2J\\u{202e}gpj/../secret"));
        assert!(!hostile_err.contains('\x1b'));
        assert!(!hostile_err.contains('\u{202e}'));

        validate_document_policy_slug("tasks/foo").unwrap();
        validate_document_policy_slug("specs/atc-agent-harness-contract").unwrap();
    }

    #[test]
    fn test_canonicalize_existing_dir_uses_real_path_but_escapes_errors() {
        let tempdir = tempfile::tempdir().unwrap();
        let weird_dir = tempdir.path().join("resume-\x1b[2J\u{202e}gpj");
        std::fs::create_dir(&weird_dir).unwrap();

        let canonical =
            canonicalize_existing_dir(&weird_dir, "dispatch-\x1b[31m\u{202e}gpj", "transcript cwd")
                .unwrap();
        assert_eq!(canonical, std::fs::canonicalize(&weird_dir).unwrap());

        let missing = weird_dir.join("missing");
        let err =
            canonicalize_existing_dir(&missing, "dispatch-\x1b[31m\u{202e}gpj", "transcript cwd")
                .unwrap_err()
                .to_string();
        assert!(err.contains("dispatch-\\x1b[31m\\u{202e}gpj"));
        assert!(err.contains("resume-\\x1b[2J\\u{202e}gpj"));
        assert!(!err.contains('\x1b'));
        assert!(!err.contains('\u{202e}'));
    }

    #[test]
    fn test_run_output_v1_dispatch_envelope_shape() {
        // Lock the v1 success envelope so consumers (the GitKB ATC app, scripts)
        // can rely on exact field names. Renaming any of these is a v2 change.
        let capabilities = atc_core::types::claude_agent_capabilities();
        let envelope = RunOutputV1 {
            schema_version: SCHEMA_VERSION,
            kind: "dispatch",
            data: DispatchEnvelope {
                dispatch_id: "tasks--foo@implement@1234567890-0001",
                task_slug: Some("tasks/foo"),
                branch: "tasks--foo",
                session: "tasks--foo@implement@1234567890-0001",
                directive: "implement",
                worktree_path: "/tmp/wt/tasks--foo".to_string(),
                worktree_policy: "branch",
                status: "running",
                resolver: "task",
                pr_urls: vec!["https://github.com/o/r/pull/1"],
                log_file: Some("/tmp/logs/tasks--foo.jsonl".to_string()),
                agent_provider: "claude",
                agent_session_id: Some("00000000-0000-4000-8000-000000000200".to_string()),
                agent_transcript_cwd: Some("/tmp/wt/tasks--foo".to_string()),
                resume_of_dispatch_id: None,
                agent_capabilities: Some(&capabilities),
                is_dry_run: false,
                inline_exit_code: None,
                dispatched_at: chrono::DateTime::parse_from_rfc3339("2026-05-04T12:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            },
        };
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["kind"], "dispatch");
        let data = &json["data"];
        assert_eq!(data["dispatch_id"], "tasks--foo@implement@1234567890-0001");
        assert_eq!(data["task_slug"], "tasks/foo");
        assert_eq!(data["branch"], "tasks--foo");
        assert_eq!(data["session"], "tasks--foo@implement@1234567890-0001");
        assert_eq!(data["directive"], "implement");
        assert_eq!(data["worktree_path"], "/tmp/wt/tasks--foo");
        assert_eq!(data["worktree_policy"], "branch");
        assert_eq!(data["status"], "running");
        assert_eq!(data["resolver"], "task");
        assert_eq!(data["pr_urls"][0], "https://github.com/o/r/pull/1");
        assert_eq!(data["log_file"], "/tmp/logs/tasks--foo.jsonl");
        assert_eq!(data["agent_provider"], "claude");
        assert_eq!(
            data["agent_session_id"],
            "00000000-0000-4000-8000-000000000200"
        );
        assert_eq!(data["agent_transcript_cwd"], "/tmp/wt/tasks--foo");
        assert!(data["resume_of_dispatch_id"].is_null());
        assert_eq!(
            data["agent_capabilities"]["supports_resume_by_session_id"],
            true
        );
        assert!(data.get("agent_capabilities_json").is_none());
        assert_eq!(data["is_dry_run"], false);
        assert!(data["inline_exit_code"].is_null());
        assert_eq!(data["dispatched_at"], "2026-05-04T12:00:00Z");
    }

    #[test]
    fn test_run_output_v1_dry_run_envelope_uses_preview_status() {
        // Dry-run envelopes are the same shape as the success envelope but
        // discriminate via `is_dry_run = true` and `status = "preview"` so
        // consumers don't insert citations for previews.
        let capabilities = atc_core::types::claude_agent_capabilities();
        let envelope = RunOutputV1 {
            schema_version: SCHEMA_VERSION,
            kind: "dispatch",
            data: DispatchEnvelope {
                dispatch_id: "tasks--foo@implement@1234567890-0001",
                task_slug: Some("tasks/foo"),
                branch: "tasks--foo",
                session: "tasks--foo@implement@1234567890-0001",
                directive: "implement",
                worktree_path: "/tmp/wt/tasks--foo".to_string(),
                worktree_policy: "branch",
                status: "preview",
                resolver: "task",
                pr_urls: vec![],
                log_file: None,
                agent_provider: "claude",
                agent_session_id: None,
                agent_transcript_cwd: None,
                resume_of_dispatch_id: None,
                agent_capabilities: Some(&capabilities),
                is_dry_run: true,
                inline_exit_code: None,
                dispatched_at: Utc::now(),
            },
        };
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["data"]["is_dry_run"], true);
        assert_eq!(json["data"]["status"], "preview");
        assert!(json["data"]["log_file"].is_null());
        assert_eq!(
            json["data"]["agent_capabilities"]["supports_resume_by_session_id"],
            true
        );
    }

    #[test]
    fn test_run_output_v1_error_envelope_shape() {
        let err = anyhow::anyhow!("tasks/harmony-9999 not found in KB");
        let envelope = RunOutputV1 {
            schema_version: SCHEMA_VERSION,
            kind: "error",
            data: ErrorEnvelope {
                code: "dispatch_error",
                message: err.to_string(),
            },
        };
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["kind"], "error");
        assert_eq!(json["data"]["code"], "dispatch_error");
        assert!(json["data"]["message"]
            .as_str()
            .unwrap()
            .contains("not found in KB"));
    }

    #[test]
    fn test_emit_run_error_envelope_includes_full_chain() {
        // anyhow chains nested errors with `.context()`. The envelope message
        // must include all causes, joined so a programmatic consumer can show
        // them in the UI without losing the inner reason.
        let inner = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let err = anyhow::Error::new(inner).context("failed to read template");
        let joined = format_error_chain(&err);
        assert!(joined.contains("failed to read template"));
        assert!(joined.contains("no such file"));
    }

    #[test]
    fn test_build_dispatch_confirmation_quotes_and_escapes_next_steps() {
        let task = "tasks/evil; rm -rf /\x1b[2J\x07\u{202e}gpj.exe";
        let id = "sess 123; rm -rf /";
        let out = build_dispatch_confirmation(
            Some(task),
            &Directive::Implement,
            id,
            "branch-\x1b[31m",
            Path::new("/tmp/worktrees/evil\x07"),
            "session-\u{202e}",
            Path::new("/tmp/logs/evil\x1b.jsonl"),
            "task",
            WorktreePolicy::Branch,
            Some("open-source/atc"),
        );

        assert!(!out.contains('\x1b'), "raw ESC in output: {out:?}");
        assert!(!out.contains('\x07'), "raw BEL in output: {out:?}");
        assert!(!out.contains('\u{202e}'), "raw bidi in output: {out:?}");
        assert!(out.contains("\\x1b"));
        assert!(out.contains("\\x07"));
        assert!(out.contains("\\u{202e}"));
        assert!(out.contains("atc logs 'tasks/evil; rm -rf /"));
        assert!(out.contains("atc watch --id 'sess 123; rm -rf /'"));
        assert!(out.contains("atc redirect 'tasks/evil; rm -rf /"));
    }

    #[test]
    fn test_render_pr_start_comment_with_task() {
        let out = render_pr_start_comment(
            "review-fix",
            Some("tasks/my-task"),
            "feat/branch",
            "/tmp/wt/branch",
            "sess-123",
        );
        assert!(out.contains("review-fix"), "should contain directive");
        assert!(out.contains("tasks/my-task"), "should contain task");
        assert!(out.contains("feat/branch"), "should contain branch");
        assert!(out.contains("sess-123"), "should contain session");
        assert!(
            out.contains("atc watch --id sess-123"),
            "should have watch cmd"
        );
        // No leftover template syntax
        assert!(!out.contains("{{"), "no template tags remaining: {}", out);
    }

    #[test]
    fn test_render_pr_start_comment_without_task() {
        let out = render_pr_start_comment(
            "pr-comments",
            None,
            "feat/branch",
            "/tmp/wt/branch",
            "sess-456",
        );
        assert!(out.contains("pr-comments"), "should contain directive");
        assert!(!out.contains("Task:"), "task line should be removed");
        // No triple blank lines
        assert!(!out.contains("\n\n\n"), "no triple blank lines");
        // No leftover template syntax
        assert!(!out.contains("{{"), "no template tags remaining: {}", out);
    }

    #[test]
    fn test_worktree_policy_label_covers_all_variants() {
        // Every variant must produce a non-empty label so the dry-run / confirmation
        // output never falls back to a missing description.
        for policy in [
            WorktreePolicy::Branch,
            WorktreePolicy::Document,
            WorktreePolicy::None,
            WorktreePolicy::Current,
        ] {
            let label = worktree_policy_label(policy);
            assert!(!label.is_empty(), "label empty for {:?}", policy);
        }
        // Spot-check a couple to lock in the user-visible wording.
        assert_eq!(
            worktree_policy_label(WorktreePolicy::Branch),
            "create or reuse a worktree by branch name"
        );
        assert_eq!(
            worktree_policy_label(WorktreePolicy::Current),
            "no worktree — run in the current working directory"
        );
    }

    #[test]
    fn test_describe_worktree_branch_uses_primary_repo() {
        // Branch policy must place the worktree under <base>/<branch>/<repo>
        // when a primary repo is supplied — otherwise just <base>/<branch>.
        // This locks the dry-run preview path to the same shape as dispatch.
        let mut config = AtcConfig::default();
        config.dispatch.worktree_base = Some(PathBuf::from("/tmp/wt"));
        let workspace_root = PathBuf::from("/tmp/ws");

        // Slashes in the branch name are sanitized to `--` to match
        // ensure_worktree's on-disk layout (so dry-run paths match reality).
        let (_, with_repo, _) = describe_worktree(
            &config,
            "feat/x",
            Some("open-source/atc"),
            WorktreePolicy::Branch,
            &workspace_root,
        );
        assert_eq!(
            with_repo,
            PathBuf::from("/tmp/wt/feat--x/open-source/atc"),
            "primary_repo must be appended to the branch path"
        );

        let (_, without_repo, _) = describe_worktree(
            &config,
            "feat/x",
            None,
            WorktreePolicy::Branch,
            &workspace_root,
        );
        assert_eq!(
            without_repo,
            PathBuf::from("/tmp/wt/feat--x"),
            "missing primary_repo must yield the bare branch path"
        );
    }

    #[test]
    fn test_describe_worktree_document_and_none_use_workspace_root() {
        // Document and None policies must echo the workspace_root supplied by
        // the caller (which dispatch resolves via the meta-discovery fallback
        // chain), not a separately-derived workspace_root. This keeps dry-run
        // previews aligned with the path dispatch actually uses.
        let config = AtcConfig::default();
        let workspace_root = PathBuf::from("/tmp/meta-ws");

        let (_, doc_path, _) = describe_worktree(
            &config,
            "feat/x",
            None,
            WorktreePolicy::Document,
            &workspace_root,
        );
        assert_eq!(doc_path, workspace_root);

        let (_, none_path, _) = describe_worktree(
            &config,
            "feat/x",
            None,
            WorktreePolicy::None,
            &workspace_root,
        );
        assert_eq!(none_path, workspace_root);
    }
}
