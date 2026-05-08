use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

use crate::proxy::RouteTable;

/// Unique deployment key: (image_name, node_digest)
pub type DeploymentKey = (String, String);

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ActiveDeployment {
    pub image_name: String,
    pub container_name: String,
    pub node_digest: String,
    pub server_ports: Vec<u16>,
}

/// In-memory tracker for active deployments.
/// Keyed by (image_name, node_digest) — the same image can be deployed to multiple nodes.
pub type DeploymentTracker = Arc<RwLock<HashMap<DeploymentKey, ActiveDeployment>>>;

pub fn new_tracker() -> DeploymentTracker {
    Arc::new(RwLock::new(HashMap::new()))
}

pub async fn record_deployment(
    tracker: &DeploymentTracker,
    image_name: &str,
    container_name: &str,
    node_digest: &str,
    server_ports: Vec<u16>,
) {
    let key = (image_name.to_string(), node_digest.to_string());
    let mut guard = tracker.write().await;
    guard.insert(
        key,
        ActiveDeployment {
            image_name: image_name.to_string(),
            container_name: container_name.to_string(),
            node_digest: node_digest.to_string(),
            server_ports,
        },
    );
    info!(
        "Deployment tracked: {} on node {} ({} ports)",
        image_name, node_digest,
        guard.get(&(image_name.to_string(), node_digest.to_string())).map_or(0, |d| d.server_ports.len())
    );
}

pub async fn remove_deployment(
    tracker: &DeploymentTracker,
    image_name: &str,
    node_digest: &str,
) -> Option<ActiveDeployment> {
    let key = (image_name.to_string(), node_digest.to_string());
    tracker.write().await.remove(&key)
}

pub async fn get_deployment(
    tracker: &DeploymentTracker,
    image_name: &str,
    node_digest: &str,
) -> Option<ActiveDeployment> {
    let key = (image_name.to_string(), node_digest.to_string());
    tracker.read().await.get(&key).cloned()
}

#[allow(dead_code)]
pub async fn list_deployments_for_image(
    tracker: &DeploymentTracker,
    image_name: &str,
) -> Vec<ActiveDeployment> {
    tracker
        .read()
        .await
        .iter()
        .filter(|((name, _), _)| name == image_name)
        .map(|(_, d)| d.clone())
        .collect()
}

/// Remove all deployments for a given node digest (on disconnect).
pub async fn remove_by_node(
    tracker: &DeploymentTracker,
    route_table: &Arc<RwLock<RouteTable>>,
    pool: &Arc<tunnel::port_pool::PortPool>,
    node_digest: &str,
) {
    let mut guard = tracker.write().await;
    let keys: Vec<DeploymentKey> = guard
        .iter()
        .filter(|(_, d)| d.node_digest == node_digest)
        .map(|(k, _)| k.clone())
        .collect();

    for key in &keys {
        if let Some(deployment) = guard.remove(key) {
            let mut rt = route_table.write().await;
            for port in &deployment.server_ports {
                rt.remove_by_port(*port);
                pool.release_by_port(*port).await;
            }
        }
    }
}
