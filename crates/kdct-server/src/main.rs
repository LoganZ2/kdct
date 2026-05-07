mod api;
mod db;
mod deploy;
mod deployment_tracker;
mod image;
mod proxy;

use anyhow::{bail, Result};
use clap::Parser;
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
#[command(name = "kdcts")]
struct Args {
    /// Path to server config TOML
    #[arg(long, default_value = "server.toml")]
    config: PathBuf,
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

    let args = Args::parse();
    start_server(args.config).await?;

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
                    api::run_api(api_db, api_clients, api_rt, api_pool, api_tracker, api_docker_results, Default::default()).await
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
