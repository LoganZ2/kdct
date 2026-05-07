use anyhow::{bail, Context, Result};
use tunnel::protocol::ControlChannelCmd;
use tunnel::registry::ClientRegistry;
use tunnel::server::spawn_port_accept_loop;
use std::sync::Arc;
use tokio::sync::{broadcast, oneshot, RwLock};
use tokio::time::{self, Duration};
use tracing::info;

use crate::db::Database;
use crate::deployment_tracker::DeploymentTracker;
use crate::proxy::RouteTable;

pub async fn deploy_connection(
    db: &Database,
    registry: &ClientRegistry,
    route_table: &Arc<RwLock<RouteTable>>,
    pool: &Arc<tunnel::port_pool::PortPool>,
    tracker: &DeploymentTracker,
    connection_id: i64,
    active_forwards: &Arc<RwLock<Vec<(u16, broadcast::Sender<bool>)>>>,
) -> Result<String> {
    let conn_info = db.get_connection(connection_id)?
        .context("Connection not found")?;

    let bridge_id = conn_info["bridge_id"].as_i64().context("Bridge not assigned")?;
    let image_id = conn_info["image_id"].as_i64().context("Image not assigned")?;
    let node_id = conn_info["node_id"].as_i64().context("Node not assigned")?;

    let bridge_name = conn_info["bridge_name"].as_str().unwrap_or("").to_string();
    let image_name = conn_info["image_name"].as_str().unwrap_or("").to_string();
    let connection_name = conn_info["name"].as_str().unwrap_or("").to_string();

    // Check node is online
    let node = db.get_node_by_id(node_id)?.context("Node not found")?;
    if node.status != "online" {
        bail!("Node '{}' is not online", node.hostname);
    }

    let image = db.get_image_by_name(&image_name)?
        .context("Image not found")?;

    let ports = db.get_bridge_ports(bridge_id)?;
    if ports.is_empty() {
        bail!("Bridge '{}' has no ports configured", bridge_name);
    }

    let required_ports = ports.len() as u16;
    let node_port_count = (node.port_range_end - node.port_range_start + 1) as u16;
    if node_port_count < required_ports {
        bail!("Node '{}' has only {} ports available, need {}", node.hostname, node_port_count, required_ports);
    }

    let free_pool = pool.free_count().await;
    if free_pool < required_ports as usize {
        bail!("Server port pool only has {} free ports, need {}", free_pool, required_ports);
    }

    // Check route conflicts
    {
        let table = route_table.read().await;
        for p in &ports {
            if let Some(ref path) = p.route_path {
                if table.resolve(path).is_some() {
                    bail!("Route path '{}' is already in use", path);
                }
            }
        }
    }

    let container_ports: Vec<u16> = ports.iter().map(|p| p.container_port as u16).collect();
    let mapping = pool.assign(&[image_id as u8], &container_ports)
        .await
        .context("Failed to assign ports from pool")?;

    let mut client_port = node.port_range_start as u16;
    let mut port_map: Vec<(u16, u16)> = Vec::new();
    let mut server_ports: Vec<u16> = Vec::new();
    for (_, server_port) in &mapping {
        port_map.push((client_port, *server_port));
        server_ports.push(*server_port);
        client_port += 1;
    }

    let container_name = connection_name.replace(['/', ':'], "-");

    let (node_digest, cmd_tx, pending_docker, data_ch_req_tx, port_data_callbacks) = {
        let guard = registry.read().await;
        let digest = guard.iter()
            .find(|(_, v)| v.hostname == node.hostname)
            .map(|(k, _)| hex::encode(k));
        let entry = guard.values()
            .find(|e| e.hostname == node.hostname)
            .context(format!("Client '{}' not found in registry", node.hostname))?;
        (digest, entry.cmd_tx.clone(), entry.pending_docker.clone(),
         entry.data_ch_req_tx.clone(), entry.port_data_callbacks.clone())
    };

    let image_tag = match image.source_type.as_str() {
        "git" => {
            let tag = format!("kdct:{}", image.name.replace('/', "-"));
            cmd_tx.send(ControlChannelCmd::DockerBuild {
                git_url: image.source.clone(),
                branch: "main".into(),
                image_tag: tag.clone(),
            }).await.context("Failed to send DockerBuild command")?;
            tag
        }
        _ => image.source.clone(),
    };

    let docker_port_map: Vec<(u16, u16)> = ports.iter()
        .enumerate()
        .map(|(i, p)| (port_map[i].0, p.container_port as u16))
        .collect();

    let envs = db.get_bridge_envs(bridge_id).unwrap_or_default();

    let (tx, rx) = oneshot::channel::<Result<Vec<u16>, String>>();
    {
        let mut pending = pending_docker.write().await;
        pending.insert(container_name.clone(), tx);
    }

    cmd_tx.send(ControlChannelCmd::DockerRun {
        image_tag: image_tag.clone(),
        container_name: container_name.clone(),
        port_map: docker_port_map,
        env: envs,
    }).await.context("Failed to send DockerRun command")?;

    info!("DockerRun sent for '{}', waiting for container...", container_name);

    let _result = match time::timeout(Duration::from_secs(60), rx).await {
        Ok(Ok(res)) => res,
        Ok(Err(err_msg)) => {
            let mut pending = pending_docker.write().await;
            pending.remove(&container_name);
            for (_, sp) in &mapping { pool.release_by_port(*sp).await; }
            return Err(anyhow::anyhow!("{}", err_msg));
        }
        Err(_elapsed) => {
            let mut pending = pending_docker.write().await;
            pending.remove(&container_name);
            for (_, sp) in &mapping { pool.release_by_port(*sp).await; }
            return Err(anyhow::anyhow!("Timeout waiting for container '{}' to start", container_name));
        }
    };

    if let Some(digest) = node_digest {
        crate::deployment_tracker::record_deployment(
            tracker, &connection_name, &container_name, &digest, server_ports.clone(),
        ).await;
    }

    // Register routes + spawn accept loops
    let mut table = route_table.write().await;
    for (i, p) in ports.iter().enumerate() {
        let server_port = port_map[i].1;
        if p.mode == "route" {
            if let Some(ref path) = p.route_path {
                table.set(path, server_port)?;
            }
        }
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);
        spawn_port_accept_loop(
            pool.clone(), server_port, port_map[i].0,
            data_ch_req_tx.clone(), port_data_callbacks.clone(), shutdown_rx,
        );
        active_forwards.write().await.push((server_port, shutdown_tx));
    }
    drop(table);

    db.update_connection_node(connection_id, Some(node_id), "deployed", Some(&container_name))?;

    info!("Deployed connection '{}' -> '{}' (container={})", connection_name, node.hostname, container_name);
    Ok(format!("Deployed '{}' to '{}'", connection_name, node.hostname))
}

pub async fn stop_connection(
    db: &Database,
    registry: &ClientRegistry,
    route_table: &Arc<RwLock<RouteTable>>,
    pool: &Arc<tunnel::port_pool::PortPool>,
    tracker: &DeploymentTracker,
    connection_id: i64,
    active_forwards: &Arc<RwLock<Vec<(u16, broadcast::Sender<bool>)>>>,
) -> Result<String> {
    let conn_info = db.get_connection(connection_id)?
        .context("Connection not found")?;

    let connection_name = conn_info["name"].as_str().unwrap_or("").to_string();
    let node_id = conn_info["node_id"].as_i64();
    let container_name = conn_info["container_name"].as_str().unwrap_or(&connection_name).to_string();

    if conn_info["status"].as_str() != Some("deployed") {
        bail!("Connection is not deployed");
    }

    // Try to send DockerStop if node is still online
    if let Some(nid) = node_id {
        if let Ok(Some(node)) = db.get_node_by_id(nid) {
            let guard = registry.read().await;
            let node_digest = guard.iter()
                .find(|(_, v)| v.hostname == node.hostname)
                .map(|(k, _)| hex::encode(k));
            if let Some(entry) = guard.values().find(|e| e.hostname == node.hostname) {
                let cmd_tx = entry.cmd_tx.clone();
                let pending_docker = entry.pending_docker.clone();
                drop(guard);

                let (tx, rx) = oneshot::channel::<Result<Vec<u16>, String>>();
                {
                    let mut pending = pending_docker.write().await;
                    pending.insert(container_name.clone(), tx);
                }
                let _ = cmd_tx.send(ControlChannelCmd::DockerStop {
                    container_name: container_name.clone(),
                }).await;
                let _ = time::timeout(Duration::from_secs(30), rx).await;
            }
        }
    }

    // Clean up routes + pool ports
    if let Some(digest_hex) = node_id.and_then(|nid| {
        db.get_node_by_id(nid).ok().flatten().map(|n|
            hex::encode(n.auth_digest.unwrap_or_default())
        )
    }) {
        if let Some(deployment) = crate::deployment_tracker::get_deployment(tracker, &connection_name, &digest_hex).await {
            let mut table = route_table.write().await;
            for port in &deployment.server_ports {
                table.remove_by_port(*port);
                pool.release_by_port(*port).await;
            }
            drop(table);

            let mut fw = active_forwards.write().await;
            fw.retain(|(sp, tx)| {
                if deployment.server_ports.contains(sp) { let _ = tx.send(true); false } else { true }
            });

            crate::deployment_tracker::remove_deployment(tracker, &connection_name, &digest_hex).await;
        }
    }

    db.update_connection_node(connection_id, None, "pending", None)?;

    info!("Stopped connection '{}'", connection_name);
    Ok(format!("Stopped '{}'", connection_name))
}
