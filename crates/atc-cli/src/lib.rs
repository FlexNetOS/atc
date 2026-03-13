use anyhow::Result;
use atc_core::config::AtcConfig;
use atc_core::executor::AgentExecutor;
use atc_core::registry::Registry;
use std::sync::Arc;

pub use args::{Args, Commands};

pub mod dispatch;

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
        Dispatch {
            /// Task slug (e.g. tasks/gitkb-42)
            slug: String,
            /// Mode (implement, research, kb-update, review-fix, pr-comments, refine, create-task)
            #[arg(value_name = "MODE", value_parser = clap::value_parser!(Mode))]
            mode: Option<Mode>,
            /// Run inline (synchronous, no tmux). Auto-enabled when ATC_CI=true.
            #[arg(long)]
            inline: bool,
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
        Commands::Dispatch { mode, slug, inline } => {
            let is_inline = *inline
                || std::env::var("ATC_CI")
                    .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
                    .unwrap_or(false);
            dispatch::dispatch(
                config,
                registry.as_ref(),
                executor.as_ref(),
                mode.clone(),
                slug,
                is_inline,
            )
            .await
        }
    }
}
