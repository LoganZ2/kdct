//! Server port pool — pre-bind, auto-assign, release.
//!
//! On startup, all ports in the pool are bound via TcpListener.
//! If any port is taken, startup fails immediately (no partial occupation).
//!
//! Ports are auto-assigned when a client reports local ports to expose.
//! Assignments are returned to the pool when the client disconnects.

use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{debug, info};

/// Parse a port range string like "9000-9999" or a comma-separated list like "9000-9010,9020".
pub fn parse_port_range(spec: &str) -> Result<Vec<u16>> {
    let mut ports = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.contains('-') {
            let mut range = part.splitn(2, '-');
            let start: u16 = range
                .next()
                .unwrap()
                .trim()
                .parse()
                .context("Invalid port range start")?;
            let end: u16 = range
                .next()
                .unwrap()
                .trim()
                .parse()
                .context("Invalid port range end")?;
            if start > end {
                bail!("Port range start {} > end {}", start, end);
            }
            for p in start..=end {
                ports.push(p);
            }
        } else {
            let p: u16 = part.parse().context("Invalid port number")?;
            ports.push(p);
        }
    }
    Ok(ports)
}

/// Pre-bound port pool.
pub struct PortPool {
    /// Available ports (pre-bound, ready to assign)
    free: RwLock<BTreeSet<u16>>,
    /// server_port → assigned (service_digest, local_port)
    assignments: RwLock<HashMap<u16, (Vec<u8>, u16)>>,
    /// Pre-bound listeners, one per port. Indexed by port number.
    listeners: RwLock<BTreeMap<u16, TcpListener>>,
    /// Total capacity
    total: u16,
}

impl PortPool {
    /// Create a new pool. Pre-binds all ports in the range.
    /// Fails immediately if any port is already in use.
    pub async fn new(range: &str) -> Result<Arc<Self>> {
        let ports = parse_port_range(range)?;
        if ports.is_empty() {
            bail!("Port pool is empty");
        }

        let mut listeners = BTreeMap::new();
        let mut free = BTreeSet::new();

        for port in &ports {
            let addr: SocketAddr = format!("0.0.0.0:{}", port).parse().unwrap();
            match TcpListener::bind(addr).await {
                Ok(l) => {
                    free.insert(*port);
                    listeners.insert(*port, l);
                    debug!("Pool: pre-bound port {}", port);
                }
                Err(e) => {
                    // Release all already-bound ports
                    return Err(anyhow::anyhow!(
                        "Port {} is already in use: {}. Aborting.",
                        port,
                        e
                    ));
                }
            }
        }

        let total = ports.len() as u16;
        info!("Port pool ready: {} ports ({}–{})", total, ports[0], ports.last().unwrap());

        Ok(Arc::new(Self {
            free: RwLock::new(free),
            assignments: RwLock::new(HashMap::new()),
            listeners: RwLock::new(listeners),
            total,
        }))
    }

    /// Total number of ports in the pool.
    pub fn total(&self) -> u16 {
        self.total
    }

    /// Number of free ports.
    pub async fn free_count(&self) -> usize {
        self.free.read().await.len()
    }

    /// Number of assigned ports.
    pub async fn assigned_count(&self) -> usize {
        self.assignments.read().await.len()
    }

    /// Get all assignments for inspection.
    pub async fn assignments_snapshot(&self) -> Vec<(u16, Vec<u8>, u16)> {
        self.assignments
            .read()
            .await
            .iter()
            .map(|(server_port, (digest, local_port))| {
                (*server_port, digest.clone(), *local_port)
            })
            .collect()
    }

    /// Assign ports from the pool to expose a client's local ports.
    ///
    /// `local_ports`: sorted list of local ports the client wants to expose.
    /// `service_digest`: identifies the client.
    ///
    /// Returns a map of local_port → server_port, or an error if not enough free ports.
    pub async fn assign(
        &self,
        service_digest: &[u8],
        local_ports: &[u16],
    ) -> Result<HashMap<u16, u16>> {
        let mut free = self.free.write().await;
        let mut assignments = self.assignments.write().await;

        if free.len() < local_ports.len() {
            bail!(
                "Not enough free ports: need {}, have {}",
                local_ports.len(),
                free.len()
            );
        }

        let mut mapping = HashMap::new();
        for local_port in local_ports {
            let server_port = free.pop_first().unwrap();
            mapping.insert(*local_port, server_port);
            assignments.insert(server_port, (service_digest.to_vec(), *local_port));
            info!(
                "Port assigned: {} → {} (digest={})",
                server_port,
                local_port,
                hex::encode(service_digest),
            );
        }

        Ok(mapping)
    }

    /// Release ALL ports assigned to a client.
    pub async fn release_client(&self, service_digest: &[u8]) {
        let mut free = self.free.write().await;
        let mut assignments = self.assignments.write().await;

        let to_free: Vec<u16> = assignments
            .iter()
            .filter(|(_, (d, _))| d.as_slice() == service_digest)
            .map(|(p, _)| *p)
            .collect();

        for port in to_free {
            assignments.remove(&port);
            free.insert(port);
            info!("Port released: {}", port);
        }
    }

    /// Release a single port by number.
    pub async fn release_by_port(&self, port: u16) {
        let mut free = self.free.write().await;
        let mut assignments = self.assignments.write().await;
        if assignments.remove(&port).is_some() {
            free.insert(port);
            info!("Port released: {}", port);
        }
    }

    /// Accept a connection on an assigned port.
    /// Waits for a visitor on the pre-bound listener for `server_port`.
    pub async fn accept(
        &self,
        server_port: u16,
    ) -> Result<(tokio::net::TcpStream, SocketAddr)> {
        let listeners = self.listeners.read().await;
        let listener = listeners
            .get(&server_port)
            .with_context(|| format!("Port {} not in pool", server_port))?;

        // Accept from the pre-bound listener (blocking cancel-safe)
        listener
            .accept()
            .await
            .with_context(|| format!("Accept error on port {}", server_port))
    }

    /// Look up which (service_digest, local_port) a server_port is assigned to.
    pub async fn lookup(&self, server_port: u16) -> Option<(Vec<u8>, u16)> {
        self.assignments.read().await.get(&server_port).cloned()
    }
}
