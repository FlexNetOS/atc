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
pub mod dispatch;
pub mod health;
pub mod info;
pub mod kb;
pub mod logs;
pub mod pipeline;
pub mod post_complete;
pub mod redirect;
pub mod resolve;
pub mod resolvers;
pub mod retry;
pub mod status;
pub mod stop;
pub mod subprocess;
pub mod watch;

mod args {
    use atc_core::types::Mode;
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
        /// Run an agent
        Run {
            /// Input: "task <slug>", template name, or raw prompt string
            input: Vec<String>,
            /// Mode (implement, research, kb-update, review-fix, pr-comments, refine, create-task, close)
            #[arg(long, value_parser = clap::value_parser!(Mode))]
            mode: Option<Mode>,
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
        /// Render and print the system prompt for a mode (useful for debugging)
        Prompt {
            /// Mode to render
            #[arg(value_parser = clap::value_parser!(Mode))]
            mode: Mode,
            /// Task slug for {{slug}} interpolation (default: "tasks/example")
            #[arg(long, default_value = "tasks/example")]
            slug: String,
            /// Additional directive passed into prompt rendering
            #[arg(long)]
            directive: Option<String>,
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
        /// Re-dispatch a failed task with the same mode and config
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
    registry: Arc<dyn Registry>,
    executor: Arc<dyn AgentExecutor>,
) -> Result<()> {
    match &args.command {
        Commands::Run {
            input,
            mode,
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
                mode: mode.clone(),
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
            health::run_health(config, registry, executor, *json, *all, *auto).await
        }
        Commands::Prompt {
            mode,
            slug,
            directive,
            worktree_path,
        } => {
            let prompt = atc_core::prompt_engine::render_prompt(
                mode,
                slug,
                config,
                directive.as_deref().unwrap_or(""),
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
        } => status::run_status(registry, status_filter.clone(), *json).await,
        Commands::Info { id } => info::run_info(registry, id).await,
        Commands::Logs { arg, follow } => logs::run_logs(registry, config, arg, *follow).await,
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
        Commands::Watch {
            id,
            all_running,
            format,
            socket,
        } => {
            watch::run_watch(
                config,
                registry.clone(),
                id.as_deref(),
                *all_running,
                format,
                socket.clone(),
            )
            .await
        }
    }
}
