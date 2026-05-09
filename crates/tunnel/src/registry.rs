use crate::protocol::{ContainerInfo, ControlChannelCmd};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::info;

pub type ForwardCallback = Box<dyn FnOnce(Box<dyn std::any::Any + Send>) + Send>;
pub type SyncedCallback = Mutex<Option<ForwardCallback>>;

/// Stable per-machine identifier assigned by the server on first connect and
/// persisted by the client. Used as the registry key so two machines that
/// share a hostname (or a service name in client.toml) no longer collide.
pub type NodeUuid = String;

pub struct ClientEntry {
    pub node_uuid: NodeUuid,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub docker_version: String,
    pub port_range_start: u16,
    pub port_range_end: u16,
    pub cpu_cores: u32,
    pub memory_mb: u64,
    pub running_containers: Vec<ContainerInfo>,
    pub cmd_tx: mpsc::Sender<ControlChannelCmd>,
    pub pending_docker: Arc<RwLock<HashMap<String, oneshot::Sender<Result<Vec<u16>, String>>>>>,
    pub data_ch_req_tx: mpsc::UnboundedSender<bool>,
    pub port_data_callbacks: Arc<RwLock<VecDeque<(SyncedCallback, u16)>>>,
}

pub type ClientRegistry = Arc<RwLock<HashMap<NodeUuid, ClientEntry>>>;

pub fn new_registry() -> ClientRegistry {
    Arc::new(RwLock::new(HashMap::new()))
}

pub async fn upsert(
    registry: &ClientRegistry,
    node_uuid: NodeUuid,
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
    data_ch_req_tx: mpsc::UnboundedSender<bool>,
    port_data_callbacks: Arc<RwLock<VecDeque<(SyncedCallback, u16)>>>,
) {
    let mut guard = registry.write().await;
    let is_new = !guard.contains_key(&node_uuid);
    guard.insert(
        node_uuid.clone(),
        ClientEntry {
            node_uuid: node_uuid.clone(),
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
            data_ch_req_tx,
            port_data_callbacks,
        },
    );
    if is_new {
        info!("Client registered: {}", node_uuid);
    }
}

pub async fn remove(registry: &ClientRegistry, node_uuid: &str) {
    if let Some(entry) = registry.write().await.remove(node_uuid) {
        info!("Client disconnected: {} ({})", entry.node_uuid, entry.hostname);
    }
}
