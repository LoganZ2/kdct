use crate::protocol::ContainerInfo;

#[derive(Debug, Clone)]
pub enum NodeEvent {
    Connected {
        /// Hex-encoded service_digest (SHA-256 of the auth token). Persisted
        /// alongside the uuid so the binding survives kdcts restarts and the
        /// uuid-claim check can be enforced.
        service_digest: String,
        hostname: String,
        os: String,
        arch: String,
        docker_version: String,
        port_range_start: u16,
        port_range_end: u16,
        cpu_cores: u32,
        memory_mb: u64,
        running_containers: Vec<ContainerInfo>,
    },
    Disconnected {
        hostname: String,
    },
    ContainerStarted {
        container_name: String,
        ports: Vec<u16>,
    },
    ContainerStopped {
        container_name: String,
    },
    ContainerError {
        container_name: String,
        error: String,
    },
}

/// Sent from server control channel when a client reports status or disconnects.
/// The kdcts binary picks this up and writes to SQLite.
///
/// `uuid` is the stable per-machine identifier (persisted by the client and
/// assigned by the server on first connect).
#[derive(Debug, Clone)]
pub struct NodeUpdate {
    pub uuid: String,
    pub event: NodeEvent,
}
