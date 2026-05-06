mod admin;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rathole::config::{Config, TransportType};
use rathole::server::Server;
use rathole::transport::TcpTransport;
use std::path::PathBuf;
use tokio::sync::broadcast;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "kdct-server", about = "KDCT tunnel server")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the server daemon
    Start {
        /// Path to server config TOML
        #[arg(long, default_value = "server.toml")]
        config: PathBuf,
        /// Admin API port (localhost only)
        #[arg(long, default_value_t = admin::DEFAULT_ADMIN_PORT)]
        admin_port: u16,
    },
    /// List connected clients
    List {
        #[arg(long, default_value_t = admin::DEFAULT_ADMIN_PORT)]
        admin_port: u16,
    },
    /// Send a pipeline to a connected client
    Pipeline {
        #[command(subcommand)]
        action: PipelineCmd,
        #[arg(long, default_value_t = admin::DEFAULT_ADMIN_PORT)]
        admin_port: u16,
    },
}

#[derive(Subcommand)]
enum PipelineCmd {
    /// Send a pipeline from a JSON/YAML file
    Send {
        /// Target client name (service name)
        #[arg(long)]
        client: String,
        /// Path to pipeline definition file (JSON or YAML)
        #[arg(long)]
        file: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Start { config, admin_port } => {
            start_server(config, admin_port).await?;
        }
        Commands::List { admin_port } => {
            admin::admin_request(admin_port, r#"{"cmd":"list"}"#).await?;
        }
        Commands::Pipeline { action, admin_port } => match action {
            PipelineCmd::Send { client, file } => {
                let content = tokio::fs::read_to_string(&file).await?;
                let steps: Vec<rathole::protocol::PipelineStep> =
                    serde_json::from_str(&content).or_else(|_| {
                        serde_yaml::from_str(&content)
                            .map_err(|e| anyhow::anyhow!("Failed to parse pipeline file: {}", e))
                    })?;
                let cmd = serde_json::json!({
                    "cmd": "pipeline",
                    "client": client,
                    "steps": steps,
                });
                admin::admin_request(admin_port, &cmd.to_string()).await?;
            }
        },
    }

    Ok(())
}

async fn start_server(config_path: PathBuf, admin_port: u16) -> Result<()> {
    let config = Config::from_file(&config_path).await?;
    let server_config = config
        .server
        .ok_or_else(|| anyhow::anyhow!("Missing [server] section in config"))?;

    // Only TCP transport is supported out-of-the-box.
    // For TLS/Noise/WebSocket, compile rathole with the appropriate features.
    match server_config.transport.transport_type {
        TransportType::Tcp => {
            let pool = if let Some(ref range) = server_config.port_pool {
                Some(rathole::port_pool::PortPool::new(range).await?)
            } else {
                None
            };
            let mut server = Server::<TcpTransport>::from(server_config, pool).await?;
            let clients = server.clients.clone();
            let rx = std::mem::replace(
                &mut server.pipeline_output_rx,
                tokio::sync::mpsc::channel(1).1,
            );

            let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);
            let admin_shutdown = shutdown_tx.subscribe();
            tokio::spawn(async move {
                if let Err(e) = admin::run_admin(admin_port, clients, rx, admin_shutdown).await {
                    tracing::error!("Admin server error: {:#}", e);
                }
            });

            tracing::info!("Server starting (TCP transport)...");
            let (_tx, update_rx) = tokio::sync::mpsc::channel(1);
            server.run(shutdown_rx, update_rx).await?;
        }
        other => {
            anyhow::bail!(
                "Transport type {:?} requires specific crate features. \
                 Use TCP for now, or rebuild with the required features.",
                other
            );
        }
    }

    Ok(())
}
