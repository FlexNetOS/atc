use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::executor::AgentExecutor;
use atc_core::registry::Registry;
use atc_core::types::DispatchOpts;
use std::sync::Arc;

pub use args::{Args, Commands};

pub mod cleanup;
pub mod close;
pub mod dispatch;
pub mod health;
pub mod info;
pub mod logs;
pub mod redirect;
pub mod resolve;
pub mod retry;
pub mod status;
pub mod stop;
pub mod subprocess;

mod args {
    use atc_core::types::Mode;
    use clap::{Parser, Subcommand};

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
        /// Dispatch an agent to work on a task
        #[command(name = "run", alias = "dispatch")]
        Dispatch {
            /// Task slug (e.g. tasks/gitkb-42)
            slug: String,
            /// Mode (implement, research, kb-update, review-fix, pr-comments, refine, create-task, close)
            #[arg(value_name = "MODE", value_parser = clap::value_parser!(Mode))]
            mode: Option<Mode>,
            /// Additional directive passed into prompt rendering
            #[arg(long)]
            directive: Option<String>,
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
    }
}

/// Library entry point for command execution. Used by `harmony-atc-cli` to compose
/// commands via the library rather than re-implementing them.
pub async fn run(
    args: &Args,
    config: &AtcConfig,
    registry: Arc<dyn Registry>,
    executor: Arc<dyn AgentExecutor>,
) -> Result<()> {
    match &args.command {
        Commands::Dispatch {
            mode,
            slug,
            directive,
            pr_url,
            inline,
            force,
            dry_run,
            max_budget_usd,
            max_turns,
        } => {
            let is_inline = *inline
                || std::env::var("ATC_CI")
                    .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
                    .unwrap_or(false);
            let opts = DispatchOpts {
                slug: slug.clone(),
                cli_mode: mode.clone(),
                directive: directive.clone(),
                pr_url: pr_url.clone(),
                inline: is_inline,
                force: *force,
                dry_run: *dry_run,
                max_budget_override: *max_budget_usd,
                max_turns_override: *max_turns,
                retries: 0,
            };
            let outcome =
                dispatch::dispatch(config, registry.as_ref(), executor.as_ref(), &opts).await?;
            if let Some(code) = outcome.inline_exit_code {
                if code != 0 {
                    anyhow::bail!("inline dispatch failed with exit code {code}");
                }
            }
            Ok(())
        }
        Commands::Health { json, all } => health::run_health(config, registry, *json, *all).await,
        Commands::Prompt {
            mode,
            slug,
            directive,
        } => {
            let prompt = atc_core::templates::render_prompt(
                mode,
                slug,
                config,
                directive.as_deref().unwrap_or(""),
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
    }
}
