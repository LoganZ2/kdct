use crate::config::{ClientConfig, ClientServiceConfig, Config, TcpConfig, TransportConfig, TransportType};
use crate::helper::udp_connect;
use crate::protocol::Hello::{self, *};
use crate::protocol::{
    self, read_ack, read_control_cmd, read_data_cmd, read_hello, Ack, Auth, ControlChannelCmd,
    DataChannelCmd, UdpTraffic, CURRENT_PROTO_VERSION, HASH_WIDTH_IN_BYTES,
};
use crate::transport::{AddrMaybeCached, SocketOpts, TcpTransport, Transport};
use anyhow::{anyhow, bail, Context, Result};
use backoff::backoff::Backoff;
use backoff::future::retry_notify;
use backoff::ExponentialBackoff;
use bytes::{Bytes, BytesMut};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{self, copy_bidirectional, AsyncReadExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};
use tokio::time::{self, Duration, Instant};
use tracing::{debug, error, info, instrument, trace, warn, Instrument, Span};

use crate::constants::{run_control_chan_backoff, UDP_BUFFER_SIZE, UDP_SENDQ_SIZE, UDP_TIMEOUT};

#[derive(Debug, Clone)]
struct TrackedContainer {
    container_name: String,
    image_tag: String,
}

/// Persisted per-machine identity. Lives at `~/.kdct/node_id` (one UUID per
/// line) and is loaded/saved by the client. The first time a new install
/// connects, the file is missing and the server assigns a UUID via
/// `AssignNodeUuid`, which the client then writes here.
struct NodeIdentity;

impl NodeIdentity {
    fn path() -> Result<PathBuf> {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .map_err(|_| anyhow!("neither HOME nor USERPROFILE is set; cannot locate ~/.kdct/node_id"))?;
        if home.is_empty() {
            bail!("HOME/USERPROFILE is empty; cannot locate ~/.kdct/node_id");
        }
        Ok(PathBuf::from(home).join(".kdct").join("node_id"))
    }

    fn load() -> Option<String> {
        let raw = std::fs::read_to_string(Self::path().ok()?).ok()?;
        let s = raw.trim();
        if s.is_empty() { None } else { Some(s.to_string()) }
    }

    /// Persist the assigned uuid. Returns an error if writing fails — callers
    /// must treat this as fatal, because a client that runs without a stable
    /// persisted uuid will be assigned a fresh one on every reconnect and
    /// silently spawn orphan rows in the server's DB.
    fn save(uuid: &str) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        std::fs::write(&path, uuid)
            .with_context(|| format!("Failed to persist node_id at {}", path.display()))?;
        // Restrict to owner read/write — the uuid is treated as an identity
        // claim by the server, so it shouldn't leak to other local users.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

struct ImageCache {
    entries: HashMap<String, i64>,
}

impl ImageCache {
    fn path() -> PathBuf {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".kdct").join("image_cache.txt")
    }

    fn load() -> Self {
        let entries = match std::fs::read_to_string(Self::path()) {
            Ok(content) => content
                .lines()
                .filter_map(|line| {
                    let mut parts = line.splitn(2, '|');
                    let image = parts.next()?.to_string();
                    let ts: i64 = parts.next()?.parse().ok()?;
                    Some((image, ts))
                })
                .collect(),
            Err(_) => HashMap::new(),
        };
        Self { entries }
    }

    fn save(&self) {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let content: String = self
            .entries
            .iter()
            .map(|(k, v)| format!("{}|{}", k, v))
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::write(&path, content);
    }

    fn record(&mut self, image: &str) {
        self.entries
            .insert(image.to_string(), std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64);
        self.save();
    }

    fn clear_expired(&mut self, _ttl_secs: i64) {
        self.entries.clear();
        self.save();
    }
}

pub async fn run_client(
    config: Config,
    shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    let config = config.client.ok_or_else(|| {
        anyhow!(
        "Try to run as a client, but the configuration is missing. Please add the `[client]` block"
    )
    })?;

    let mut client = Client::<TcpTransport>::from(config).await?;
    client.run(shutdown_rx).await
}

type ServiceDigest = protocol::Digest;
type Nonce = protocol::Digest;

pub struct Client<T: Transport> {
    config: ClientConfig,
    service_handles: HashMap<String, ControlChannelHandle>,
    transport: Arc<T>,
}

impl<T: 'static + Transport> Client<T> {
    pub async fn from(config: ClientConfig) -> Result<Client<T>> {
        let transport_cfg = TransportConfig { transport_type: TransportType::Tcp, tcp: TcpConfig::default() };
        let transport =
            Arc::new(T::new(&transport_cfg).with_context(|| "Failed to create the transport")?);
        Ok(Client {
            config,
            service_handles: HashMap::new(),
            transport,
        })
    }

    pub async fn run(
        &mut self,
        mut shutdown_rx: broadcast::Receiver<bool>,
    ) -> Result<()> {
        let services: Vec<(String, ClientServiceConfig)> = self.config.services
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        if services.is_empty() {
            return Err(anyhow!(
                "No services configured in [client.services]. Please add at least one service with port_range_start and port_range_end."
            ));
        }

        for (name, config) in &services {
            let handle = ControlChannelHandle::new(
                (*config).clone(),
                self.config.remote_addr.clone(),
                self.transport.clone(),
                self.config.heartbeat_timeout,
            );
            self.service_handles.insert(name.clone(), handle);
        }

        loop {
            tokio::select! {
                val = shutdown_rx.recv() => {
                    match val {
                        Ok(_) => {}
                        Err(err) => {
                            error!("Unable to listen for shutdown signal: {}", err);
                        }
                    }
                    break;
                },
            }
        }

        for (_, handle) in self.service_handles.drain() {
            handle.shutdown();
        }

        Ok(())
    }
}

struct RunDataChannelArgs<T: Transport> {
    session_key: Nonce,
    remote_addr: AddrMaybeCached,
    connector: Arc<T>,
    socket_opts: SocketOpts,
    service: ClientServiceConfig,
}

async fn do_data_channel_handshake<T: Transport>(
    args: Arc<RunDataChannelArgs<T>>,
) -> Result<T::Stream> {
    let backoff = ExponentialBackoff {
        max_interval: Duration::from_millis(100),
        max_elapsed_time: Some(Duration::from_secs(10)),
        ..Default::default()
    };

    let mut conn: T::Stream = retry_notify(
        backoff,
        || async {
            args.connector
                .connect(&args.remote_addr)
                .await
                .with_context(|| format!("Failed to connect to {}", &args.remote_addr))
                .map_err(backoff::Error::transient)
        },
        |e, duration| {
            warn!("{:#}. Retry in {:?}", e, duration);
        },
    )
    .await?;

    T::hint(&conn, args.socket_opts);

    let v: &[u8; HASH_WIDTH_IN_BYTES] = args.session_key[..].try_into().unwrap();
    let hello = Hello::DataChannelHello(CURRENT_PROTO_VERSION, v.to_owned());
    protocol::write_hello(&mut conn, &hello).await?;

    Ok(conn)
}

async fn run_data_channel<T: Transport>(args: Arc<RunDataChannelArgs<T>>) -> Result<()> {
    let mut conn = do_data_channel_handshake(args.clone()).await?;

    match read_data_cmd(&mut conn).await? {
        DataChannelCmd::StartForwardTcp(local_port) => {
            let addr = match local_port {
                Some(port) => format!("127.0.0.1:{}", port),
                None => args.service.local_addr.clone().ok_or_else(|| {
                    anyhow!(
                        "Server requested forwarding without a port and no `local_addr` is set \
                         on [client.services.{}]",
                        args.service.name
                    )
                })?,
            };
            run_data_channel_for_tcp::<T>(conn, &addr).await?;
        }
        DataChannelCmd::StartForwardUdp => {
            let addr = args.service.local_addr.clone().ok_or_else(|| {
                anyhow!(
                    "Server requested UDP forwarding but no `local_addr` is set on \
                     [client.services.{}]",
                    args.service.name
                )
            })?;
            run_data_channel_for_udp::<T>(conn, &addr, args.service.prefer_ipv6).await?;
        }
    }
    Ok(())
}

#[instrument(skip(conn))]
async fn run_data_channel_for_tcp<T: Transport>(
    mut conn: T::Stream,
    local_addr: &str,
) -> Result<()> {
    debug!("New data channel starts forwarding");

    let mut local = TcpStream::connect(local_addr)
        .await
        .with_context(|| format!("Failed to connect to {}", local_addr))?;
    let _ = copy_bidirectional(&mut conn, &mut local).await;
    Ok(())
}

type UdpPortMap = Arc<RwLock<HashMap<SocketAddr, mpsc::Sender<Bytes>>>>;

#[instrument(skip(conn))]
async fn run_data_channel_for_udp<T: Transport>(conn: T::Stream, local_addr: &str, prefer_ipv6: bool) -> Result<()> {
    debug!("New data channel starts forwarding");

    let port_map: UdpPortMap = Arc::new(RwLock::new(HashMap::new()));

    let (outbound_tx, mut outbound_rx) = mpsc::channel::<UdpTraffic>(UDP_SENDQ_SIZE);

    let (mut rd, mut wr) = io::split(conn);

    tokio::spawn(async move {
        while let Some(t) = outbound_rx.recv().await {
            trace!("outbound {:?}", t);
            if let Err(e) = t
                .write(&mut wr)
                .await
                .with_context(|| "Failed to forward UDP traffic to the server")
            {
                debug!("{:?}", e);
                break;
            }
        }
    });

    loop {
        let hdr_len = rd.read_u8().await?;
        let packet = UdpTraffic::read(&mut rd, hdr_len)
            .await
            .with_context(|| "Failed to read UDPTraffic from the server")?;
        let m = port_map.read().await;

        if m.get(&packet.from).is_none() {
            drop(m);

            let mut m = port_map.write().await;

            match udp_connect(local_addr, prefer_ipv6).await {
                Ok(s) => {
                    let (inbound_tx, inbound_rx) = mpsc::channel(UDP_SENDQ_SIZE);
                    m.insert(packet.from, inbound_tx);
                    tokio::spawn(run_udp_forwarder(
                        s,
                        inbound_rx,
                        outbound_tx.clone(),
                        packet.from,
                        port_map.clone(),
                    ));
                }
                Err(e) => {
                    error!("{:#}", e);
                }
            }
        }

        let m = port_map.read().await;
        if let Some(tx) = m.get(&packet.from) {
            let _ = tx.send(packet.data).await;
        }
    }
}

#[instrument(skip_all, fields(from))]
async fn run_udp_forwarder(
    s: UdpSocket,
    mut inbound_rx: mpsc::Receiver<Bytes>,
    outbount_tx: mpsc::Sender<UdpTraffic>,
    from: SocketAddr,
    port_map: UdpPortMap,
) -> Result<()> {
    debug!("Forwarder created");
    let mut buf = BytesMut::new();
    buf.resize(UDP_BUFFER_SIZE, 0);

    loop {
        tokio::select! {
            data = inbound_rx.recv() => {
                if let Some(data) = data {
                    s.send(&data).await?;
                } else {
                    break;
                }
            },

            val = s.recv(&mut buf) => {
                let len = match val {
                    Ok(v) => v,
                    Err(_) => break
                };

                let t = UdpTraffic{
                    from,
                    data: Bytes::copy_from_slice(&buf[..len])
                };

                outbount_tx.send(t).await?;
            },

            _ = time::sleep(Duration::from_secs(UDP_TIMEOUT)) => {
                break;
            }
        }
    }

    let mut port_map = port_map.write().await;
    port_map.remove(&from);

    debug!("Forwarder dropped");
    Ok(())
}

struct ControlChannel<T: Transport> {
    digest: ServiceDigest,
    service: ClientServiceConfig,
    shutdown_rx: oneshot::Receiver<u8>,
    remote_addr: String,
    transport: Arc<T>,
    heartbeat_timeout: u64,
    /// Tracked containers (survives across reconnections)
    containers: Arc<Mutex<Vec<TrackedContainer>>>,
    /// Image pull cache
    image_cache: Arc<Mutex<ImageCache>>,
    /// Persisted node identity. Shared across reconnections; updated when the
    /// server assigns a UUID via AssignNodeUuid.
    node_uuid: Arc<Mutex<Option<String>>>,
}

pub struct ControlChannelHandle {
    shutdown_tx: oneshot::Sender<u8>,
}

impl<T: 'static + Transport> ControlChannel<T> {
    #[instrument(skip_all)]
    async fn run(&mut self) -> Result<()> {
        let mut remote_addr = AddrMaybeCached::new(&self.remote_addr);
        remote_addr.resolve().await?;

        let mut conn = self
            .transport
            .connect(&remote_addr)
            .await
            .with_context(|| format!("Failed to connect to {}", &self.remote_addr))?;
        T::hint(&conn, SocketOpts::for_control_channel());

        debug!("Sending hello");
        let hello_send =
            Hello::ControlChannelHello(CURRENT_PROTO_VERSION, self.digest[..].try_into().unwrap());
        protocol::write_hello(&mut conn, &hello_send).await?;

        debug!("Reading hello");
        let nonce = match read_hello(&mut conn).await? {
            ControlChannelHello(_, d) => d,
            _ => {
                bail!("Unexpected type of hello");
            }
        };

        debug!("Sending auth");
        let mut concat = Vec::from(self.service.token.as_ref().unwrap().as_bytes());
        concat.extend_from_slice(&nonce);

        let session_key = protocol::digest(&concat);
        let auth = Auth(session_key);
        protocol::write_auth(&mut conn, &auth).await?;

        debug!("Reading ack");
        match read_ack(&mut conn).await? {
            Ack::Ok => {}
            v => {
                return Err(anyhow!("{}", v))
                    .with_context(|| format!("Authentication failed: {}", self.service.name));
            }
        }

        info!("Control channel established");

        let (mut rd, wr) = io::split(conn);
        let wr = Arc::new(tokio::sync::Mutex::new(wr));

        // Send ReportNodeStatus to register with the server. Carries our
        // persisted node UUID (or None on first ever connect, in which case the
        // server will reply with AssignNodeUuid).
        {
            let mut guard = wr.lock().await;
            let uuid = self.node_uuid.lock().unwrap().clone();
            let cmd = gather_node_status(&self.service, uuid).await;
            if let Err(e) = protocol::write_control_cmd(&mut *guard, &cmd).await {
                warn!("Failed to send ReportNodeStatus: {:#}", e);
            }
        }

        let socket_opts = SocketOpts::from_client_cfg(&self.service);
        let data_ch_args = Arc::new(RunDataChannelArgs {
            session_key,
            remote_addr,
            connector: self.transport.clone(),
            socket_opts,
            service: self.service.clone(),
        });

        let mut status_interval = time::interval(Duration::from_secs(5));

        loop {
            tokio::select! {
                val = read_control_cmd(&mut rd) => {
                    let val = val?;
                    debug!("Received {:?}", val);
                    match val {
                        ControlChannelCmd::CreateDataChannel => {
                            let args = data_ch_args.clone();
                            tokio::spawn(async move {
                                if let Err(e) = run_data_channel(args).await.with_context(|| "Failed to run the data channel") {
                                    warn!("{:#}", e);
                                }
                            }.instrument(Span::current()));
                        },
                        ControlChannelCmd::HeartBeat => {
                            let mut guard = wr.lock().await;
                            let uuid = self.node_uuid.lock().unwrap().clone();
                            let cmd = gather_node_status(&self.service, uuid).await;
                            if let Err(e) = protocol::write_control_cmd(&mut *guard, &cmd).await {
                                warn!("Failed to send heartbeat status: {:#}", e);
                            }
                        }
                        ControlChannelCmd::PortsAssigned { mappings } => {
                            info!("Server assigned ports: {:?}", mappings);
                        }
                        ControlChannelCmd::AssignNodeUuid { uuid } => {
                            info!("Server assigned node_uuid={}", uuid);
                            // If we can't persist this, every reconnect will be assigned a
                            // fresh uuid by the server and quietly orphan DB rows. Make it
                            // loud so the supervisor surfaces the problem.
                            if let Err(e) = NodeIdentity::save(&uuid) {
                                return Err(anyhow!(
                                    "Cannot persist server-assigned node_uuid: {:#}", e
                                ));
                            }
                            *self.node_uuid.lock().unwrap() = Some(uuid.clone());
                        }
                        ControlChannelCmd::ImageStart { image_tag, source, source_type, container_name, port_map, env } => {
                            let wr = wr.clone();
                            let containers = self.containers.clone();
                            tokio::spawn(async move {
                                match image_start(&image_tag, &source, &source_type, &container_name, &port_map, &env).await {
                                    Ok(ports) => {
                                        containers.lock().unwrap().push(TrackedContainer {
                                            container_name: container_name.clone(),
                                            image_tag: image_tag.clone(),
                                        });
                                        let mut guard = wr.lock().await;
                                        let _ = protocol::write_control_cmd(&mut *guard, &ControlChannelCmd::ContainerStarted {
                                            container_name: container_name.clone(),
                                            ports,
                                        }).await;
                                    }
                                    Err(e) => {
                                        let mut guard = wr.lock().await;
                                        let _ = protocol::write_control_cmd(&mut *guard, &ControlChannelCmd::ContainerError {
                                            container_name: container_name.clone(),
                                            error: format!("{}", e),
                                        }).await;
                                    }
                                }
                            });
                        }
                        ControlChannelCmd::ImageStop { container_name } => {
                            let wr = wr.clone();
                            let containers = self.containers.clone();
                            tokio::spawn(async move {
                                match docker_stop(&container_name).await {
                                    Ok(()) => {
                                        containers.lock().unwrap().retain(|c| c.container_name != container_name);
                                        let mut guard = wr.lock().await;
                                        let _ = protocol::write_control_cmd(&mut *guard, &ControlChannelCmd::ContainerStopped {
                                            container_name: container_name.clone(),
                                        }).await;
                                    }
                                    Err(e) => {
                                        let mut guard = wr.lock().await;
                                        let _ = protocol::write_control_cmd(&mut *guard, &ControlChannelCmd::ContainerError {
                                            container_name: container_name.clone(),
                                            error: format!("{}", e),
                                        }).await;
                                    }
                                }
                            });
                        }
                        other => {
                            info!("Unhandled command from server: {:?}", other);
                        }
                    }
                },
                _ = status_interval.tick() => {
                    let mut guard = wr.lock().await;
                    let uuid = self.node_uuid.lock().unwrap().clone();
                    let cmd = gather_node_status(&self.service, uuid).await;
                    if let Err(e) = protocol::write_control_cmd(&mut *guard, &cmd).await {
                        warn!("Failed to send periodic status: {:#}", e);
                    }
                },
                _ = time::sleep(Duration::from_secs(self.heartbeat_timeout)), if self.heartbeat_timeout != 0 => {
                    return Err(anyhow!("Heartbeat timed out"))
                }
                _ = &mut self.shutdown_rx => {
                    break;
                }
            }
        }

        info!("Control channel shutdown");
        Ok(())
    }
}

impl ControlChannelHandle {
    #[instrument(name="handle", skip_all, fields(service = %service.name))]
    pub fn new<T: 'static + Transport>(
        service: ClientServiceConfig,
        remote_addr: String,
        transport: Arc<T>,
        heartbeat_timeout: u64,
    ) -> ControlChannelHandle {
        let digest = protocol::digest(service.name.as_bytes());

        info!("Starting {}", hex::encode(digest));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let containers: Arc<Mutex<Vec<TrackedContainer>>> =
            Arc::new(Mutex::new(Vec::new()));
        let image_cache: Arc<Mutex<ImageCache>> =
            Arc::new(Mutex::new(ImageCache::load()));
        let node_uuid: Arc<Mutex<Option<String>>> =
            Arc::new(Mutex::new(NodeIdentity::load()));

        if let Some(u) = node_uuid.lock().unwrap().as_ref() {
            info!("Loaded node_uuid={}", u);
        } else {
            info!("No persisted node_uuid; will request one from the server on first connect");
        }

        let retry_interval = service.retry_interval;
        let cache_ttl = service.image_cache_ttl_seconds;

        let mut retry_backoff = run_control_chan_backoff(retry_interval.unwrap());

        let mut s = ControlChannel {
            digest,
            service,
            shutdown_rx,
            remote_addr,
            transport,
            heartbeat_timeout,
            containers: containers.clone(),
            image_cache: image_cache.clone(),
            node_uuid: node_uuid.clone(),
        };

        tokio::spawn(
            async move {
                let mut start = Instant::now();
                let mut disconnected_at: Option<Instant> = None;
                let mut cleanup_done = false;
                let ttl = Duration::from_secs(cache_ttl);

                while let Err(err) = s
                    .run()
                    .await
                    .with_context(|| "Failed to run the control channel")
                {
                    if disconnected_at.is_none() {
                        disconnected_at = Some(Instant::now());
                        info!("Control channel disconnected, cache TTL = {}s", cache_ttl);
                    }

                    if !cleanup_done {
                        if let Some(dt) = disconnected_at {
                            if dt.elapsed() > ttl {
                                info!("Cache TTL expired, cleaning up containers and images");
                                let tracked: Vec<TrackedContainer> =
                                    s.containers.lock().unwrap().drain(..).collect();
                                for c in &tracked {
                                    warn!("TTL expired — stopping orphan container {}", c.container_name);
                                    let _ = docker_stop(&c.container_name).await;
                                    let _ = docker_rmi(&c.image_tag).await;
                                }
                                s.image_cache.lock().unwrap().clear_expired(ttl.as_secs() as i64);
                                cleanup_done = true;
                            }
                        }
                    }

                    if s.shutdown_rx.try_recv() != Err(oneshot::error::TryRecvError::Empty) {
                        break;
                    }

                    if start.elapsed() > Duration::from_secs(3) {
                        retry_backoff.reset();
                    }

                    if let Some(duration) = retry_backoff.next_backoff() {
                        error!("{:#}. Retry in {:?}...", err, duration);
                        time::sleep(duration).await;
                    } else {
                        panic!("{:#}. Break", err);
                    }

                    start = Instant::now();
                }
            }
            .instrument(Span::current()),
        );

        ControlChannelHandle { shutdown_tx }
    }

    pub fn shutdown(self) {
        let _ = self.shutdown_tx.send(0u8);
    }
}

fn get_hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .to_string()
        })
        .unwrap_or_else(|_| "unknown".to_string())
}

fn get_docker_version() -> String {
    std::process::Command::new("docker")
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "not installed".to_string())
}

fn get_memory_mb() -> u64 {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("MemTotal:"))
                    .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
                    .map(|kb| kb / 1024)
            })
            .unwrap_or(0)
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .parse::<u64>()
                    .ok()
            })
            .map(|bytes| bytes / 1024 / 1024)
            .unwrap_or(0)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0
    }
}

async fn get_running_containers() -> Vec<crate::protocol::ContainerInfo> {
    let output = tokio::process::Command::new("docker")
        .args(["ps", "--format", "{{.Names}}\t{{.Image}}\t{{.Ports}}\t{{.Status}}"])
        .output()
        .await
        .ok();

    match output {
        Some(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            stdout
                .lines()
                .filter_map(|line| {
                    let parts: Vec<&str> = line.split('\t').collect();
                    if parts.len() < 4 {
                        return None;
                    }
                    let ports: Vec<u16> = parts[2]
                        .split(',')
                        .filter_map(|p| {
                            let p = p.trim();
                            p.split("->").next()?.split(':').last()?.parse().ok()
                        })
                        .collect();
                    Some(crate::protocol::ContainerInfo {
                        container_name: parts[0].to_string(),
                        image: parts[1].to_string(),
                        ports,
                        status: parts[3].to_string(),
                    })
                })
                .collect()
        }
        _ => Vec::new(),
    }
}
async fn gather_node_status(service: &ClientServiceConfig, node_uuid: Option<String>) -> ControlChannelCmd {
    let hostname = get_hostname();
    let os = std::env::consts::OS.to_string();
    let arch = std::env::consts::ARCH.to_string();
    let docker_version = get_docker_version();
    let (port_range_start, port_range_end) = service.port_range();
    let cpu_cores = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
    let memory_mb = get_memory_mb();
    let running_containers = get_running_containers().await;

    ControlChannelCmd::ReportNodeStatus {
        node_uuid,
        hostname,
        os,
        arch,
        docker_version,
        port_range_start,
        port_range_end,
        cpu_cores,
        memory_mb,
        running_containers,
    }
}

async fn docker_pull(image: &str) -> Result<()> {
    use tokio::process::Command;
    info!("Pulling image: {}", image);
    let status = Command::new("docker")
        .args(["pull", image])
        .status()
        .await
        .with_context(|| format!("docker pull failed for {}", image))?;
    if status.success() {
        info!("Pull complete: {}", image);
        Ok(())
    } else {
        Err(anyhow::anyhow!("docker pull failed for {}", image))
    }
}

async fn docker_build_from_git(git_url: &str, branch: &str, image_tag: &str) -> Result<()> {
    use tokio::process::Command;
    info!("Building {} from {} (branch: {})", image_tag, git_url, branch);

    let tmp_dir = std::env::temp_dir().join(format!(
        "kdct-build-{}",
        image_tag.replace(['/', ':', '@', '\\'], "_")
    ));
    if tmp_dir.exists() {
        let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
    }

    let clone_status = Command::new("git")
        .args(["clone", "--depth", "1", "--branch", branch, git_url])
        .arg(&tmp_dir)
        .status()
        .await
        .with_context(|| format!("git clone failed for {}", git_url))?;

    if !clone_status.success() {
        return Err(anyhow::anyhow!("git clone failed for {}", git_url));
    }

    let build_status = Command::new("docker")
        .args(["build", "-t", image_tag, "."])
        .current_dir(&tmp_dir)
        .status()
        .await
        .with_context(|| format!("docker build failed for {}", image_tag))?;

    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;

    if build_status.success() {
        info!("Build complete: {}", image_tag);
        Ok(())
    } else {
        Err(anyhow::anyhow!("docker build failed for {}", image_tag))
    }
}

async fn image_start(
    image_tag: &str,
    source: &str,
    source_type: &str,
    container_name: &str,
    port_map: &[(u16, u16)],
    env: &[(String, String)],
) -> Result<Vec<u16>> {
    use tokio::process::Command;

    // Remove existing container with the same name so deploy always starts clean
    if docker_container_exists(container_name).await {
        info!("Container '{}' already exists, removing for clean deploy", container_name);
        let _ = Command::new("docker").args(["stop", container_name]).status().await;
        let _ = Command::new("docker").args(["rm", "-f", container_name]).status().await;
    }

    // Build or pull image if not found locally
    if !docker_image_exists(image_tag).await {
        if source_type == "git" {
            info!("Image '{}' not found locally, building from {}", image_tag, source);
            docker_build_from_git(source, "main", image_tag).await?;
        } else {
            info!("Image '{}' not found locally, pulling...", image_tag);
            docker_pull(source).await?;
        }
        image_cache_record(image_tag);
    }

    docker_run(image_tag, container_name, port_map, env).await
}

fn image_cache_record(image: &str) {
    let mut cache = ImageCache::load();
    cache.record(image);
}

async fn docker_image_exists(image: &str) -> bool {
    use tokio::process::Command;
    let output = Command::new("docker")
        .args(["images", "-q", image])
        .output()
        .await;
    match output {
        Ok(o) => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        Err(_) => false,
    }
}

async fn docker_container_exists(container_name: &str) -> bool {
    use tokio::process::Command;
    let output = Command::new("docker")
        .args(["ps", "-a", "-q", "-f", &format!("name=^{}$", container_name)])
        .output()
        .await;
    match output {
        Ok(o) => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        Err(_) => false,
    }
}

async fn docker_run(
    image_tag: &str,
    container_name: &str,
    port_map: &[(u16, u16)],
    env: &[(String, String)],
) -> Result<Vec<u16>> {
    use tokio::process::Command;
    info!("Running container: {} ({})", container_name, image_tag);

    let mut cmd = Command::new("docker");
    cmd.arg("run").arg("-d").arg("--name").arg(container_name);

    for (host_port, container_port) in port_map {
        cmd.arg("-p").arg(format!("{}:{}", host_port, container_port));
    }
    for (k, v) in env {
        cmd.arg("-e").arg(format!("{}={}", k, v));
    }
    cmd.arg(image_tag);

    let output = cmd.output().await
        .with_context(|| format!("docker run failed for {}", container_name))?;

    if output.status.success() {
        info!("Container started: {}", container_name);
        Ok(port_map.iter().map(|(h, _)| *h).collect())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(anyhow::anyhow!("docker run failed: {}", stderr))
    }
}

async fn docker_stop(container_name: &str) -> Result<()> {
    use tokio::process::Command;
    info!("Stopping container: {}", container_name);
    let status = Command::new("docker")
        .args(["stop", container_name])
        .status()
        .await
        .with_context(|| format!("docker stop failed for {}", container_name))?;
    if status.success() {
        let _ = Command::new("docker").args(["rm", container_name]).status().await;
        info!("Container stopped: {}", container_name);
        Ok(())
    } else {
        Err(anyhow::anyhow!("docker stop failed for {}", container_name))
    }
}

async fn docker_rmi(image: &str) -> Result<()> {
    use tokio::process::Command;
    info!("Removing image: {}", image);
    let status = Command::new("docker")
        .args(["rmi", image])
        .status()
        .await
        .with_context(|| format!("docker rmi failed for {}", image))?;
    if status.success() {
        info!("Image removed: {}", image);
        Ok(())
    } else {
        Err(anyhow::anyhow!("docker rmi failed for {}", image))
    }
}
