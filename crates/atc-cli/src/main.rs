use clap::Parser;
use std::sync::Arc;

use atc_core::config::AtcConfig;
use atc_core::executor::ClaudeExecutor;
use atc_core::registry::SqliteRegistry;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = atc_cli::Args::parse();

    let config = AtcConfig::load(cli.config.as_deref())?;
    let db_path = config.registry.resolved_path();
    let registry = Arc::new(SqliteRegistry::open(&db_path).await?);
    let executor = Arc::new(ClaudeExecutor::default());

    atc_cli::run(&cli, registry, executor).await
}
