use anyhow::{bail, Result};
use clap::Parser;
use std::path::PathBuf;
use tokio::sync::broadcast;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "kdctc", about = "KDCT Docker tunnel client")]
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

    check_docker()?;

    let cli = Cli::parse();
    let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);

    tunnel::run(
        tunnel::cli::Cli {
            config_path: Some(cli.config),
            server: false,
            client: true,
        },
        shutdown_rx,
    )
    .await?;

    drop(shutdown_tx);
    Ok(())
}

fn check_docker() -> Result<()> {
    let output = std::process::Command::new("docker")
        .arg("--version")
        .output();
    match output {
        Ok(o) if o.status.success() => {
            tracing::info!(
                "Docker detected: {}",
                String::from_utf8_lossy(&o.stdout).trim()
            );
            Ok(())
        }
        _ => {
            bail!("Docker is not installed or not in PATH. Please install Docker first.")
        }
    }
}
