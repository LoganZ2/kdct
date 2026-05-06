use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use tokio::sync::broadcast;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "kdct-client", about = "KDCT tunnel client")]
struct Cli {
    /// Path to client config TOML
    #[arg(long, default_value = "client.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let (_shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);

    rathole::run(
        rathole::cli::Cli {
            config_path: Some(cli.config),
            server: false,
            client: true,
            genkey: None,
        },
        shutdown_rx,
    )
    .await?;

    Ok(())
}
