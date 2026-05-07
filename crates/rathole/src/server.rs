use crate::config::{Config, ServerConfig, ServerServiceConfig, ServiceType, TransportType};
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

use rand::RngCore;
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

    match config.transport.transport_type {
        TransportType::Tcp => {
            let (node_update_tx, _) = mpsc::channel(1024);
            let mut server = Server::<TcpTransport>::from(config, None, node_update_tx).await?;
            server.run(shutdown_rx).await?;
        }
    }

    Ok(())
}

type ControlChannelMap<T> = MultiMap<ServiceDigest, Nonce, ControlChannelHandle<T>>;

pub struct Server<T: Transport> {
    config: Arc<ServerConfig>,
    services: Arc<RwLock<HashMap<ServiceDigest, ServerServiceConfig>>>,
    control_channels: Arc<RwLock<ControlChannelMap<T>>>,
    transport: Arc<T>,
    pub clients: ClientRegistry,
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
    ) -> Result<Server<T>> {
        let config = Arc::new(config);
        let services = Arc::new(RwLock::new(generate_service_hashmap(&config)));
        let control_channels = Arc::new(RwLock::new(ControlChannelMap::new()));
        let transport = Arc::new(T::new(&config.transport)?);
        let clients = registry::new_registry();
        Ok(Server {
            config,
            services,
            control_channels,
            transport,
            clients,
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
                                            let server_config = self.config.clone();
                                            let clients = self.clients.clone();
                                            let port_pool = self.port_pool.clone();
                                            let node_update_tx = self.node_update_tx.clone();
                                            tokio::spawn(async move {
                                                if let Err(err) = handle_connection(conn, services, control_channels, server_config, clients, port_pool, node_update_tx).await {
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
    server_config: Arc<ServerConfig>,
    clients: ClientRegistry,
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
                service_digest,
                server_config,
                clients,
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
    service_digest: ServiceDigest,
    server_config: Arc<ServerConfig>,
    clients: ClientRegistry,
    port_pool: Option<Arc<crate::port_pool::PortPool>>,
    node_update_tx: mpsc::Sender<crate::node_update::NodeUpdate>,
) -> Result<()> {
    info!("Try to handshake a control channel");

    T::hint(&conn, SocketOpts::for_control_channel());

    let mut nonce = vec![0u8; HASH_WIDTH_IN_BYTES];
    rand::thread_rng().fill_bytes(&mut nonce);

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

    let mut h = control_channels.write().await;

    if h.remove1(&service_digest).is_some() {
        warn!("Dropping previous control channel for {}", service_name);
    }

    protocol::write_ack(&mut conn, &Ack::Ok).await?;

    // Build synthetic service config for the client
    let service_config = ServerServiceConfig {
        service_type: ServiceType::Tcp,
        name: service_name.clone(),
        bind_addr: "127.0.0.1:0".into(),
        token: Some(server_config.default_token.clone()),
        nodelay: None,
    };

    info!(service = %service_name, "Control channel established");
    let handle =
        ControlChannelHandle::new(conn, service_config, server_config.heartbeat_interval, clients, service_digest, port_pool, node_update_tx);

    let _ = h.insert(service_digest, session_key, handle);

    Ok(())
}

async fn do_data_channel_handshake<T: 'static + Transport>(
    conn: T::Stream,
    control_channels: Arc<RwLock<ControlChannelMap<T>>>,
    nonce: Nonce,
) -> Result<()> {
    debug!("Try to handshake a data channel");

    let (hint_opts, port_data_pending, data_ch_tx) = {
        let guard = control_channels.read().await;
        match guard.get2(&nonce) {
            Some(handle) => (
                Some(SocketOpts::from_server_cfg(&handle.service)),
                Some(handle.port_data_pending.clone()),
                Some(handle.data_ch_tx.clone()),
            ),
            None => {
                warn!("Data channel has incorrect nonce");
                (None, None, None)
            }
        }
    };

    let (hint_opts, port_data_pending, data_ch_tx) = match (hint_opts, port_data_pending, data_ch_tx) {
        (Some(h), Some(p), Some(tx)) => (h, p, tx),
        _ => return Ok(()),
    };

    T::hint(&conn, hint_opts);

    let mut pending = port_data_pending.write().await;
    if let Some((sender, _local_port)) = pending.pop_front() {
        drop(pending);
        let _ = sender.send(conn);
    } else {
        drop(pending);
        data_ch_tx
            .send(conn)
            .await
            .with_context(|| "Data channel for a stale control channel")?;
    }
    Ok(())
}

pub struct ControlChannelHandle<T: Transport> {
    _shutdown_tx: broadcast::Sender<bool>,
    data_ch_tx: mpsc::Sender<T::Stream>,
    service: ServerServiceConfig,
    /// Send commands to this client's control channel
    pub cmd_tx: mpsc::Sender<ControlChannelCmd>,
    port_data_pending: Arc<RwLock<std::collections::VecDeque<(tokio::sync::oneshot::Sender<T::Stream>, u16)>>>,
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
        service_digest: ServiceDigest,
        port_pool: Option<Arc<crate::port_pool::PortPool>>,
        node_update_tx: mpsc::Sender<crate::node_update::NodeUpdate>,
    ) -> ControlChannelHandle<T> {
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);
        let (data_ch_tx, data_ch_rx) = mpsc::channel(CHAN_SIZE * 2);
        let (data_ch_req_tx, data_ch_req_rx) = mpsc::unbounded_channel();

        // Channel for sending commands to this client
        let (cmd_tx, cmd_rx) = mpsc::channel::<ControlChannelCmd>(64);
        let cmd_tx_for_registry = cmd_tx.clone();

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
            clients,
            port_data_pending: port_data_pending.clone(),
            port_pool,
            data_ch_req_tx: data_ch_req_tx_for_control,
            shutdown_tx: shutdown_tx.clone(),
            node_update_tx: node_update_tx.clone(),
        };

        tokio::spawn(
            async move {
                if let Err(err) = ch.run().await {
                    error!("{:#}", err);
                }
                let _ = shutdown_tx_clone.send(true);
                registry::remove(&clients_for_drop, &digest_for_drop).await;
            }
            .instrument(Span::current()),
        );

        ControlChannelHandle {
            _shutdown_tx: shutdown_tx,
            data_ch_tx,
            service,
            cmd_tx,
            port_data_pending,
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
    service_digest: ServiceDigest,
    service_name: String,
    clients: ClientRegistry,
    port_data_pending: Arc<RwLock<VecDeque<(oneshot::Sender<T::Stream>, u16)>>>,
    port_pool: Option<Arc<crate::port_pool::PortPool>>,
    data_ch_req_tx: mpsc::UnboundedSender<bool>,
    shutdown_tx: broadcast::Sender<bool>,
    node_update_tx: mpsc::Sender<crate::node_update::NodeUpdate>,
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
                        Ok(ControlChannelCmd::ReportNodeStatus { hostname, os, arch, docker_version, port_range_start, port_range_end, cpu_cores, memory_mb, running_containers }) => {
                            info!(
                                "Client status: {} {} {} docker={} ports={}-{} cpu={} mem={}MB containers={}",
                                hostname, os, arch, docker_version,
                                port_range_start, port_range_end,
                                cpu_cores, memory_mb,
                                running_containers.len()
                            );
                            registry::upsert(
                                &self.clients,
                                self.service_digest,
                                self.service_name.clone(),
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
                            ).await;

                            let _ = self.node_update_tx.send(crate::node_update::NodeUpdate {
                                digest: hex::encode(self.service_digest),
                                hostname,
                                os,
                                arch,
                                docker_version,
                                port_range_start,
                                port_range_end,
                                cpu_cores,
                                memory_mb,
                                running_containers,
                            }).await;
                        }
                        Ok(ControlChannelCmd::DockerPullProgress { image, status }) => {
                            info!("Docker pull progress: {} — {}", image, status);
                        }
                        Ok(ControlChannelCmd::DockerBuildProgress { image_tag, status }) => {
                            info!("Docker build progress: {} — {}", image_tag, status);
                        }
                        Ok(ControlChannelCmd::ContainerStarted { container_name, ports }) => {
                            info!("Container started: {} ports:{:?}", container_name, ports);
                        }
                        Ok(ControlChannelCmd::ContainerStopped { container_name }) => {
                            info!("Container stopped: {}", container_name);
                        }
                        Ok(ControlChannelCmd::ContainerError { container_name, error }) => {
                            error!("Container error: {} — {}", container_name, error);
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

        Ok(())
    }
}

#[allow(dead_code)]
fn spawn_port_accept_loop<T: Transport>(
    pool: Arc<crate::port_pool::PortPool>,
    server_port: u16,
    local_port: u16,
    data_ch_req_tx: mpsc::UnboundedSender<bool>,
    port_data_pending: Arc<RwLock<VecDeque<(oneshot::Sender<T::Stream>, u16)>>>,
    mut shutdown_rx: broadcast::Receiver<bool>,
) {
    tokio::spawn(
        async move {
            loop {
                tokio::select! {
                    result = pool.accept(server_port) => {
                        match result {
                            Ok((mut visitor, addr)) => {
                                debug!("Port {} visitor from {}", server_port, addr);
                                if data_ch_req_tx.send(true).is_err() {
                                    break;
                                }
                                let (tx, rx) = oneshot::channel::<T::Stream>();
                                port_data_pending.write().await.push_back((tx, local_port));
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
