use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = atc_cli::Args::parse();
    match cli.command {
        atc_cli::Commands::Dispatch { mode, slug } => {
            eprintln!(
                "dispatch not yet implemented: mode={:?} slug={}",
                mode, slug
            );
            std::process::exit(1);
        }
    }
}
