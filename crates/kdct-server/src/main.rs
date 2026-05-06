mod admin;
mod db;
mod deploy;
mod image;
mod interactive;
mod node;
mod proxy;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use rathole::config::{Config, TransportType};
use rathole::port_pool::PortPool;
use rathole::server::Server;
use rathole::transport::TcpTransport;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tracing_subscriber::EnvFilter;

use crate::db::Database;
use crate::proxy::RouteTable;

#[derive(Parser)]
#[command(name = "kdcts", about = "KDCT Docker Container Tunnel Server")]
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
    },
    /// Load a Docker image and inspect its EXPOSE ports
    Image {
        #[command(subcommand)]
        action: ImageCmd,
    },
    /// Manage connected client nodes
    Node {
        #[command(subcommand)]
        action: NodeCmd,
    },
}

#[derive(Subcommand)]
enum ImageCmd {
    /// Load an image from Docker Hub or Git URL
    Load {
        /// Image source (e.g. nginx:latest, or git URL)
        source: String,
    },
    /// Add port mappings or env vars to an image
    Config {
        /// Image name
        name: String,
        /// Additional port mappings (HOST:CONTAINER)
        #[arg(short = 'p', long = "port")]
        ports: Vec<String>,
        /// Environment variables (KEY=VALUE, repeatable)
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
    },
    /// List all loaded images
    List,
    /// Show image details and route configuration
    Show {
        /// Image name
        name: String,
    },
    /// Deploy an image to a client node
    Deploy {
        /// Image name
        name: String,
        /// Target node ID
        #[arg(long = "to")]
        node_id: i64,
    },
    /// Stop a deployed image
    Stop {
        /// Image name
        name: String,
    },
}

#[derive(Subcommand)]
enum NodeCmd {
    /// List all client nodes
    List,
    /// Show node details
    Show {
        /// Node ID
        id: i64,
    },
}

fn parse_port_mapping(s: &str) -> anyhow::Result<(i64, i64)> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        bail!("Invalid port mapping: '{}'. Use HOST:CONTAINER format.", s);
    }
    let host: i64 = parts[0]
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid host port: '{}'", parts[0]))?;
    let container: i64 = parts[1]
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid container port: '{}'", parts[1]))?;
    Ok((host, container))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    check_docker().await?;

    let cli = Cli::parse();

    match cli.command {
        Commands::Start { config } => {
            start_server(config).await?;
        }
        Commands::Image { action } => match action {
            ImageCmd::Load { source } => {
                let db = Database::open(&PathBuf::from("kdct.db"))?;
                let name = image::load_image(&db, &source).await?;
                interactive::configure_routes_interactive(&db, &name, &[]).await?;
            }
            ImageCmd::Config { name, ports, env } => {
                let db = Database::open(&PathBuf::from("kdct.db"))?;
                for p in &ports {
                    let (_host, container) = parse_port_mapping(p)?;
                    image::add_port_mapping(&db, &name, _host, container).await?;
                }
                interactive::configure_routes_interactive(&db, &name, &env).await?;
            }
            ImageCmd::List => {
                let db = Database::open(&PathBuf::from("kdct.db"))?;
                let images = db.list_images()?;
                if images.is_empty() {
                    println!("No images loaded.");
                } else {
                    println!("{:<20} {:<15} {:<15} {}", "NAME", "SOURCE", "TYPE", "STATUS");
                    for img in &images {
                        println!(
                            "{:<20} {:<15} {:<15} {}",
                            img.name, img.source, img.source_type, img.status
                        );
                    }
                }
            }
            ImageCmd::Show { name } => {
                let db = Database::open(&PathBuf::from("kdct.db"))?;
                image::show_image(&db, &name)?;
            }
            ImageCmd::Deploy { name, node_id } => {
                let cmd = format!("deploy {} {}", name, node_id);
                admin::admin_request(&cmd).await?;
            }
            ImageCmd::Stop { name } => {
                let cmd = format!("stop {}", name);
                admin::admin_request(&cmd).await?;
            }
        },
        Commands::Node { action } => {
            let db = Database::open(&PathBuf::from("kdct.db"))?;
            match action {
                NodeCmd::List => {
                    node::list_nodes(&db)?;
                }
                NodeCmd::Show { id } => {
                    node::show_node(&db, id)?;
                }
            }
        }
    }

    Ok(())
}

async fn check_docker() -> Result<()> {
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

async fn start_server(config_path: PathBuf) -> Result<()> {
    fdlimit::raise_fd_limit();
    let config = Config::from_file(&config_path).await?;
    let server_config = config
        .server
        .ok_or_else(|| anyhow::anyhow!("Missing [server] section in config"))?;

    let domain = server_config
        .domain
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!(
            "Domain is required. Set `domain = \"example.com\"` in [server] config."
        ))?
        .clone();

    let http_port = server_config.http_port;
    let https_port = server_config.https_port;
    let db_path = PathBuf::from("kdct.db");

    let db = Database::open(&db_path)?;

    let route_table = Arc::new(RwLock::new(RouteTable::new()));
    {
        let saved_routes = db.get_active_routes()?;
        let mut table = route_table.write().await;
        for (path, port) in saved_routes {
            let _ = table.set(&path, port as u16);
        }
    }

    let pool = PortPool::new(&server_config.port_pool).await?;

    tracing::info!("Starting KDCT server with domain: {}", domain);

    match server_config.transport.transport_type {
        TransportType::Tcp => {
            let (node_update_tx, mut node_update_rx) =
                tokio::sync::mpsc::channel::<rathole::node_update::NodeUpdate>(1024);

            let mut server =
                Server::<TcpTransport>::from(server_config.clone(), Some(pool.clone()), node_update_tx)
                    .await?;
            let clients = server.clients.clone();
            let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);

            // Sync node updates to SQLite
            let sync_db_path = db_path.clone();
            tokio::spawn(async move {
                while let Some(update) = node_update_rx.recv().await {
                    match Database::open(&sync_db_path) {
                        Ok(d) => {
                            let _ = d.upsert_node(
                                &update.digest,
                                &update.hostname, &update.os, &update.arch,
                                &update.docker_version,
                                update.port_range_start as i64,
                                update.port_range_end as i64,
                                update.cpu_cores as i64,
                                update.memory_mb as i64,
                            );
                        }
                        Err(e) => tracing::error!("Failed to open DB for node update: {}", e),
                    }
                }
            });

            // Pingora reverse proxy
            let proxy_rt = route_table.clone();
            let proxy_domain = domain.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    proxy::run_proxy(proxy_rt, proxy_domain, http_port, https_port).await
                {
                    tracing::error!("Proxy error: {:#}", e);
                }
            });

            // Admin TCP API (for deploy/stop CLI commands)
            let admin_db = db;
            let admin_clients = clients.clone();
            let admin_rt = route_table.clone();
            let admin_pool = pool.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    admin::run_admin(admin_db, admin_clients, admin_rt, admin_pool).await
                {
                    tracing::error!("Admin error: {:#}", e);
                }
            });

            let (_tx, update_rx) = tokio::sync::mpsc::channel(1);
            server.run(shutdown_rx, update_rx).await?;
            drop(shutdown_tx);
            Ok(())
        }
        other => {
            bail!(
                "Transport type {:?} requires specific crate features. \
                 Use TCP for now, or rebuild with the required features.",
                other
            );
        }
    }
}
