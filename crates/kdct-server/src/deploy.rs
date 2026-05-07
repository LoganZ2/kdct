use anyhow::{bail, Context, Result};
use tunnel::protocol::ControlChannelCmd;
use tunnel::registry::ClientRegistry;
use std::sync::Arc;
use tokio::sync::{oneshot, RwLock};
use std::collections::HashMap;
use tokio::time::{self, Duration};
use tracing::info;

use crate::db::Database;
use crate::deployment_tracker::DeploymentTracker;
use crate::proxy::RouteTable;

/// Deploy an image to a client node.
pub async fn deploy_image(
    db: &Database,
    registry: &ClientRegistry,
    route_table: &Arc<RwLock<RouteTable>>,
    pool: &Arc<tunnel::port_pool::PortPool>,
    tracker: &DeploymentTracker,
    _docker_results: &Arc<RwLock<HashMap<String, Result<Vec<u16>, String>>>>,
    image_name: &str,
    target_node_id: i64,
) -> Result<String> {
    let image = db
        .get_image_by_name(image_name)?
        .context("Image not found. Use 'image load' first.")?;

    if image.status != "configured" {
        bail!(
            "Image '{}' is not configured (status: {}). Use 'image config' first.",
            image_name,
            image.status
        );
    }

    let nodes = db.list_nodes()?;
    let node = nodes
        .iter()
        .find(|n| n.id == target_node_id)
        .context(format!("Node {} not found", target_node_id))?;

    if node.status != "online" {
        bail!("Node '{}' is not online", node.hostname);
    }

    let routes = db.get_image_routes(image.id)?;
    if routes.is_empty() {
        bail!("No ports configured for image '{}'", image_name);
    }

    let required_ports = routes.len() as u16;
    let node_port_count = (node.port_range_end - node.port_range_start + 1) as u16;
    if node_port_count < required_ports {
        bail!(
            "Node '{}' has only {} ports available, need {}",
            node.hostname,
            node_port_count,
            required_ports
        );
    }

    let free_pool = pool.free_count().await;
    if free_pool < required_ports as usize {
        bail!(
            "Server port pool only has {} free ports, need {}",
            free_pool,
            required_ports
        );
    }

    let table = route_table.write().await;
    for (_, route_path) in &routes {
        if let Some(route_path) = route_path {
            if table.resolve(route_path).is_some() {
                drop(table);
                bail!("Route path '{}' is already in use", route_path);
            }
        }
    }
    drop(table);

    let local_ports: Vec<u16> = routes
        .iter()
        .map(|(port, _)| port.port as u16)
        .collect();
    let mapping = pool
        .assign(&[image.id as u8], &local_ports)
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

    let container_name = image_name.replace(['/', ':'], "-");

    let (node_digest, cmd_tx, pending_docker) = {
        let guard = registry.read().await;
        let digest = guard
            .iter()
            .find(|(_, v)| v.hostname == node.hostname)
            .map(|(k, _)| hex::encode(k));
        let cmd_tx = guard
            .values()
            .find(|e| e.hostname == node.hostname)
            .map(|e| e.cmd_tx.clone())
            .context(format!("Client '{}' not found in registry", node.hostname))?;
        let pending = guard
            .values()
            .find(|e| e.hostname == node.hostname)
            .map(|e| e.pending_docker.clone())
            .context(format!("Client '{}' not found in registry", node.hostname))?;
        (digest, cmd_tx, pending)
    };

    let image_tag = match image.source_type.as_str() {
        "git" => {
            let branch = "main";
            let tag = format!("kdct:{}", image.name.replace('/', "-"));
            cmd_tx
                .send(ControlChannelCmd::DockerBuild {
                    git_url: image.source.clone(),
                    branch: branch.into(),
                    image_tag: tag.clone(),
                })
                .await
                .context("Failed to send DockerBuild command")?;
            info!("Sent DockerBuild for {} -> {}", image.source, tag);
            tag
        }
        _ => image.source.clone(),
    };

    let docker_port_map: Vec<(u16, u16)> = routes
        .iter()
        .enumerate()
        .map(|(i, (img_port, _))| (port_map[i].0, img_port.port as u16))
        .collect();

    let envs = db.get_image_envs(image.id).unwrap_or_default();

    // Register a oneshot to wait for the Docker response
    let (tx, rx) = oneshot::channel::<Result<Vec<u16>, String>>();
    {
        let mut pending = pending_docker.write().await;
        pending.insert(container_name.clone(), tx);
    }

    cmd_tx
        .send(ControlChannelCmd::DockerRun {
            image_tag: image_tag.clone(),
            container_name: container_name.clone(),
            port_map: docker_port_map,
            env: envs,
        })
        .await
        .context("Failed to send DockerRun command")?;

    info!("DockerRun sent for '{}', waiting for container...", container_name);

    let result = match time::timeout(Duration::from_secs(60), rx).await {
        Ok(Ok(res)) => res,
        Ok(Err(err_msg)) => {
            // Clean up pending entry on error
            let mut pending = pending_docker.write().await;
            pending.remove(&container_name);
            return Err(anyhow::anyhow!("{}", err_msg));
        }
        Err(_elapsed) => {
            // Clean up pending entry on timeout
            let mut pending = pending_docker.write().await;
            pending.remove(&container_name);
            return Err(anyhow::anyhow!(
                "Timeout waiting for container '{}' to start (check server logs)",
                container_name
            ));
        }
    };

    if let Some(digest) = node_digest {
        crate::deployment_tracker::record_deployment(
            tracker,
            image_name,
            &container_name,
            &digest,
            server_ports.clone(),
        )
        .await;
    }

    let mut table = route_table.write().await;
    for (i, (_, route_path)) in routes.iter().enumerate() {
        if let Some(path) = route_path {
            table.set(path, port_map[i].1)?;
        }
    }
    drop(table);

    info!(
        "Deployed '{}' -> '{}' (container={}, ports={:?})",
        image_name, node.hostname, container_name, result
    );

    Ok(format!(
        "Deployed '{}' to '{}'. Container: {} ({:?})",
        image_name, node.hostname, container_name, result
    ))
}

/// Stop a deployed image on a specific node.
pub async fn stop_image(
    db: &Database,
    registry: &ClientRegistry,
    route_table: &Arc<RwLock<RouteTable>>,
    pool: &Arc<tunnel::port_pool::PortPool>,
    tracker: &DeploymentTracker,
    _docker_results: &Arc<RwLock<HashMap<String, Result<Vec<u16>, String>>>>,
    image_name: &str,
    target_node_id: i64,
) -> Result<String> {
    let nodes = db.list_nodes()?;
    let node = nodes
        .iter()
        .find(|n| n.id == target_node_id)
        .context(format!("Node {} not found", target_node_id))?;

    let guard = registry.read().await;
    let node_digest = guard
        .iter()
        .find(|(_, v)| v.hostname == node.hostname)
        .map(|(k, _)| hex::encode(k))
        .context("Node not connected in registry")?;
    let cmd_tx = guard
        .iter()
        .find(|(_, v)| v.hostname == node.hostname)
        .map(|(_, v)| v.cmd_tx.clone())
        .context("Node not connected in registry")?;
    let pending_docker = guard
        .iter()
        .find(|(_, v)| v.hostname == node.hostname)
        .map(|(_, v)| v.pending_docker.clone())
        .context("Node not connected in registry")?;
    drop(guard);

    let deployment = crate::deployment_tracker::get_deployment(tracker, image_name, &node_digest)
        .await
        .context("Image is not deployed on this node")?;

    let container_name = image_name.replace(['/', ':'], "-");

    let (tx, rx) = oneshot::channel::<Result<Vec<u16>, String>>();
    {
        let mut pending = pending_docker.write().await;
        pending.insert(container_name.clone(), tx);
    }

    cmd_tx
        .send(ControlChannelCmd::DockerStop {
            container_name: container_name.clone(),
        })
        .await
        .context("Failed to send DockerStop command")?;

    let _ = time::timeout(Duration::from_secs(30), rx).await;

    let mut table = route_table.write().await;
    for port in &deployment.server_ports {
        table.remove_by_port(*port);
        pool.release_by_port(*port).await;
    }
    drop(table);

    crate::deployment_tracker::remove_deployment(tracker, image_name, &node_digest).await;

    info!("Stopped '{}' on '{}'", image_name, node.hostname);

    Ok(format!("Stopped '{}' on '{}'", image_name, node.hostname))
}
