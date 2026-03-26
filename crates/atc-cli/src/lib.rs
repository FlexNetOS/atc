use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::executor::AgentExecutor;
use atc_core::registry::Registry;
use atc_core::types::RunOpts;
use std::collections::HashMap;
use std::sync::Arc;

pub use args::{Args, Commands};

pub mod cleanup;
pub mod close;
pub mod daemon;
pub mod dispatch;
pub mod enqueue;
pub mod health;
pub mod info;
pub mod init;
pub mod kb;
pub mod logs;
pub mod pipeline;
pub mod post_complete;
pub mod queue_cmd;
pub mod redirect;
pub mod resolve;
pub mod resolvers;
pub mod retry;
pub mod status;
pub mod stop;
pub mod subprocess;
pub mod watch;

mod args {
    use atc_core::types::Directive;
    use clap::{Parser, Subcommand};
    use std::path::PathBuf;

    #[derive(Parser)]
    #[command(name = "atc", about = "Air Traffic Control — agent orchestrator")]
    pub struct Args {
        #[arg(long, global = true)]
        pub config: Option<std::path::PathBuf>,

        #[command(subcommand)]
        pub command: Commands,
    }

    #[derive(Subcommand)]
    pub enum Commands {
        /// Run an agent (direct dispatch, no queue)
        Run {
            /// Input: "task <slug>", template name, or raw prompt string
            input: Vec<String>,
            /// Directive (implement, research, kb-update, review-fix, pr-comments, refine, create-task, close)
            #[arg(long, value_parser = clap::value_parser!(Directive))]
            directive: Option<Directive>,
            /// Key=value pairs for template rendering
            #[arg(long = "param")]
            param: Vec<String>,
            /// PR URL (required for review-fix and pr-comments modes)
            #[arg(long)]
            pr_url: Option<String>,
            /// Run inline (synchronous, no tmux). Auto-enabled when ATC_CI=true.
            #[arg(long)]
            inline: bool,
            /// Force dispatch even if worktree is in use or session exists
            #[arg(long)]
            force: bool,
            /// Preview full dispatch config without launching
            #[arg(long)]
            dry_run: bool,
            /// List available templates
            #[arg(long)]
            list: bool,
            /// Comma-separated directive override
            #[arg(long)]
            directives: Option<String>,
            /// Skip worktree creation (run in current directory)
            #[arg(long)]
            no_worktree: bool,
            /// Override max budget (USD) for this dispatch
            #[arg(long)]
            max_budget_usd: Option<f64>,
            /// Override max turns for this dispatch
            #[arg(long)]
            max_turns: Option<u32>,
        },
        /// Check health of all active dispatches
        Health {
            /// Output as JSON array
            #[arg(long)]
            json: bool,
            /// Include done and failed records
            #[arg(long)]
            all: bool,
            /// Auto-dispatch review-fix for NeedsReview records with PR URLs
            #[arg(long)]
            auto: bool,
        },
        /// Render and print the system prompt for a directive (useful for debugging)
        Prompt {
            /// Directive to render
            #[arg(value_parser = clap::value_parser!(Directive))]
            directive: Directive,
            /// Task slug for {{slug}} interpolation (default: "tasks/example")
            #[arg(long, default_value = "tasks/example")]
            slug: String,
            /// Additional directive text passed into prompt rendering
            #[arg(long = "directive-text")]
            directive_text: Option<String>,
            /// Worktree path for resolving project-level partials
            #[arg(long)]
            worktree_path: Option<PathBuf>,
        },
        /// Mark a task as complete, remove worktree, update git-kb
        Close {
            /// Task slug (e.g. tasks/gitkb-42)
            slug: String,
            /// PR URL to record
            #[arg(long)]
            pr: Option<String>,
        },
        /// Send a message to a running agent's tmux session
        Redirect {
            /// Dispatch ID or task slug
            id: String,
            /// Message to send to the agent
            message: String,
        },
        /// Re-dispatch a failed task with the same directive and config
        Retry {
            /// Dispatch ID or task slug
            id: String,
        },
        /// Show table view of all dispatch records
        #[command(name = "status")]
        StatusCmd {
            /// Filter by status (running, done, failed, needs-review, needs-human, stopped, retrying)
            #[arg(long = "status")]
            status_filter: Option<String>,
            /// Output as JSON array
            #[arg(long)]
            json: bool,
        },
        /// Show detailed info for a single dispatch record
        Info {
            /// Dispatch ID or task slug
            id: String,
        },
        /// Tail the stream-json log for a dispatch
        Logs {
            /// Dispatch ID, task slug, or session name
            arg: String,
            /// Follow log file (like tail -f)
            #[arg(short = 'f', long)]
            follow: bool,
        },
        /// Stop a running dispatch (kill session, mark stopped)
        Stop {
            /// Dispatch ID or task slug
            id: String,
        },
        /// Clean up a dispatch (remove worktree, kill session)
        Cleanup {
            /// Dispatch ID or task slug (omit for --done mode)
            id: Option<String>,
            /// Clean up all Done dispatches
            #[arg(long)]
            done: bool,
        },
        /// Run post-completion pipeline (extract artifacts, update registry, notify)
        PostComplete {
            /// Dispatch ID (default: most recent Running)
            #[arg(long)]
            id: Option<String>,
            /// Exit code from the agent process (inferred from log if not provided)
            #[arg(long)]
            exit_code: Option<i32>,
            /// Path to stream-json log file (resolved from registry if not provided)
            #[arg(long)]
            log: Option<std::path::PathBuf>,
        },
        /// Initialize .atc/ directory from current config
        Init {
            /// Force overwrite existing .atc/ directory
            #[arg(long)]
            force: bool,
        },
        /// Watch running agent sessions and emit structured events
        Watch {
            /// Dispatch ID to watch (default: most recent Running)
            #[arg(long)]
            id: Option<String>,
            /// Watch all running dispatches
            #[arg(long)]
            all_running: bool,
            /// Output format: ndjson or human
            #[arg(long, default_value = "auto")]
            format: String,
            /// Unix socket path for multi-consumer mode
            #[arg(long)]
            socket: Option<std::path::PathBuf>,
        },
        /// Add work to the dispatch queue
        Enqueue {
            /// Input: "task <slug>", template name, or raw prompt
            input: Vec<String>,
            /// Target named queue
            #[arg(long, default_value = "default")]
            queue: String,
            /// Dispatch priority (critical, high, medium, low)
            #[arg(long, default_value = "medium")]
            priority: String,
            /// Override directive/mode for dispatched items
            #[arg(long)]
            mode: Option<String>,
            /// Key=value pairs for template rendering
            #[arg(long = "param")]
            param: Vec<String>,
            /// Delegate selection to kb_ready scoring
            #[arg(long)]
            ready: bool,
            /// Limit for --ready (how many top-scored tasks to enqueue)
            #[arg(long, default_value = "1")]
            limit: u32,
            /// Delegate selection to board query
            #[arg(long)]
            board: bool,
            /// Board filter: status
            #[arg(long = "status")]
            status_filter: Option<String>,
            /// Board filter: only unblocked tasks
            #[arg(long)]
            unblocked: bool,
            /// Board filter: only unassigned tasks
            #[arg(long)]
            unassigned: bool,
            /// Delegate selection to a saved view
            #[arg(long)]
            view: Option<String>,
            /// Read slugs from stdin (one per line)
            #[arg(long)]
            stdin: bool,
        },
        /// View and manage dispatch queues
        Queue {
            #[command(subcommand)]
            action: Option<QueueAction>,
            /// Queue name to inspect
            #[arg(long, default_value = "default")]
            name: String,
        },
        /// Run the continuous dispatch daemon
        Daemon {
            #[command(subcommand)]
            action: Option<DaemonAction>,
            /// Queue(s) to drain
            #[arg(long = "queue")]
            queues: Vec<String>,
            /// Max concurrent dispatches
            #[arg(long)]
            max_concurrent: Option<usize>,
            /// Activate source(s) alongside the drain loop
            #[arg(long = "source")]
            sources: Vec<String>,
            /// Run in background (write PID file)
            #[arg(long)]
            detach: bool,
        },
    }

    #[derive(Subcommand)]
    pub enum QueueAction {
        /// Dispatch all pending items in one shot, then exit
        Drain,
        /// Remove all pending items from the queue
        Clear,
    }

    #[derive(Subcommand)]
    pub enum DaemonAction {
        /// Gracefully stop the running daemon
        Stop,
        /// Show daemon status (uptime, queue depth, active dispatches)
        Status,
    }
}

/// Parse `--param key=value` pairs into a HashMap.
fn parse_params(param_args: &[String]) -> Result<HashMap<String, String>> {
    let mut params = HashMap::new();
    for p in param_args {
        let (k, v) = p.split_once('=').ok_or_else(|| {
            anyhow::anyhow!("invalid --param format: {:?} (expected key=value)", p)
        })?;
        params.insert(k.to_string(), v.to_string());
    }
    Ok(params)
}

/// Library entry point for command execution.
pub async fn run(
    args: &Args,
    config: &AtcConfig,
    registry: Arc<atc_core::registry::SqliteRegistry>,
    executor: Arc<dyn AgentExecutor>,
) -> Result<()> {
    match &args.command {
        Commands::Run {
            input,
            directive,
            param,
            pr_url,
            inline,
            force,
            dry_run,
            list,
            directives,
            no_worktree,
            max_budget_usd,
            max_turns,
        } => {
            // Handle --list
            if *list {
                let templates = resolvers::template::TemplateResolver::list_templates(config);
                if templates.is_empty() {
                    println!("No templates found.");
                } else {
                    println!("Available templates:");
                    for name in &templates {
                        println!("  {name}");
                    }
                }
                return Ok(());
            }

            if input.is_empty() || input.iter().all(|s| s.trim().is_empty()) {
                anyhow::bail!(
                    "input is required: provide a task slug, template name, or prompt string"
                );
            }

            // Parse input: if first word is "task", strip it and route to TaskResolver explicitly
            let (raw_input, force_task) = if input.first().map(|s| s.as_str()) == Some("task") {
                let slug = input[1..].join(" ");
                if slug.is_empty() {
                    anyhow::bail!(
                        "'atc run task' requires a task slug, e.g. 'atc run task tasks/gitkb-42'"
                    );
                }
                (slug, true)
            } else {
                (input.join(" "), false)
            };

            let is_inline = *inline
                || std::env::var("ATC_CI")
                    .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
                    .unwrap_or(false);

            let params = parse_params(param)?;

            let opts = RunOpts {
                input: raw_input.clone(),
                directive: directive.clone(),
                params,
                pr_url: pr_url.clone(),
                inline: is_inline,
                force: *force,
                dry_run: *dry_run,
                directives: directives.clone(),
                no_worktree: *no_worktree,
                max_budget_usd: *max_budget_usd,
                max_turns: *max_turns,
                retries: 0,
                list: false,
            };

            // Build resolver chain
            let all_resolvers = resolvers::build_resolvers(config);
            let resolvers_to_use = if force_task {
                // "task <slug>" explicitly routes to TaskResolver
                all_resolvers
                    .into_iter()
                    .filter(|r| r.name() == "task")
                    .collect()
            } else {
                all_resolvers
            };

            let pipeline = pipeline::DispatchPipeline {
                resolvers: resolvers_to_use,
                config,
                registry: registry.as_ref(),
                executor: executor.as_ref(),
            };

            let outcome = pipeline.execute(&raw_input, &opts).await?;
            if let Some(code) = outcome.inline_exit_code {
                if code != 0 {
                    anyhow::bail!("inline dispatch failed with exit code {code}");
                }
            }
            Ok(())
        }
        Commands::Health { json, all, auto } => {
            health::run_health(
                config,
                registry.clone() as Arc<dyn Registry>,
                executor,
                *json,
                *all,
                *auto,
            )
            .await
        }
        Commands::Prompt {
            directive,
            slug,
            directive_text,
            worktree_path,
        } => {
            let prompt = atc_core::prompt_engine::render_prompt(
                directive,
                slug,
                config,
                directive_text.as_deref().unwrap_or(""),
                worktree_path.as_deref(),
            )
            .await?;
            println!("{prompt}");
            Ok(())
        }
        Commands::Close { slug, pr } => {
            close::run_close(config, registry.as_ref(), slug, pr.as_deref()).await
        }
        Commands::Redirect { id, message } => {
            redirect::run_redirect(registry.as_ref(), id, message).await
        }
        Commands::Retry { id } => {
            retry::run_retry(config, registry.as_ref(), executor.as_ref(), id).await
        }
        Commands::StatusCmd {
            status_filter,
            json,
        } => {
            status::run_status(
                registry.clone() as Arc<dyn Registry>,
                status_filter.clone(),
                *json,
            )
            .await
        }
        Commands::Info { id } => info::run_info(registry.clone() as Arc<dyn Registry>, id).await,
        Commands::Logs { arg, follow } => {
            logs::run_logs(registry.clone() as Arc<dyn Registry>, config, arg, *follow).await
        }
        Commands::Stop { id } => stop::run_stop(config, registry.as_ref(), id).await,
        Commands::Cleanup { id, done } => {
            cleanup::run_cleanup(config, registry.as_ref(), id.as_deref(), *done).await
        }
        Commands::PostComplete { id, exit_code, log } => {
            post_complete::run_post_complete(
                config,
                registry.as_ref(),
                id.as_deref(),
                *exit_code,
                log.clone(),
            )
            .await
        }
        Commands::Init { force } => init::run_init(config, *force).await,
        Commands::Watch {
            id,
            all_running,
            format,
            socket,
        } => {
            watch::run_watch(
                config,
                registry.clone() as Arc<dyn Registry>,
                id.as_deref(),
                *all_running,
                format,
                socket.clone(),
            )
            .await
        }
        Commands::Enqueue {
            input,
            queue,
            priority,
            mode,
            param,
            ready,
            limit,
            board,
            status_filter,
            unblocked,
            unassigned,
            view,
            stdin,
        } => {
            let priority: atc_core::queue::Priority = priority.parse()?;
            let params = parse_params(param)?;
            let opts = enqueue::EnqueueOpts {
                input: input.clone(),
                queue: queue.clone(),
                priority,
                mode: mode.clone(),
                params,
                ready: *ready,
                limit: *limit,
                board: *board,
                status_filter: status_filter.clone(),
                unblocked: *unblocked,
                unassigned: *unassigned,
                view: view.clone(),
                stdin: *stdin,
                enqueued_by: "user".to_string(),
            };
            enqueue::run_enqueue(registry.as_ref(), &opts).await
        }
        Commands::Queue { action, name } => match action {
            Some(args::QueueAction::Drain) => {
                queue_cmd::run_queue_drain(
                    registry.as_ref(),
                    registry.as_ref(),
                    executor.as_ref(),
                    config,
                    name,
                )
                .await
            }
            Some(args::QueueAction::Clear) => {
                queue_cmd::run_queue_clear(registry.as_ref(), name).await
            }
            None => queue_cmd::run_queue_list(registry.as_ref(), name).await,
        },
        Commands::Daemon {
            action,
            queues,
            max_concurrent,
            sources,
            detach,
        } => match action {
            Some(args::DaemonAction::Stop) => daemon::stop_daemon(config),
            Some(args::DaemonAction::Status) => {
                daemon::daemon_status(config, registry.as_ref(), registry.as_ref(), queues).await
            }
            None => {
                let max_concurrent = max_concurrent.unwrap_or(config.daemon.max_concurrent);
                let opts = daemon::DaemonOpts {
                    queues: queues.clone(),
                    max_concurrent,
                    sources: sources.clone(),
                    detach: *detach,
                };
                daemon::run_daemon(registry, executor, config, &opts).await
            }
        },
    }
}
