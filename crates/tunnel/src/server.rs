use crate::config::{Config, ServerConfig, ServerServiceConfig, ServiceType, TcpConfig, TransportConfig, TransportType};
use crate::constants::{listen_backoff, UDP_BUFFER_SIZE};
use crate::helper::retry_notify_with_deadline;
use crate::multi_map::MultiMap;
use crate::protocol::Hello::{ControlChannelHello, DataChannelHello};
use crate::protocol::{
    self, read_auth, read_control_cmd, read_hello, Ack, ControlChannelCmd, DataChannelCmd, Hello, UdpTraffic,
    HASH_WIDTH_IN_BYTES,
};
use crate::registry::{self, ClientRegistry};
use crate::transport::{SocketOpts, TcpTransport, Transport};
use anyhow::{anyhow, bail, Context, Result};
use backoff::backoff::Backoff;
use backoff::ExponentialBackoff;

use rand::Rng;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{self, copy_bidirectional, AsyncReadExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};
use tokio::time;
use tracing::{debug, error, info, info_span, instrument, warn, Instrument, Span};

type ServiceDigest = protocol::Digest;
type Nonce = protocol::Digest;
/// Per-connection identifier for the control_channels map. Different from
/// `service_digest` (which is the SHA-256 of the auth token and is shared
/// across all clients using that token) so two clients on the same token
/// can both have entries.
type ChannelId = protocol::Digest;

const TCP_POOL_SIZE: usize = 8;
const UDP_POOL_SIZE: usize = 2;
const CHAN_SIZE: usize = 2048;
const HANDSHAKE_TIMEOUT: u64 = 5;

pub async fn run_server(
    config: Config,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let config = match config.server {
            Some(config) => config,
            None => {
                return Err(anyhow!("Try to run as a server, but the configuration is missing. Please add the `[server]` block"))
            }
        };

    let (node_update_tx, _) = mpsc::channel(1024);
    let bindings = registry::new_bindings();
    let mut server = Server::<TcpTransport>::from(config, None, node_update_tx, bindings).await?;
    server.run(shutdown_rx).await?;

    Ok(())
}

type ControlChannelMap<T> = MultiMap<ChannelId, Nonce, ControlChannelHandle<T>>;

/// `node_uuid → channel_id` for the currently-serving channel of each
/// machine. Used to evict a stale control channel when its machine
/// reconnects (and to confirm at cleanup time that we're removing our own
/// entry, not a newer reconnect's).
type UuidIndex = Arc<RwLock<HashMap<String, ChannelId>>>;

pub struct Server<T: Transport> {
    config: Arc<ServerConfig>,
    services: Arc<RwLock<HashMap<ServiceDigest, ServerServiceConfig>>>,
    control_channels: Arc<RwLock<ControlChannelMap<T>>>,
    uuid_index: UuidIndex,
    transport: Arc<T>,
    pub clients: ClientRegistry,
    pub bindings: crate::registry::NodeBindings,
    pub port_pool: Option<Arc<crate::port_pool::PortPool>>,
    pub node_update_tx: mpsc::Sender<crate::node_update::NodeUpdate>,
}

fn generate_service_hashmap(
    server_config: &ServerConfig,
) -> HashMap<ServiceDigest, ServerServiceConfig> {
    let mut ret = HashMap::new();
    for u in &server_config.services {
        ret.insert(protocol::digest(u.0.as_bytes()), (*u.1).clone());
    }
    ret
}

impl<T: 'static + Transport> Server<T> {
    pub async fn from(
        config: ServerConfig,
        port_pool: Option<Arc<crate::port_pool::PortPool>>,
        node_update_tx: mpsc::Sender<crate::node_update::NodeUpdate>,
        bindings: crate::registry::NodeBindings,
    ) -> Result<Server<T>> {
        let config = Arc::new(config);
        let services = Arc::new(RwLock::new(generate_service_hashmap(&config)));
        let control_channels = Arc::new(RwLock::new(ControlChannelMap::new()));
        let uuid_index: UuidIndex = Arc::new(RwLock::new(HashMap::new()));
        let transport_cfg = TransportConfig { transport_type: TransportType::Tcp, tcp: TcpConfig::default() };
        let transport = Arc::new(T::new(&transport_cfg)?);
        let clients = registry::new_registry();
        Ok(Server {
            config,
            services,
            control_channels,
            uuid_index,
            transport,
            clients,
            bindings,
            port_pool,
            node_update_tx,
        })
    }

    pub async fn run(
        &mut self,
        mut shutdown_rx: broadcast::Receiver<bool>,
    ) -> Result<()> {
        let l = self
            .transport
            .bind(&self.config.bind_addr)
            .await
            .with_context(|| "Failed to listen at `server.bind_addr`")?;
        info!("Listening at {}", self.config.bind_addr);

        let mut backoff = ExponentialBackoff {
            max_interval: Duration::from_millis(100),
            max_elapsed_time: None,
            ..Default::default()
        };

        loop {
            tokio::select! {
                ret = self.transport.accept(&l) => {
                    match ret {
                        Err(err) => {
                            if let Some(err) = err.downcast_ref::<io::Error>() {
                                if let Some(d) = backoff.next_backoff() {
                                    error!("Failed to accept: {:#}. Retry in {:?}...", err, d);
                                    time::sleep(d).await;
                                } else {
                                    error!("Too many retries. Aborting...");
                                    break;
                                }
                            }
                        }
                        Ok((conn, addr)) => {
                            backoff.reset();

                            match time::timeout(Duration::from_secs(HANDSHAKE_TIMEOUT), self.transport.handshake(conn)).await {
                                Ok(conn) => {
                                    match conn.with_context(|| "Failed to do transport handshake") {
                                        Ok(conn) => {
                                            let services = self.services.clone();
                                            let control_channels = self.control_channels.clone();
                                            let uuid_index = self.uuid_index.clone();
                                            let server_config = self.config.clone();
                                            let clients = self.clients.clone();
                                            let bindings = self.bindings.clone();
                                            let port_pool = self.port_pool.clone();
                                            let node_update_tx = self.node_update_tx.clone();
                                            tokio::spawn(async move {
                                                if let Err(err) = handle_connection(conn, services, control_channels, uuid_index, server_config, clients, bindings, port_pool, node_update_tx).await {
                                                    error!("{:#}", err);
                                                }
                                            }.instrument(info_span!("connection", %addr)));
                                        }, Err(e) => {
                                            error!("{:#}", e);
                                        }
                                    }
                                },
                                Err(e) => {
                                    error!("Transport handshake timeout: {}", e);
                                }
                            }
                        }
                    }
                },
                _ = shutdown_rx.recv() => {
                    info!("Shuting down gracefully...");
                    break;
                },
            }
        }

        info!("Shutdown");

        Ok(())
    }
}

async fn handle_connection<T: 'static + Transport>(
    mut conn: T::Stream,
    services: Arc<RwLock<HashMap<ServiceDigest, ServerServiceConfig>>>,
    control_channels: Arc<RwLock<ControlChannelMap<T>>>,
    uuid_index: UuidIndex,
    server_config: Arc<ServerConfig>,
    clients: ClientRegistry,
    bindings: crate::registry::NodeBindings,
    port_pool: Option<Arc<crate::port_pool::PortPool>>,
    node_update_tx: mpsc::Sender<crate::node_update::NodeUpdate>,
) -> Result<()> {
    let hello = read_hello(&mut conn).await?;
    match hello {
        ControlChannelHello(_, service_digest) => {
            do_control_channel_handshake(
                conn,
                services,
                control_channels,
                uuid_index,
                service_digest,
                server_config,
                clients,
                bindings,
                port_pool,
                node_update_tx,
            )
            .await?;
        }
        DataChannelHello(_, nonce) => {
            do_data_channel_handshake(conn, control_channels, nonce).await?;
        }
    }
    Ok(())
}

async fn do_control_channel_handshake<T: 'static + Transport>(
    mut conn: T::Stream,
    _services: Arc<RwLock<HashMap<ServiceDigest, ServerServiceConfig>>>,
    control_channels: Arc<RwLock<ControlChannelMap<T>>>,
    uuid_index: UuidIndex,
    service_digest: ServiceDigest,
    server_config: Arc<ServerConfig>,
    clients: ClientRegistry,
    bindings: crate::registry::NodeBindings,
    port_pool: Option<Arc<crate::port_pool::PortPool>>,
    node_update_tx: mpsc::Sender<crate::node_update::NodeUpdate>,
) -> Result<()> {
    info!("Try to handshake a control channel");

    T::hint(&conn, SocketOpts::for_control_channel());

    let mut nonce = vec![0u8; HASH_WIDTH_IN_BYTES];
    rand::rng().fill_bytes(&mut nonce);

    let hello_send = Hello::ControlChannelHello(
        protocol::CURRENT_PROTO_VERSION,
        nonce.clone().try_into().unwrap(),
    );
    protocol::write_hello(&mut conn, &hello_send).await?;

    // Token-only auth: use global default_token, no service lookup
    let mut concat = Vec::from(server_config.default_token.as_bytes());
    concat.append(&mut nonce);

    let protocol::Auth(d) = read_auth(&mut conn).await?;

    let session_key = protocol::digest(&concat);
    if session_key != d {
        protocol::write_ack(&mut conn, &Ack::AuthFailed).await?;
        debug!(
            "Expect {}, but got {}",
            hex::encode(session_key),
            hex::encode(d)
        );
        bail!("Client failed authentication");
    }

    let service_name = hex::encode(service_digest);

    // Per-connection channel id — different from `service_digest` so two
    // clients authenticating with the same token can both have entries in
    // `control_channels`. Eviction of stale entries happens later, by
    // node_uuid, after ReportNodeStatus tells us which machine this is.
    let mut channel_id = [0u8; HASH_WIDTH_IN_BYTES];
    rand::rng().fill_bytes(&mut channel_id);

    protocol::write_ack(&mut conn, &Ack::Ok).await?;

    // Build synthetic service config for the client
    let service_config = ServerServiceConfig {
        service_type: ServiceType::Tcp,
        name: service_name.clone(),
        bind_addr: "127.0.0.1:0".into(),
        token: Some(server_config.default_token.clone()),
        nodelay: None,
    };

    info!(service = %service_name, channel_id = %hex::encode(channel_id), "Control channel established");
    let handle = ControlChannelHandle::new(
        conn,
        service_config,
        server_config.heartbeat_interval,
        clients,
        bindings,
        service_digest,
        channel_id,
        control_channels.clone(),
        uuid_index,
        port_pool,
        node_update_tx,
    );

    let mut h = control_channels.write().await;
    let _ = h.insert(channel_id, session_key, handle);

    Ok(())
}

async fn do_data_channel_handshake<T: 'static + Transport>(
    conn: T::Stream,
    control_channels: Arc<RwLock<ControlChannelMap<T>>>,
    nonce: Nonce,
) -> Result<()> {
    debug!("Try to handshake a data channel");

    let (hint_opts, port_data_pending, data_ch_tx, port_data_callbacks) = {
        let guard = control_channels.read().await;
        match guard.get2(&nonce) {
            Some(handle) => (
                Some(SocketOpts::from_server_cfg(&handle.service)),
                Some(handle.port_data_pending.clone()),
                Some(handle.data_ch_tx.clone()),
                Some(handle.port_data_callbacks.clone()),
            ),
            None => {
                warn!("Data channel has incorrect nonce");
                (None, None, None, None)
            }
        }
    };

    let (hint_opts, port_data_pending, data_ch_tx, port_data_callbacks) = match (hint_opts, port_data_pending, data_ch_tx, port_data_callbacks) {
        (Some(h), Some(p), Some(tx), Some(cb)) => (h, p, tx, cb),
        _ => return Ok(()),
    };

    T::hint(&conn, hint_opts);

    // First check generic port_data_pending
    {
        let mut pending = port_data_pending.write().await;
        if let Some((sender, _local_port)) = pending.pop_front() {
            drop(pending);
            let _ = sender.send(conn);
            return Ok(());
        }
    }

    // Then check type-erased callbacks
    {
        let mut pending = port_data_callbacks.write().await;
        if let Some((cb, _local_port)) = pending.pop_front() {
            drop(pending);
            let mut guard = cb.lock().unwrap();
            if let Some(cb) = guard.take() {
                cb(Box::new(conn));
            }
            return Ok(());
        }
    }

    data_ch_tx
        .send(conn)
        .await
        .with_context(|| "Data channel for a stale control channel")?;

    Ok(())
}

pub struct ControlChannelHandle<T: Transport> {
    _shutdown_tx: broadcast::Sender<bool>,
    data_ch_tx: mpsc::Sender<T::Stream>,
    service: ServerServiceConfig,
    /// Send commands to this client's control channel
    pub cmd_tx: mpsc::Sender<ControlChannelCmd>,
    port_data_pending: Arc<RwLock<std::collections::VecDeque<(tokio::sync::oneshot::Sender<T::Stream>, u16)>>>,
    pub port_data_callbacks: Arc<RwLock<std::collections::VecDeque<(crate::registry::SyncedCallback, u16)>>>,
    pub node_update_tx: mpsc::Sender<crate::node_update::NodeUpdate>,
}

impl<T> ControlChannelHandle<T>
where
    T: 'static + Transport,
{
    #[instrument(name = "handle", skip_all, fields(service = %service.name))]
    fn new(
        conn: T::Stream,
        service: ServerServiceConfig,
        heartbeat_interval: u64,
        clients: ClientRegistry,
        bindings: crate::registry::NodeBindings,
        service_digest: ServiceDigest,
        channel_id: ChannelId,
        control_channels: Arc<RwLock<ControlChannelMap<T>>>,
        uuid_index: UuidIndex,
        port_pool: Option<Arc<crate::port_pool::PortPool>>,
        node_update_tx: mpsc::Sender<crate::node_update::NodeUpdate>,
    ) -> ControlChannelHandle<T> {
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);
        let (data_ch_tx, data_ch_rx) = mpsc::channel(CHAN_SIZE * 2);
        let (data_ch_req_tx, data_ch_req_rx) = mpsc::unbounded_channel();

        // Channel for sending commands to this client
        let (cmd_tx, cmd_rx) = mpsc::channel::<ControlChannelCmd>(64);
        let cmd_tx_for_registry = cmd_tx.clone();

        // Map for pending Docker response oneshots
        let pending_docker: Arc<RwLock<std::collections::HashMap<String, tokio::sync::oneshot::Sender<Result<Vec<u16>, String>>>>> =
            Arc::new(RwLock::new(std::collections::HashMap::new()));

        let pool_size = match service.service_type {
            ServiceType::Tcp => TCP_POOL_SIZE,
            ServiceType::Udp => UDP_POOL_SIZE,
        };

        for _i in 0..pool_size {
            if let Err(e) = data_ch_req_tx.send(true) {
                error!("Failed to request data channel {}", e);
            };
        }

        let data_ch_req_tx_for_control = data_ch_req_tx.clone();

        let shutdown_rx_clone = shutdown_tx.subscribe();
        let bind_addr = service.bind_addr.clone();
        let shutdown_tx_clone = shutdown_tx.clone();
        let digest_for_drop = service_digest;
        let clients_for_drop = clients.clone();
        let control_channels_for_drop = control_channels.clone();
        let uuid_index_for_drop = uuid_index.clone();

        match service.service_type {
            ServiceType::Tcp => tokio::spawn(
                async move {
                    if let Err(e) = run_tcp_connection_pool::<T>(
                        bind_addr,
                        data_ch_rx,
                        data_ch_req_tx,
                        shutdown_rx_clone,
                    )
                    .await
                    .with_context(|| "Failed to run TCP connection pool")
                    {
                        error!("{:#}", e);
                    }
                }
                .instrument(Span::current()),
            ),
            ServiceType::Udp => tokio::spawn(
                async move {
                    if let Err(e) = run_udp_connection_pool::<T>(
                        bind_addr,
                        data_ch_rx,
                        data_ch_req_tx,
                        shutdown_rx_clone,
                    )
                    .await
                    .with_context(|| "Failed to run UDP connection pool")
                    {
                        error!("{:#}", e);
                    }
                }
                .instrument(Span::current()),
            ),
        };

        let port_data_pending = Arc::new(RwLock::new(VecDeque::new()));
        let port_data_callbacks = Arc::new(RwLock::new(VecDeque::new()));

        // Shared with the run loop: filled in once the client reports its
        // node_uuid. Used by the cleanup task to remove the registry entry on
        // disconnect.
        let node_uuid_slot: Arc<tokio::sync::Mutex<Option<String>>> =
            Arc::new(tokio::sync::Mutex::new(None));

        let service_name = service.name.clone();
        let ch = ControlChannel::<T> {
            conn,
            shutdown_rx,
            data_ch_req_rx,
            heartbeat_interval,
            cmd_rx,
            cmd_tx: cmd_tx_for_registry,
            service_digest: digest_for_drop,
            service_name,
            channel_id,
            clients,
            bindings,
            uuid_index: uuid_index.clone(),
            control_channels: control_channels.clone(),
            port_data_pending: port_data_pending.clone(),
            port_data_callbacks: port_data_callbacks.clone(),
            port_pool,
            data_ch_req_tx: data_ch_req_tx_for_control,
            shutdown_tx: shutdown_tx.clone(),
            node_update_tx: node_update_tx.clone(),
            hostname: None,
            node_uuid: None,
            node_uuid_slot: node_uuid_slot.clone(),
            pending_docker: pending_docker.clone(),
        };

        tokio::spawn(
            async move {
                if let Err(err) = ch.run().await {
                    error!("{:#}", err);
                }
                let _ = shutdown_tx_clone.send(true);
                let uuid = node_uuid_slot.lock().await.clone();
                if let Some(uuid) = uuid {
                    // Only clear the uuid_index entry if it's still ours —
                    // a newer reconnect for the same uuid may have taken over.
                    let mut idx = uuid_index_for_drop.write().await;
                    if idx.get(&uuid).copied() == Some(channel_id) {
                        idx.remove(&uuid);
                    }
                    drop(idx);
                    registry::remove(&clients_for_drop, &uuid).await;
                }
                // Always drop our control_channels entry — keyed by our
                // unique channel_id, so this can't clobber anyone else.
                let _ = control_channels_for_drop.write().await.remove1(&channel_id);
            }
            .instrument(Span::current()),
        );

        ControlChannelHandle {
            _shutdown_tx: shutdown_tx,
            data_ch_tx,
            service,
            cmd_tx,
            port_data_pending,
            port_data_callbacks,
            node_update_tx,
        }
    }
}

struct ControlChannel<T: Transport> {
    conn: T::Stream,
    shutdown_rx: broadcast::Receiver<bool>,
    data_ch_req_rx: mpsc::UnboundedReceiver<bool>,
    heartbeat_interval: u64,
    cmd_rx: mpsc::Receiver<ControlChannelCmd>,
    cmd_tx: mpsc::Sender<ControlChannelCmd>,
    #[allow(dead_code)]
    service_digest: ServiceDigest,
    service_name: String,
    /// Per-connection identifier — primary key in `control_channels`. Lets
    /// us evict the *previous* channel for a given node_uuid (same machine
    /// reconnecting) without disturbing channels from other machines that
    /// happen to share the same auth token.
    channel_id: ChannelId,
    clients: ClientRegistry,
    bindings: crate::registry::NodeBindings,
    uuid_index: UuidIndex,
    /// Map of all control channels (keyed by channel_id). Held so we can
    /// evict a peer entry — used when a same-uuid reconnect comes in.
    control_channels: Arc<RwLock<ControlChannelMap<T>>>,
    port_data_pending: Arc<RwLock<VecDeque<(oneshot::Sender<T::Stream>, u16)>>>,
    port_data_callbacks: Arc<RwLock<VecDeque<(crate::registry::SyncedCallback, u16)>>>,
    port_pool: Option<Arc<crate::port_pool::PortPool>>,
    data_ch_req_tx: mpsc::UnboundedSender<bool>,
    shutdown_tx: broadcast::Sender<bool>,
    node_update_tx: mpsc::Sender<crate::node_update::NodeUpdate>,
    hostname: Option<String>,
    /// Stable per-machine UUID once known (received in ReportNodeStatus, or
    /// generated server-side and sent back via AssignNodeUuid).
    node_uuid: Option<String>,
    /// Mirror of `node_uuid` shared with the cleanup task so it can find the
    /// registry entry to remove when the channel exits.
    node_uuid_slot: Arc<tokio::sync::Mutex<Option<String>>>,
    pending_docker: Arc<RwLock<std::collections::HashMap<String, tokio::sync::oneshot::Sender<Result<Vec<u16>, String>>>>>,
}

impl<T: Transport> ControlChannel<T> {
    #[instrument(skip_all)]
    async fn run(mut self) -> Result<()> {
        let (mut rd, wr) = io::split(self.conn);
        let wr = Arc::new(tokio::sync::Mutex::new(wr));

        let _pool = self.port_pool.clone();
        let _data_ch_req_tx = self.data_ch_req_tx.clone();
        let _port_data_pending = self.port_data_pending.clone();
        let _shutdown_tx = self.shutdown_tx.clone();

        loop {
            tokio::select! {
                val = read_control_cmd(&mut rd) => {
                    match val {
                        Ok(ControlChannelCmd::ReportNodeStatus { node_uuid, hostname, os, arch, docker_version, port_range_start, port_range_end, cpu_cores, memory_mb, running_containers }) => {
                            // Resolve the uuid this client should be known as.
                            //
                            // `bindings: node_uuid → service_digest_hex`.
                            //   * A claim of a uuid that's bound to a *different* digest is
                            //     rejected (cross-token spoof protection).
                            //   * A claim of a uuid bound to *our* digest, or to nobody, is
                            //     accepted.
                            //   * No claim + we have an *offline* bound uuid → restore it
                            //     (file-loss recovery).
                            //   * No claim + all our bound uuids are currently online (i.e.
                            //     a second machine sharing the same token) → fresh uuid.
                            let digest_hex = hex::encode(self.service_digest);
                            let claimed = node_uuid.as_deref().and_then(|s| {
                                let trimmed = s.trim();
                                if trimmed.is_empty() {
                                    None
                                } else {
                                    uuid::Uuid::parse_str(trimmed).ok().map(|_| trimmed.to_string())
                                }
                            });

                            enum Resolve { Accept(String), AssignAndUse(String) }
                            // Snapshot currently-online uuids so the binding decision can
                            // distinguish "primary machine lost its file" from "second
                            // machine sharing the token".
                            let online_uuids: std::collections::HashSet<String> = self
                                .clients
                                .read()
                                .await
                                .keys()
                                .cloned()
                                .collect();
                            let resolved = if let Some(existing) = self.node_uuid.clone() {
                                // We've already resolved an identity for this channel —
                                // a follow-up ReportNodeStatus (e.g., a heartbeat fired
                                // before the client processed our AssignNodeUuid) must
                                // not be allowed to drift onto a different uuid.
                                Resolve::Accept(existing)
                            } else {
                                let mut map = self.bindings.write().await;
                                match claimed {
                                    Some(c) => match map.get(&c) {
                                        Some(d) if d == &digest_hex => Resolve::Accept(c),
                                        Some(d) => {
                                            warn!(
                                                "Client {} ({}) claimed uuid={} which is bound to a different service_digest ({}); issuing a fresh uuid",
                                                self.service_name, hostname, c, d
                                            );
                                            let n = uuid::Uuid::new_v4().to_string();
                                            map.insert(n.clone(), digest_hex.clone());
                                            Resolve::AssignAndUse(n)
                                        }
                                        None => {
                                            info!(
                                                "Binding uuid={} to service_digest={} ({})",
                                                c, digest_hex, hostname
                                            );
                                            map.insert(c.clone(), digest_hex.clone());
                                            Resolve::Accept(c)
                                        }
                                    },
                                    None => {
                                        // File-loss recovery is only safe when a digest
                                        // owns exactly one bound uuid — otherwise we'd
                                        // have to guess which machine is which and would
                                        // get it wrong half the time. For multi-machine
                                        // (shared-token) deployments, just assign fresh
                                        // and let the new row sit alongside the orphan.
                                        let bound_for_digest: Vec<String> = map
                                            .iter()
                                            .filter(|(_, d)| *d == &digest_hex)
                                            .map(|(u, _)| u.clone())
                                            .collect();
                                        let solo_offline = match bound_for_digest.as_slice() {
                                            [only] if !online_uuids.contains(only) => {
                                                Some(only.clone())
                                            }
                                            _ => None,
                                        };
                                        match solo_offline {
                                            Some(u) => {
                                                info!(
                                                    "Restoring uuid={} for {} ({}); client had no persisted node_id",
                                                    u, self.service_name, hostname
                                                );
                                                Resolve::AssignAndUse(u)
                                            }
                                            None => {
                                                let n = uuid::Uuid::new_v4().to_string();
                                                info!(
                                                    "Assigning uuid={} to {} ({})",
                                                    n, self.service_name, hostname
                                                );
                                                map.insert(n.clone(), digest_hex.clone());
                                                Resolve::AssignAndUse(n)
                                            }
                                        }
                                    }
                                }
                            };

                            let uuid = match resolved {
                                Resolve::Accept(u) => u,
                                Resolve::AssignAndUse(u) => {
                                    let _ = self.cmd_tx
                                        .send(ControlChannelCmd::AssignNodeUuid { uuid: u.clone() })
                                        .await;
                                    u
                                }
                            };

                            // If this connection now owns a different UUID than a previous
                            // ReportNodeStatus on the same channel, drop the old registry entry.
                            if let Some(prev) = self.node_uuid.as_ref() {
                                if prev != &uuid {
                                    crate::registry::remove(&self.clients, prev).await;
                                }
                            }

                            // Evict any *other* control channel that's currently registered for
                            // this uuid. This is the per-machine equivalent of the old
                            // remove-by-service_digest at handshake time: two channels from the
                            // same machine (e.g., after a network blip + reconnect) shouldn't
                            // coexist, but two channels from different machines on the same auth
                            // token should.
                            let prev_channel_id = {
                                let mut idx = self.uuid_index.write().await;
                                let prev = idx.get(&uuid).copied();
                                idx.insert(uuid.clone(), self.channel_id);
                                prev
                            };
                            if let Some(prev_id) = prev_channel_id {
                                if prev_id != self.channel_id {
                                    warn!(
                                        "Evicting previous control channel for uuid={} (was channel_id={})",
                                        uuid, hex::encode(prev_id)
                                    );
                                    let _ = self.control_channels.write().await.remove1(&prev_id);
                                }
                            }

                            info!(
                                "Client status: uuid={} {} {} {} docker={} ports={}-{} cpu={} mem={}MB containers={}",
                                uuid, hostname, os, arch, docker_version,
                                port_range_start, port_range_end,
                                cpu_cores, memory_mb,
                                running_containers.len()
                            );
                            registry::upsert(
                                &self.clients,
                                uuid.clone(),
                                hostname.clone(),
                                os.clone(),
                                arch.clone(),
                                docker_version.clone(),
                                port_range_start,
                                port_range_end,
                                cpu_cores,
                                memory_mb,
                                running_containers.clone(),
                                self.cmd_tx.clone(),
                                self.pending_docker.clone(),
                                self.data_ch_req_tx.clone(),
                                self.port_data_callbacks.clone(),
                            ).await;

                            self.node_uuid = Some(uuid.clone());
                            *self.node_uuid_slot.lock().await = Some(uuid.clone());

                            let _ = self.node_update_tx.send(crate::node_update::NodeUpdate {
                                uuid: uuid.clone(),
                                event: crate::node_update::NodeEvent::Connected {
                                    service_digest: digest_hex.clone(),
                                    hostname: hostname.clone(),
                                    os,
                                    arch,
                                    docker_version,
                                    port_range_start,
                                    port_range_end,
                                    cpu_cores,
                                    memory_mb,
                                    running_containers: running_containers.clone(),
                                },
                            }).await;
                            self.hostname = Some(hostname);

                            // Resolve any pending Docker oneshots for containers now running
                            let mut pending = self.pending_docker.write().await;
                            for c in &running_containers {
                                if let Some(tx) = pending.remove(&c.container_name) {
                                    let _ = tx.send(Ok(c.ports.clone()));
                                }
                            }
                        }
                        Ok(ControlChannelCmd::ContainerStarted { container_name, ports }) => {
                            info!("Container started: {} ports:{:?}", container_name, ports);
                            let mut pending = self.pending_docker.write().await;
                            if let Some(tx) = pending.remove(&container_name) {
                                let _ = tx.send(Ok(ports.clone()));
                            }
                            if let Some(uuid) = self.node_uuid.clone() {
                                let _ = self.node_update_tx.send(crate::node_update::NodeUpdate {
                                    uuid,
                                    event: crate::node_update::NodeEvent::ContainerStarted {
                                        container_name: container_name.clone(),
                                        ports,
                                    },
                                }).await;
                            }
                        }
                        Ok(ControlChannelCmd::ContainerStopped { container_name }) => {
                            info!("Container stopped: {}", container_name);
                            let mut pending = self.pending_docker.write().await;
                            if let Some(tx) = pending.remove(&container_name) {
                                let _ = tx.send(Ok(vec![]));
                            }
                            if let Some(uuid) = self.node_uuid.clone() {
                                let _ = self.node_update_tx.send(crate::node_update::NodeUpdate {
                                    uuid,
                                    event: crate::node_update::NodeEvent::ContainerStopped {
                                        container_name: container_name.clone(),
                                    },
                                }).await;
                            }
                        }
                        Ok(ControlChannelCmd::ContainerError { container_name, error }) => {
                            error!("Container error: {} — {}", container_name, error);
                            let mut pending = self.pending_docker.write().await;
                            if let Some(tx) = pending.remove(&container_name) {
                                let _ = tx.send(Err(error.clone()));
                            }
                            if let Some(uuid) = self.node_uuid.clone() {
                                let _ = self.node_update_tx.send(crate::node_update::NodeUpdate {
                                    uuid,
                                    event: crate::node_update::NodeEvent::ContainerError {
                                        container_name: container_name.clone(),
                                        error: error.clone(),
                                    },
                                });
                            }
                        }
                        Ok(_) => {
                            debug!("Unexpected control cmd from client");
                        }
                        Err(e) => {
                            debug!("Control channel read error: {:#}", e);
                            break;
                        }
                    }
                },
                val = self.data_ch_req_rx.recv() => {
                    match val {
                        Some(_) => {
                            let mut guard = wr.lock().await;
                            if let Err(e) = protocol::write_control_cmd(
                                &mut *guard,
                                &ControlChannelCmd::CreateDataChannel,
                            ).await {
                                error!("{:#}", e);
                                break;
                            }
                        }
                        None => break,
                    }
                },
                _ = time::sleep(Duration::from_secs(self.heartbeat_interval)), if self.heartbeat_interval != 0 => {
                    let mut guard = wr.lock().await;
                    if let Err(e) = protocol::write_control_cmd(
                        &mut *guard,
                        &ControlChannelCmd::HeartBeat,
                    ).await {
                        error!("{:#}", e);
                        break;
                    }
                },
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(cmd) => {
                            let mut guard = wr.lock().await;
                            if let Err(e) = protocol::write_control_cmd(&mut *guard, &cmd).await {
                                error!("{:#}", e);
                                break;
                            }
                        }
                        None => break,
                    }
                },
                _ = self.shutdown_rx.recv() => {
                    break;
                }
            }
        }

        info!("Control channel shutdown");

        if let (Some(hostname), Some(uuid)) = (self.hostname.take(), self.node_uuid.clone()) {
            let _ = self.node_update_tx.send(crate::node_update::NodeUpdate {
                uuid,
                event: crate::node_update::NodeEvent::Disconnected { hostname },
            }).await;
        }

        Ok(())
    }
}

pub fn spawn_port_accept_loop(
    pool: Arc<crate::port_pool::PortPool>,
    server_port: u16,
    local_port: u16,
    data_ch_req_tx: mpsc::UnboundedSender<bool>,
    port_data_callbacks: Arc<RwLock<VecDeque<(crate::registry::SyncedCallback, u16)>>>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) {
    tokio::spawn(
        async move {
            use tokio::net::TcpStream;
            loop {
                tokio::select! {
                    result = pool.accept(server_port) => {
                        match result {
                            Ok((mut visitor, addr)) => {
                                debug!("Port {} visitor from {}", server_port, addr);
                                if data_ch_req_tx.send(true).is_err() {
                                    break;
                                }
                                let (tx, rx) = oneshot::channel::<TcpStream>();
                                let cb: crate::registry::ForwardCallback = Box::new(move |stream: Box<dyn std::any::Any + Send>| {
                                    if let Ok(tcp) = stream.downcast::<TcpStream>() {
                                        let _ = tx.send(*tcp);
                                    }
                                });
                                let synced = std::sync::Mutex::new(Some(cb));
                                port_data_callbacks.write().await.push_back((synced, local_port));
                                match rx.await {
                                    Ok(mut data_ch) => {
                                        if let Err(e) = protocol::write_data_cmd(
                                            &mut data_ch,
                                            &DataChannelCmd::StartForwardTcp(Some(local_port)),
                                        ).await {
                                            error!("Failed to write StartForwardTcp: {:#}", e);
                                            continue;
                                        }
                                        let _ = copy_bidirectional(&mut data_ch, &mut visitor).await;
                                    }
                                    Err(_) => debug!("Port data channel request cancelled"),
                                }
                            }
                            Err(e) => {
                                error!("Accept error on port {}: {:#}", server_port, e);
                                break;
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        break;
                    }
                }
            }
            info!("Port {} accept loop shutdown", server_port);
        }
        .instrument(Span::current()),
    );
}

pub fn spawn_port_udp_loop(
    server_port: u16,
    data_ch_req_tx: mpsc::UnboundedSender<bool>,
    port_data_callbacks: Arc<RwLock<VecDeque<(crate::registry::SyncedCallback, u16)>>>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) {
    tokio::spawn(
        async move {
            use tokio::net::TcpStream;
            let socket = match UdpSocket::bind(format!("0.0.0.0:{}", server_port)).await {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to bind UDP port {}: {:#}", server_port, e);
                    return;
                }
            };
            info!("UDP port accept loop started on {}", server_port);

            // Request one data channel for UDP traffic
            if data_ch_req_tx.send(true).is_err() {
                return;
            }
            let (tx, rx) = oneshot::channel::<TcpStream>();
            let cb: crate::registry::ForwardCallback = Box::new(move |stream: Box<dyn std::any::Any + Send>| {
                if let Ok(tcp) = stream.downcast::<TcpStream>() {
                    let _ = tx.send(*tcp);
                }
            });
            let synced = std::sync::Mutex::new(Some(cb));
            port_data_callbacks.write().await.push_back((synced, server_port));

            let mut conn = match rx.await {
                Ok(c) => c,
                Err(_) => {
                    debug!("UDP data channel request cancelled");
                    return;
                }
            };

            if let Err(e) = protocol::write_data_cmd(&mut conn, &DataChannelCmd::StartForwardUdp).await {
                error!("Failed to write StartForwardUdp: {:#}", e);
                return;
            }

            let (mut rd, mut wr) = io::split(conn);
            let mut buf = [0u8; UDP_BUFFER_SIZE];

            loop {
                tokio::select! {
                    val = socket.recv_from(&mut buf) => {
                        match val {
                            Ok((n, from)) => {
                                if let Err(e) = UdpTraffic::write_slice(&mut wr, from, &buf[..n]).await {
                                    error!("UDP write error: {:#}", e);
                                    break;
                                }
                            }
                            Err(e) => {
                                error!("UDP recv error: {:#}", e);
                                break;
                            }
                        }
                    }
                    hdr_len = rd.read_u8() => {
                        match hdr_len {
                            Ok(len) => {
                                match UdpTraffic::read(&mut rd, len).await {
                                    Ok(t) => {
                                        let _ = socket.send_to(&t.data, t.from).await;
                                    }
                                    Err(e) => {
                                        error!("UDP read error: {:#}", e);
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                error!("UDP read_hdr error: {:#}", e);
                                break;
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        break;
                    }
                }
            }
            info!("UDP port {} accept loop shutdown", server_port);
        }
        .instrument(Span::current()),
    );
}

fn tcp_listen_and_send(
    addr: String,
    data_ch_req_tx: mpsc::UnboundedSender<bool>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> mpsc::Receiver<TcpStream> {
    let (tx, rx) = mpsc::channel(CHAN_SIZE);

    tokio::spawn(async move {
        let l = retry_notify_with_deadline(listen_backoff(),  || async {
            Ok(TcpListener::bind(&addr).await?)
        }, |e, duration| {
            error!("{:#}. Retry in {:?}", e, duration);
        }, &mut shutdown_rx).await
        .with_context(|| "Failed to listen for the service");

        let l: TcpListener = match l {
            Ok(v) => v,
            Err(e) => {
                error!("{:#}", e);
                return;
            }
        };

        info!("Listening at {}", &addr);

        let mut backoff = ExponentialBackoff {
            max_interval: Duration::from_secs(1),
            max_elapsed_time: None,
            ..Default::default()
        };

        loop {
            tokio::select! {
                val = l.accept() => {
                    match val {
                        Err(e) => {
                            error!("{}. Sleep for a while", e);
                            if let Some(d) = backoff.next_backoff() {
                                time::sleep(d).await;
                            } else {
                                error!("Too many retries. Aborting...");
                                break;
                            }
                        }
                        Ok((incoming, addr)) => {
                            if data_ch_req_tx.send(true).with_context(|| "Failed to send data chan create request").is_err() {
                                break;
                            }

                            backoff.reset();

                            debug!("New visitor from {}", addr);

                            let _ = tx.send(incoming).await;
                        }
                    }
                },
                _ = shutdown_rx.recv() => {
                    break;
                }
            }
        }

        info!("TCPListener shutdown");
    }.instrument(Span::current()));

    rx
}

#[instrument(skip_all)]
async fn run_tcp_connection_pool<T: Transport>(
    bind_addr: String,
    mut data_ch_rx: mpsc::Receiver<T::Stream>,
    data_ch_req_tx: mpsc::UnboundedSender<bool>,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let mut visitor_rx = tcp_listen_and_send(bind_addr, data_ch_req_tx.clone(), shutdown_rx);

    'pool: while let Some(mut visitor) = visitor_rx.recv().await {
        loop {
            if let Some(mut ch) = data_ch_rx.recv().await {
                if protocol::write_data_cmd(&mut ch, &DataChannelCmd::StartForwardTcp(None)).await.is_ok() {
                    tokio::spawn(async move {
                        let _ = copy_bidirectional(&mut ch, &mut visitor).await;
                    });
                    break;
                } else {
                    if data_ch_req_tx.send(true).is_err() {
                        break 'pool;
                    }
                }
            } else {
                break 'pool;
            }
        }
    }

    info!("Shutdown");
    Ok(())
}

#[instrument(skip_all)]
async fn run_udp_connection_pool<T: Transport>(
    bind_addr: String,
    mut data_ch_rx: mpsc::Receiver<T::Stream>,
    _data_ch_req_tx: mpsc::UnboundedSender<bool>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let l = retry_notify_with_deadline(
        listen_backoff(),
        || async { Ok(UdpSocket::bind(&bind_addr).await?) },
        |e, duration| {
            warn!("{:#}. Retry in {:?}", e, duration);
        },
        &mut shutdown_rx,
    )
    .await
    .with_context(|| "Failed to listen for the service")?;

    info!("Listening at {}", &bind_addr);

    let mut conn = data_ch_rx
        .recv()
        .await
        .ok_or_else(|| anyhow!("No available data channels"))?;
    protocol::write_data_cmd(&mut conn, &DataChannelCmd::StartForwardUdp).await?;

    let mut buf = [0u8; UDP_BUFFER_SIZE];
    loop {
        tokio::select! {
            val = l.recv_from(&mut buf) => {
                let (n, from) = val?;
                UdpTraffic::write_slice(&mut conn, from, &buf[..n]).await?;
            },

            hdr_len = conn.read_u8() => {
                let t = UdpTraffic::read(&mut conn, hdr_len?).await?;
                l.send_to(&t.data, t.from).await?;
            }

            _ = shutdown_rx.recv() => {
                break;
            }
        }
    }

    debug!("UDP pool dropped");

    Ok(())
}
