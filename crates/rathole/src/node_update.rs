use crate::protocol::ContainerInfo;

/// Sent from server control channel when a client reports status.
/// The kdcts binary picks this up and writes to SQLite.
#[derive(Debug, Clone)]
pub struct NodeUpdate {
    pub digest: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub docker_version: String,
    pub port_range_start: u16,
    pub port_range_end: u16,
    pub cpu_cores: u32,
    pub memory_mb: u64,
    pub running_containers: Vec<ContainerInfo>,
}
