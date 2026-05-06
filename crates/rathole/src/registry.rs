//! Connected-client registry.
//!
//! When a client connects and sends `ReportStatus`, its entry is updated.
//! The admin layer queries this registry for `list` and `pipeline send`.

use crate::protocol::{ControlChannelCmd, Digest};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tracing::info;

/// Information about a connected client
#[derive(Debug, Clone)]
pub struct ClientEntry {
    pub service_name: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    /// Send pipeline commands to this client's control channel
    pub pipeline_tx: mpsc::Sender<ControlChannelCmd>,
}

/// Shared client registry, indexed by service digest.
pub type ClientRegistry = Arc<RwLock<HashMap<Digest, ClientEntry>>>;

/// Create a new empty registry.
pub fn new_registry() -> ClientRegistry {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Update a client entry when ReportStatus is received.
/// If the client is not yet in the registry, inserts it.
pub async fn upsert(
    registry: &ClientRegistry,
    digest: Digest,
    service_name: String,
    hostname: String,
    os: String,
    arch: String,
    pipeline_tx: mpsc::Sender<ControlChannelCmd>,
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
            pipeline_tx,
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
