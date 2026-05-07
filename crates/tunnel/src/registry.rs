//! Connected-client registry.
//!
//! When a client connects and sends `ReportNodeStatus`, its entry is updated.
//! The admin layer queries this registry for client info.

use crate::protocol::{ContainerInfo, ControlChannelCmd, Digest};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::info;

/// Information about a connected client
pub struct ClientEntry {
    pub service_name: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub docker_version: String,
    pub port_range_start: u16,
    pub port_range_end: u16,
    pub cpu_cores: u32,
    pub memory_mb: u64,
    pub running_containers: Vec<ContainerInfo>,
    /// Send commands to this client's control channel
    pub cmd_tx: mpsc::Sender<ControlChannelCmd>,
    /// Pending Docker response channels, keyed by container_name
    pub pending_docker: Arc<RwLock<HashMap<String, oneshot::Sender<Result<Vec<u16>, String>>>>>,
}

/// Shared client registry, indexed by service digest.
pub type ClientRegistry = Arc<RwLock<HashMap<Digest, ClientEntry>>>;

/// Create a new empty registry.
pub fn new_registry() -> ClientRegistry {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Update a client entry when ReportNodeStatus is received.
pub async fn upsert(
    registry: &ClientRegistry,
    digest: Digest,
    service_name: String,
    hostname: String,
    os: String,
    arch: String,
    docker_version: String,
    port_range_start: u16,
    port_range_end: u16,
    cpu_cores: u32,
    memory_mb: u64,
    running_containers: Vec<ContainerInfo>,
    cmd_tx: mpsc::Sender<ControlChannelCmd>,
    pending_docker: Arc<RwLock<HashMap<String, oneshot::Sender<Result<Vec<u16>, String>>>>>,
) {
    let mut guard = registry.write().await;
    let is_new = !guard.contains_key(&digest);
    guard.insert(
        digest,
        ClientEntry {
            service_name: service_name.clone(),
            hostname,
            os,
            arch,
            docker_version,
            port_range_start,
            port_range_end,
            cpu_cores,
            memory_mb,
            running_containers,
            cmd_tx,
            pending_docker,
        },
    );
    if is_new {
        info!("Client registered: {}", service_name);
    }
}

/// Remove a client from the registry.
pub async fn remove(registry: &ClientRegistry, digest: &Digest) {
    if let Some(entry) = registry.write().await.remove(digest) {
        info!("Client disconnected: {}", entry.service_name);
    }
}
