use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{error, info};

use crate::db::Database;
use crate::deploy;
use crate::proxy::RouteTable;
use rathole::port_pool::PortPool;
use rathole::registry::ClientRegistry;

const ADMIN_PORT: u16 = 9921;

pub async fn run_admin(
    db: Database,
    registry: ClientRegistry,
    route_table: Arc<RwLock<RouteTable>>,
    pool: Arc<PortPool>,
) -> Result<()> {
    let listener = TcpListener::bind(format!("127.0.0.1:{}", ADMIN_PORT))
        .await
        .context("Failed to bind admin port")?;
    info!("Admin API listening on 127.0.0.1:{}", ADMIN_PORT);

    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                error!("Admin accept error: {}", e);
                continue;
            }
        };

        let db = match db.clone_for_connection() {
            Ok(d) => Arc::new(d),
            Err(e) => {
                error!("Failed to open admin DB: {}", e);
                continue;
            }
        };
        let registry = registry.clone();
        let rt = route_table.clone();
        let p = pool.clone();

        tokio::spawn(async move {
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            if reader.read_line(&mut line).await.is_err() {
                return;
            }
            let line = line.trim().to_string();

            let parts: Vec<&str> = line.split_whitespace().collect();
            let result = match parts.get(0).copied() {
                Some("deploy") if parts.len() >= 3 => {
                    let image_name = parts[1];
                    let node_id: i64 = parts[2].parse().unwrap_or(0);
                    deploy::deploy_image(db.as_ref(), &registry, &rt, &p, image_name, node_id).await
                }
                Some("stop") if parts.len() >= 2 => {
                    let image_name = parts[1];
                    deploy::stop_image(db.as_ref(), &registry, &rt, &p, image_name).await
                }
                Some("ping") => Ok(()),
                _ => Err(anyhow::anyhow!("unknown command")),
            };

            let resp = match result {
                Ok(()) => "OK\n".to_string(),
                Err(e) => format!("ERROR {}\n", e),
            };
            let _ = stream.write_all(resp.as_bytes()).await;
        });
    }
}

/// Send a command to the admin API and print the response.
pub async fn admin_request(command: &str) -> Result<()> {
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", ADMIN_PORT))
        .await
        .context("Failed to connect to admin API. Is kdcts running?")?;

    stream
        .write_all(format!("{}\n", command).as_bytes())
        .await?;

    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;

    if line.starts_with("ERROR") {
        eprintln!("{}", &line[6..].trim());
    } else if line.starts_with("OK") {
        println!("{}", &line[3..].trim());
    }

    Ok(())
}
