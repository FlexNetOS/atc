use clap::Parser;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

use atc_core::config::AtcConfig;
use atc_core::executor::ClaudeExecutor;
use atc_core::registry::SqliteRegistry;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize structured logging. Controlled by RUST_LOG env var.
    // Default: info-level for atc crates, warn for everything else.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("atc_core=info,atc_cli=info,warn")),
        )
        .with_target(true)
        .init();

    let cli = atc_cli::Args::parse();

    let config = AtcConfig::load(cli.config.as_deref())?;
    let db_path = config.registry.resolved_path();
    let registry = Arc::new(SqliteRegistry::open(&db_path).await?);
    let executor = Arc::new(ClaudeExecutor {
        claude_bin: config.dispatch.resolved_claude_bin(),
    });

    atc_cli::run(&cli, &config, registry, executor).await
}
