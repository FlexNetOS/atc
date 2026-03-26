use anyhow::{Context, Result};
use atc_core::config::AtcConfig;
use atc_core::executor::{AgentExecutor, AgentOpts};
use atc_core::registry::Registry;
use atc_core::resolver::InputResolver;
use atc_core::types::{Directive, DispatchOutcome, DispatchRecord, HealthChecks, RunOpts, Status};
use chrono::Utc;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use crate::dispatch::{
    compute_allowed_paths, ensure_worktree, resolve_gh_token, tmux_session_alive,
    validate_branch_name, write_diag_file, WorktreeOpts,
};

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
        // 1. Find first resolver that can handle input
        let resolver = self.find_resolver(input).await?;
        info!(resolver = resolver.name(), "selected resolver");

        // 2. Resolve → ResolvedInput
        let mut resolved = resolver.resolve(input, opts, self.config).await?;

        // 3. Validate
        // PR URL can come from --pr-url or from template --param pr=<url>
        let effective_pr_url = opts
            .pr_url
            .clone()
            .or_else(|| opts.params.get("pr").cloned());
        if matches!(
            resolved.directive,
            Directive::ReviewFix | Directive::PrComments
        ) && effective_pr_url.is_none()
        {
            // Rollback resolver state
            let tmp_record = self.make_tmp_record(&resolved, opts, resolver.name());
            resolver.on_cleanup(&tmp_record, self.config, None).await;
            anyhow::bail!(
                "{} directive requires a PR URL (--pr-url or --param pr=<url>). Cannot dispatch without it.",
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
                "tmux session '{}' already exists. Use --force to override.",
                session_name
            );
        }

        // 4. Dry run — no resolver state was mutated (resolve() skips CAS claim
        //    when dry_run is set), so we can return immediately.
        if opts.dry_run {
            // Compute providers for display
            let mut dry_providers =
                atc_core::providers::providers_for_directive(self.config, &resolved.directive);
            if (opts.params.iter().any(|(k, _)| k == "pr") || effective_pr_url.is_some())
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
            );
        }

        // 5. Ensure worktree (skip if --no-worktree)
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let meta = crate::dispatch::discover_meta(&cwd).await;

        let workspace_root = dispatch_cfg
            .resolved_meta_workspace_root(self.config.config_dir.as_deref())
            .ok()
            .or_else(|| meta.as_ref().map(|m| m.workspace_root.clone()))
            .unwrap_or_else(|| cwd.clone());
        // Use resolver-discovered KB root when available (e.g. multi-KB discovery),
        // falling back to workspace_root for resolvers that don't set it.
        let kb_root = resolved.kb_root.as_deref().unwrap_or(&workspace_root);

        let (worktree_path, wt_created, wt_is_meta) = if opts.no_worktree {
            // Run in current directory, no worktree creation
            (cwd.clone(), false, false)
        } else {
            let repo = match dispatch_cfg.resolved_repo() {
                Some(r) => Some(r.to_string()),
                None => meta.as_ref().map(|m| m.repo.clone()),
            };

            let worktree_base = dispatch_cfg.resolved_worktree_base();
            let wt_opts = WorktreeOpts {
                worktree_base: &worktree_base,
                repo: repo.as_deref(),
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
            (wt_result.path, wt_result.created, wt_result.is_meta)
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
                        env_canon.display()
                    ));
                }
                match atc_core::project_env::parse_env_file(&env_canon) {
                    Ok(penv) => {
                        tracing::debug!(
                            path = %env_path.display(),
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

        // GH_TOKEN resolution (default only if not already set by resolver/project)
        if !env.contains_key("GH_TOKEN") {
            match resolve_gh_token().await {
                Ok(token) => {
                    env.insert("GH_TOKEN".to_string(), token);
                }
                Err(e) => {
                    warn!(error = %e, "could not resolve GH_TOKEN (non-fatal)");
                }
            }
        }

        // 7. Setup log file
        let log_dir = dispatch_cfg.resolved_log_dir();
        if let Err(e) = tokio::fs::create_dir_all(&log_dir).await {
            let tmp_record = self.make_tmp_record(&resolved, opts, resolver.name());
            if wt_created {
                rollback_worktree(wt_is_meta, &worktree_path, &workspace_root).await;
            }
            resolver.on_cleanup(&tmp_record, self.config, None).await;
            return Err(e.into());
        }
        let log_file = log_dir.join(format!("{}.jsonl", resolved.dispatch_id));

        // Write diagnostic file
        let gh_token_present = env.contains_key("GH_TOKEN") && !env["GH_TOKEN"].is_empty();
        write_diag_file(&log_dir, &resolved.dispatch_id, gh_token_present).await;

        // 7b. Run context providers (non-fatal errors logged, dispatch continues)
        let mut providers =
            atc_core::providers::providers_for_directive(self.config, &resolved.directive);

        // Unconditional pr-context: if `pr` param exists or --pr-url is set,
        // ensure pr-context provider runs regardless of directive config.
        if (opts.params.iter().any(|(k, _)| k == "pr") || effective_pr_url.is_some())
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
        if !providers.is_empty() {
            let dispatch_ctx = atc_core::providers::DispatchContext {
                dispatch_id: resolved.dispatch_id.clone(),
                task_slug: resolved.task_slug.clone(),
                branch: resolved.branch.clone(),
                worktree_path: worktree_path.clone(),
                directive: resolved.directive.clone(),
                pr_url: effective_pr_url.clone(),
                params: opts.params.clone(),
                kb_root: kb_root.to_path_buf(),
                log_dir: log_dir.clone(),
                config: std::sync::Arc::new(self.config.clone()),
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
                        path = %rel_path.display(),
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
                        path = %abs_path.display(),
                        error = %e,
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

        // GITKB_ROOT: only set for task dispatches where git-kb is needed.
        // Re-assert from the resolver-validated kb_root so that project env
        // or provider env cannot redirect git-kb to an arbitrary path.
        if resolved.task_slug.is_some() {
            env.insert(
                "GITKB_ROOT".to_string(),
                kb_root.to_string_lossy().into_owned(),
            );
        }

        // CLAUDECODE: always clear to prevent recursive agent-spawning.
        env.insert("CLAUDECODE".to_string(), String::new());

        // 9. Build agent opts and spawn
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
            log_file: log_file.clone(),
            env,
            session_name: session_name.clone(),
            dispatch_id: resolved.dispatch_id.clone(),
            sandbox: dispatch_cfg.sandbox,
            inline: opts.inline,
            max_turns: turns,
            max_budget_usd: budget,
            stdin_content,
        };

        let handle = match self.executor.spawn(&agent_opts).await {
            Ok(h) => h,
            Err(e) => {
                let tmp_record = self.make_tmp_record(&resolved, opts, resolver.name());
                if wt_created {
                    rollback_worktree(wt_is_meta, &worktree_path, &workspace_root).await;
                }
                resolver.on_cleanup(&tmp_record, self.config, None).await;
                return Err(e);
            }
        };

        // 10. Insert registry record
        let status = match handle.inline_exit_code {
            Some(0) => Status::Done,
            Some(_) => Status::Failed,
            None => Status::Running,
        };
        let now = Utc::now();
        let record = DispatchRecord {
            id: resolved.dispatch_id.clone(),
            task_slug: resolved.task_slug.clone(),
            branch: resolved.branch.clone(),
            worktree_path: worktree_path.clone(),
            session: handle.session.clone(),
            log_file: log_file.clone(),
            status,
            directive: resolved.directive.clone(),
            retries: opts.retries,
            resolver: resolver.name().to_string(),
            pr_url: effective_pr_url.clone(),
            no_worktree: opts.no_worktree,
            original_input: Some(input.to_string()),
            checks: HealthChecks::default(),
            kb_root: resolved.kb_root.clone(),
            cost_usd: None,
            num_turns: None,
            duration_ms: None,
            artifacts: None,
            dispatched_at: now,
            updated_at: now,
        };
        if let Err(e) = self.registry.insert(&record).await {
            warn!(id = %resolved.dispatch_id, error = %e, "registry insert failed; killing orphan session");
            let session_killed = crate::kb::kill_tmux_session(&handle.session).await;
            if session_killed {
                resolver
                    .on_cleanup(&record, self.config, Some(self.registry))
                    .await;
            } else {
                warn!(
                    id = %resolved.dispatch_id,
                    session = %handle.session,
                    "tmux kill inconclusive after registry insert failure; skipping on_cleanup to avoid orphaned agent"
                );
            }
            return Err(e);
        }

        // "Agent starting" PR comment
        if matches!(
            resolved.directive,
            Directive::ReviewFix | Directive::PrComments
        ) {
            if let Some(ref url) = effective_pr_url {
                let comment = format!(
                    "\u{1f916} Agent starting: {} on {}",
                    resolved.directive.as_str(),
                    resolved.branch
                );
                post_pr_comment(url, &comment).await;
            }
        }

        let outcome = DispatchOutcome {
            id: resolved.dispatch_id.clone(),
            session: handle.session.clone(),
            inline_exit_code: handle.inline_exit_code,
        };

        // Post-dispatch confirmation
        print_dispatch_confirmation(
            resolved.task_slug.as_deref(),
            &resolved.directive,
            &resolved.dispatch_id,
            &resolved.branch,
            &worktree_path,
            &handle.session,
            &log_file,
            resolver.name(),
        );

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
            pr_url: opts.pr_url.clone(),
            no_worktree: opts.no_worktree,
            original_input: None,
            checks: HealthChecks::default(),
            kb_root: resolved.kb_root.clone(),
            cost_usd: None,
            num_turns: None,
            duration_ms: None,
            artifacts: None,
            dispatched_at: now,
            updated_at: now,
        }
    }

    /// Execute a dry-run: print config and return without dispatching.
    fn dry_run(
        &self,
        resolved: &atc_core::resolver::ResolvedInput,
        pr_url: Option<&str>,
        budget: f64,
        turns: u32,
        resolver_name: &str,
        providers: &[&str],
    ) -> Result<DispatchOutcome> {
        println!("=== DRY RUN ===");
        println!(
            "Input:       {}",
            resolved.task_slug.as_deref().unwrap_or(&resolved.branch)
        );
        println!("Resolver:    {}", resolver_name);
        println!("Directive:   {}", resolved.directive.as_str());
        println!("Branch:      {}", resolved.branch);
        println!("ID:          {}", resolved.dispatch_id);
        println!("Budget:      ${:.2}", budget);
        println!("Turns:       {}", turns);
        println!("PR URL:      {}", pr_url.unwrap_or("(none)"));
        println!("Providers:   {:?}", providers);
        if resolved.is_template {
            println!("Template:    yes (system prompt from directive components)");
        }
        Ok(DispatchOutcome {
            id: resolved.dispatch_id.clone(),
            session: resolved.dispatch_id.clone(),
            inline_exit_code: Some(0),
        })
    }
}

/// Instantiate a resolver by name for use in stop/cleanup/close/retry.
/// Delegates to the centralized factory in `resolvers::make_resolver`.
pub fn resolver_by_name(name: &str) -> Option<Box<dyn InputResolver>> {
    crate::resolvers::make_resolver(name)
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
) {
    let slug_display = task_slug.unwrap_or("(none)");
    println!("Dispatched: {}", slug_display);
    println!("  Resolver:  {}", resolver_name);
    println!("  Directive: {}", directive.as_str());
    println!("  ID:        {}", id);
    println!("  Branch:    {}", branch);
    println!("  Worktree:  {}", worktree_path.display());
    println!("  Session:   {}", session);
    println!("  Log:       {}", log_file.display());
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
                    worktree = %worktree_path.display(),
                    "rollback worktree remove exited with {status}"
                );
            }
            Ok(Err(e)) => {
                warn!(
                    worktree = %worktree_path.display(),
                    "rollback worktree remove failed: {e}"
                );
            }
            Err(_) => {
                let _ = child.kill().await;
                warn!(
                    worktree = %worktree_path.display(),
                    "rollback worktree remove timed out"
                );
            }
            _ => {}
        },
        Err(e) => {
            warn!(
                worktree = %worktree_path.display(),
                "failed to spawn worktree remove: {e}"
            );
        }
    }
}
