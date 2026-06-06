use clap::Parser;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

use atc_core::config::AtcConfig;
use atc_core::executor::{AgentExecutor, ClaudeExecutor, RemoteExecutor};
use atc_core::registry::{PgRegistry, Registry, SqliteRegistry};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize structured logging. Controlled by RUST_LOG env var.
    // Default: info-level for atc crates, warn for everything else.
    //
    // Logs go to stderr so `--json` callers can pipe stdout into `jq` without
    // having to scrub tracing output. This matches the standard CLI convention
    // (logs to stderr, data to stdout) and is required by every `--json`
    // command in the binary, not just `atc run --json`.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("atc_core=info,atc_cli=info,warn")),
        )
        .with_target(true)
        .with_writer(std::io::stderr)
        .init();

    let cli = atc_cli::Args::parse();

    // Apply --color flag to the global style state. Errors here would be
    // pre-config (no logging yet), so use anyhow::Context.
    let color_mode: atc_cli::style::ColorMode = cli.color.parse()?;
    atc_cli::style::set_color_mode(color_mode);

    // Honor --no-pager via env var so the pager module sees it without
    // having to thread the flag through every callsite.
    if cli.no_pager {
        std::env::set_var("ATC_NO_PAGER", "1");
    }

    let config = AtcConfig::load(cli.config.as_deref())?;

    // Cloud ATC slice: select the remote Fly-worker executor + Postgres
    // registry when [cloud] is enabled, otherwise the local tmux/SQLite path.
    if config.cloud.enabled {
        let database_url = config.cloud.resolved_database_url().ok_or_else(|| {
            anyhow::anyhow!(
                "cloud.enabled is true but no Postgres URL is set (cloud.database_url or DATABASE_URL)"
            )
        })?;
        let registry: Arc<dyn Registry> = Arc::new(PgRegistry::connect(&database_url).await?);
        let executor: Arc<dyn AgentExecutor> = Arc::new(RemoteExecutor::new(config.cloud.clone()));
        return atc_cli::run_cloud(&cli, &config, registry, executor).await;
    }

    let db_path = config.registry.resolved_path();
    let registry = Arc::new(SqliteRegistry::open(&db_path).await?);
    let executor = Arc::new(ClaudeExecutor {
        claude_bin: config.dispatch.resolved_claude_bin(),
    });

    atc_cli::run(&cli, &config, registry, executor).await
}
