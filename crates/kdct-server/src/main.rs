mod admin;
mod api;
mod db;
mod deploy;
mod deployment_tracker;
mod image;
mod interactive;
mod node;
mod proxy;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use tunnel::config::{Config, TransportType};
use tunnel::port_pool::PortPool;
use tunnel::server::Server;
use tunnel::transport::TcpTransport;
use std::path::PathBuf;
use std::sync::Arc;
use std::collections::HashMap;
use tokio::sync::{broadcast, RwLock};
use tracing_subscriber::EnvFilter;

use crate::db::Database;
use crate::proxy::RouteTable;
use tunnel::node_update::NodeEvent;

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
    /// Stop a deployed image on a node
    Stop {
        /// Image name
        name: String,
        /// Target node ID
        #[arg(long = "to")]
        node_id: i64,
    },
    /// List active deployments
    Deployments,
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

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

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
            ImageCmd::Stop { name, node_id } => {
                let cmd = format!("stop {} {}", name, node_id);
                admin::admin_request(&cmd).await?;
            }
            ImageCmd::Deployments => {
                admin::admin_request("deployments").await?;
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

    let pool = PortPool::new(&server_config.port_pool).await?;
    let tracker = crate::deployment_tracker::new_tracker();

    // Mark all nodes offline on startup (they'll come back online when they reconnect)
    db.mark_all_offline()?;

    tracing::info!("Starting KDCT server with domain: {}", domain);

    match server_config.transport.transport_type {
        TransportType::Tcp => {
            let (node_update_tx, mut node_update_rx) =
                tokio::sync::mpsc::channel::<tunnel::node_update::NodeUpdate>(1024);

            let mut server =
                Server::<TcpTransport>::from(server_config.clone(), Some(pool.clone()), node_update_tx)
                    .await?;
            let clients = server.clients.clone();
            let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);

            // Shared map for Docker command results (container_name → result)
            let docker_results: Arc<RwLock<HashMap<String, Result<Vec<u16>, String>>>> =
                Arc::new(RwLock::new(HashMap::new()));

            // Sync node updates to SQLite and handle disconnect cleanup
            let sync_db_path = db_path.clone();
            let sync_rt = route_table.clone();
            let sync_pool = pool.clone();
            let sync_tracker = tracker.clone();
            let sync_docker_results = docker_results.clone();
            tokio::spawn(async move {
                while let Some(update) = node_update_rx.recv().await {
                    match update.event {
                        NodeEvent::Connected { hostname, os, arch, docker_version, port_range_start, port_range_end, cpu_cores, memory_mb, running_containers: _ } => {
                            if let Ok(d) = Database::open(&sync_db_path) {
                                let _ = d.upsert_node(
                                    &update.digest,
                                    &hostname, &os, &arch,
                                    &docker_version,
                                    port_range_start as i64,
                                    port_range_end as i64,
                                    cpu_cores as i64,
                                    memory_mb as i64,
                                );
                            }
                        }
                        NodeEvent::Disconnected { hostname: _ } => {
                            if let Ok(d) = Database::open(&sync_db_path) {
                                let _ = d.set_node_offline(&update.digest);
                            }
                            crate::deployment_tracker::remove_by_node(
                                &sync_tracker,
                                &sync_rt,
                                &sync_pool,
                                &update.digest,
                            ).await;
                        }
                        NodeEvent::ContainerStarted { container_name, ports } => {
                            tracing::info!("docker_results insert: ContainerStarted {}", container_name);
                            let mut results = sync_docker_results.write().await;
                            results.insert(container_name, Ok(ports));
                        }
                        NodeEvent::ContainerStopped { container_name } => {
                            tracing::info!("docker_results insert: ContainerStopped {}", container_name);
                            let mut results = sync_docker_results.write().await;
                            results.insert(container_name, Ok(vec![]));
                        }
                        NodeEvent::ContainerError { container_name, error } => {
                            tracing::info!("docker_results insert: ContainerError {} — {}", container_name, error);
                            let mut results = sync_docker_results.write().await;
                            results.insert(container_name, Err(error));
                        }
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

            // HTTP API (serves web UI and REST API)
            let api_db = Arc::new(db);
            let api_clients = clients.clone();
            let api_rt = route_table.clone();
            let api_pool = pool.clone();
            let api_tracker = tracker.clone();
            let api_docker_results = docker_results.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    api::run_api(api_db, api_clients, api_rt, api_pool, api_tracker, api_docker_results).await
                {
                    tracing::error!("API error: {:#}", e);
                }
            });

            server.run(shutdown_rx).await?;
            drop(shutdown_tx);
            Ok(())
        }
    }
}
