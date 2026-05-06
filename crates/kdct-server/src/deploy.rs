use anyhow::{bail, Context, Result};
use rathole::protocol::ControlChannelCmd;
use rathole::registry::ClientRegistry;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

use crate::db::Database;
use crate::proxy::RouteTable;

/// Deploy an image to a client node.
pub async fn deploy_image(
    db: &Database,
    registry: &ClientRegistry,
    route_table: &Arc<RwLock<RouteTable>>,
    pool: &Arc<rathole::port_pool::PortPool>,
    image_name: &str,
    target_node_id: i64,
) -> Result<()> {
    // 1. Find the image
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

    // 2. Find the target node
    let nodes = db.list_nodes()?;
    let node = nodes
        .iter()
        .find(|n| n.id == target_node_id)
        .context(format!("Node {} not found", target_node_id))?;

    if node.status != "online" {
        bail!("Node '{}' is not online", node.hostname);
    }

    // 3. Get image ports and routes
    let routes = db.get_image_routes(image.id)?;
    if routes.is_empty() {
        bail!("No ports configured for image '{}'", image_name);
    }

    let required_ports = routes.len() as u16;

    // 4. Check free ports on the node
    let node_port_count = (node.port_range_end - node.port_range_start + 1) as u16;
    if node_port_count < required_ports {
        bail!(
            "Node '{}' has only {} ports available, need {}",
            node.hostname,
            node_port_count,
            required_ports
        );
    }

    // 5. Check free server pool ports
    let free_pool = pool.free_count().await;
    if free_pool < required_ports as usize {
        bail!(
            "Server port pool only has {} free ports, need {}",
            free_pool,
            required_ports
        );
    }

    // 6. Check route conflicts
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

    // 7. Assign server ports from pool
    let local_ports: Vec<u16> = routes
        .iter()
        .map(|(port, _)| port.port as u16)
        .collect();
    let mapping = pool
        .assign(&[image.id as u8], &local_ports)
        .await
        .context("Failed to assign ports from pool")?;

    // 8. Allocate client ports from node's range
    let mut client_port = node.port_range_start as u16;
    let mut port_map: Vec<(u16, u16)> = Vec::new();
    for (_, server_port) in &mapping {
        port_map.push((client_port as u16, *server_port));
        client_port += 1;
    }

    // 9. Find the client's control channel
    let guard = registry.read().await;
    let cmd_tx = guard
        .values()
        .find(|e| e.hostname == node.hostname)
        .map(|e| e.cmd_tx.clone())
        .context(format!("Client '{}' not found in registry", node.hostname))?;
    drop(guard);

    let container_name = image_name.replace(['/', ':'], "-");

    // Build image tag for deployment
    let image_tag = match image.source_type.as_str() {
        "git" => {
            // Send DockerBuild to client first
            let branch = "main"; // TODO: make configurable
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
        _ => image.source.clone(), // docker_hub: use source directly
    };

    // Map (client_port, image_port) for DockerRun
    let docker_port_map: Vec<(u16, u16)> = routes
        .iter()
        .enumerate()
        .map(|(i, (img_port, _))| (port_map[i].0, img_port.port as u16))
        .collect();

    let envs = db.get_image_envs(image.id).unwrap_or_default();

    cmd_tx
        .send(ControlChannelCmd::DockerRun {
            image_tag: image_tag.clone(),
            container_name: container_name.clone(),
            port_map: docker_port_map,
            env: envs,
        })
        .await
        .context("Failed to send DockerRun command")?;

    // 10. Record deployment
    let deployment_id = db.insert_deployment(image.id, node.id)?;
    for (i, (image_port, _)) in routes.iter().enumerate() {
        db.insert_port_allocation(
            deployment_id,
            image_port.id,
            port_map[i].0 as i64,
            port_map[i].1 as i64,
        )?;
    }

    // 11. Update RouteTable
    let mut table = route_table.write().await;
    for (i, (_, route_path)) in routes.iter().enumerate() {
        if let Some(path) = route_path {
            table.set(path, port_map[i].1)?;
        }
    }
    drop(table);

    // 12. Update image status
    db.update_image_status(image.id, "deployed")?;

    info!(
        "Deployed '{}' -> '{}' (container={})",
        image_name, node.hostname, container_name
    );
    println!(
        "Deployed '{}' to '{}'. Container: {}",
        image_name, node.hostname, container_name
    );

    Ok(())
}

/// Stop a deployed image.
pub async fn stop_image(
    db: &Database,
    registry: &ClientRegistry,
    route_table: &Arc<RwLock<RouteTable>>,
    pool: &Arc<rathole::port_pool::PortPool>,
    image_name: &str,
) -> Result<()> {
    let image = db
        .get_image_by_name(image_name)?
        .context("Image not found")?;

    let deployment = db
        .get_deployment_by_image(image.id)?
        .context("Image is not deployed")?;

    if deployment.status != "running" {
        bail!("Image '{}' is not running", image_name);
    }

    // Find the node
    let nodes = db.list_nodes()?;
    let node = nodes
        .iter()
        .find(|n| n.id == deployment.client_node_id)
        .context("Deployment node not found")?;

    // Send DockerStop
    let container_name = image_name.replace(['/', ':'], "-");

    let guard = registry.read().await;
    let cmd_tx = guard
        .values()
        .find(|e| e.hostname == node.hostname)
        .map(|e| e.cmd_tx.clone())
        .context(format!("Client '{}' not found in registry", node.hostname))?;
    drop(guard);

    cmd_tx
        .send(ControlChannelCmd::DockerStop {
            container_name: container_name.clone(),
        })
        .await
        .context("Failed to send DockerStop command")?;

    // Remove routes from RouteTable
    let allocations = db.get_port_allocations(deployment.id)?;
    let mut table = route_table.write().await;
    for alloc in &allocations {
        table.remove_by_port(alloc.server_port as u16);
        // Release server port
        pool.release_by_port(alloc.server_port as u16).await;
    }
    drop(table);

    // Update DB
    db.set_deployment_stopped(image.id)?;
    db.update_image_status(image.id, "stopped")?;

    info!("Stopped '{}'", image_name);
    println!("Stopped '{}'", image_name);

    Ok(())
}
