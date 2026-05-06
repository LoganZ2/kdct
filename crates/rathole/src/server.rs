use crate::config::{Config, ServerConfig, ServerServiceConfig, ServiceType, TransportType};
use crate::config_watcher::{ConfigChange, ServerServiceChange};
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
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{self, copy_bidirectional, AsyncReadExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio::time;
use tracing::{debug, error, info, info_span, instrument, warn, Instrument, Span};

#[cfg(feature = "noise")]
use crate::transport::NoiseTransport;
#[cfg(any(feature = "native-tls", feature = "rustls"))]
use crate::transport::TlsTransport;
#[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
use crate::transport::WebsocketTransport;

type ServiceDigest = protocol::Digest; // SHA256 of a service name
type Nonce = protocol::Digest; // Also called `session_key`

const TCP_POOL_SIZE: usize = 8; // The number of cached connections for TCP servies
const UDP_POOL_SIZE: usize = 2; // The number of cached connections for UDP services
const CHAN_SIZE: usize = 2048; // The capacity of various chans
const HANDSHAKE_TIMEOUT: u64 = 5; // Timeout for transport handshake

// The entrypoint of running a server
pub async fn run_server(
    config: Config,
    shutdown_rx: broadcast::Receiver<bool>,
    update_rx: mpsc::Receiver<ConfigChange>,
) -> Result<()> {
    let config = match config.server {
            Some(config) => config,
            None => {
                return Err(anyhow!("Try to run as a server, but the configuration is missing. Please add the `[server]` block"))
            }
        };

    match config.transport.transport_type {
        TransportType::Tcp => {
            let mut server = Server::<TcpTransport>::from(config, None).await?;
            server.run(shutdown_rx, update_rx).await?;
        }
        TransportType::Tls => {
            #[cfg(any(feature = "native-tls", feature = "rustls"))]
            {
                let mut server = Server::<TlsTransport>::from(config, None).await?;
                server.run(shutdown_rx, update_rx).await?;
            }
            #[cfg(not(any(feature = "native-tls", feature = "rustls")))]
            crate::helper::feature_neither_compile("native-tls", "rustls")
        }
        TransportType::Noise => {
            #[cfg(feature = "noise")]
            {
                let mut server = Server::<NoiseTransport>::from(config, None).await?;
                server.run(shutdown_rx, update_rx).await?;
            }
            #[cfg(not(feature = "noise"))]
            crate::helper::feature_not_compile("noise")
        }
        TransportType::Websocket => {
            #[cfg(any(feature = "websocket-native-tls", feature = "websocket-rustls"))]
            {
                let mut server = Server::<WebsocketTransport>::from(config, None).await?;
                server.run(shutdown_rx, update_rx).await?;
            }
            #[cfg(not(any(feature = "websocket-native-tls", feature = "websocket-rustls")))]
            crate::helper::feature_neither_compile("websocket-native-tls", "websocket-rustls")
        }
    }

    Ok(())
}

// A hash map of ControlChannelHandles, indexed by ServiceDigest or Nonce
// See also MultiMap
type ControlChannelMap<T> = MultiMap<ServiceDigest, Nonce, ControlChannelHandle<T>>;

// Server holds all states of running a server
pub struct Server<T: Transport> {
    // `[server]` config
    config: Arc<ServerConfig>,

    // `[server.services]` config, indexed by ServiceDigest
    services: Arc<RwLock<HashMap<ServiceDigest, ServerServiceConfig>>>,
    // Collection of control channels
    control_channels: Arc<RwLock<ControlChannelMap<T>>>,
    // Wrapper around the transport layer
    transport: Arc<T>,
    // Connected client registry (pub for admin API)
    pub clients: ClientRegistry,
    // Port pool for auto-assignment
    pub port_pool: Option<Arc<crate::port_pool::PortPool>>,
    // Pipeline output channel (read by admin layer)
    pub pipeline_output_rx: mpsc::Receiver<(ServiceDigest, ControlChannelCmd)>,
    pipeline_output_tx: mpsc::Sender<(ServiceDigest, ControlChannelCmd)>,
}

// Generate a hash map of services which is indexed by ServiceDigest
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
    // Create a server from `[server]`
    pub async fn from(
        config: ServerConfig,
        port_pool: Option<Arc<crate::port_pool::PortPool>>,
    ) -> Result<Server<T>> {
        let config = Arc::new(config);
        let services = Arc::new(RwLock::new(generate_service_hashmap(&config)));
        let control_channels = Arc::new(RwLock::new(ControlChannelMap::new()));
        let transport = Arc::new(T::new(&config.transport)?);
        let (pipeline_output_tx, pipeline_output_rx) = mpsc::channel(64);
        let clients = registry::new_registry();
        Ok(Server {
            config,
            services,
            control_channels,
            transport,
            clients,
            port_pool,
            pipeline_output_rx,
            pipeline_output_tx,
        })
    }

    // The entry point of Server
    pub async fn run(
        &mut self,
        mut shutdown_rx: broadcast::Receiver<bool>,
        mut update_rx: mpsc::Receiver<ConfigChange>,
    ) -> Result<()> {
        // Listen at `server.bind_addr`
        let l = self
            .transport
            .bind(&self.config.bind_addr)
            .await
            .with_context(|| "Failed to listen at `server.bind_addr`")?;
        info!("Listening at {}", self.config.bind_addr);

        // Retry at least every 100ms
        let mut backoff = ExponentialBackoff {
            max_interval: Duration::from_millis(100),
            max_elapsed_time: None,
            ..Default::default()
        };

        // Wait for connections and shutdown signals
        loop {
            tokio::select! {
                // Wait for incoming control and data channels
                ret = self.transport.accept(&l) => {
                    match ret {
                        Err(err) => {
                            // Detects whether it's an IO error
                            if let Some(err) = err.downcast_ref::<io::Error>() {
                                // If it is an IO error, then it's possibly an
                                // EMFILE. So sleep for a while and retry
                                // TODO: Only sleep for EMFILE, ENFILE, ENOMEM, ENOBUFS
                                if let Some(d) = backoff.next_backoff() {
                                    error!("Failed to accept: {:#}. Retry in {:?}...", err, d);
                                    time::sleep(d).await;
                                } else {
                                    // This branch will never be executed according to the current retry policy
                                    error!("Too many retries. Aborting...");
                                    break;
                                }
                            }
                            // If it's not an IO error, then it comes from
                            // the transport layer, so just ignore it
                        }
                        Ok((conn, addr)) => {
                            backoff.reset();

                            // Do transport handshake with a timeout
                            match time::timeout(Duration::from_secs(HANDSHAKE_TIMEOUT), self.transport.handshake(conn)).await {
                                Ok(conn) => {
                                    match conn.with_context(|| "Failed to do transport handshake") {
                                        Ok(conn) => {
                                            let services = self.services.clone();
                                            let control_channels = self.control_channels.clone();
                                            let server_config = self.config.clone();
                                            let clients = self.clients.clone();
                                            let pipeline_output_tx = self.pipeline_output_tx.clone();
                                            tokio::spawn(async move {
                                                if let Err(err) = handle_connection(conn, services, control_channels, server_config, clients, pipeline_output_tx).await {
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
                // Wait for the shutdown signal
                _ = shutdown_rx.recv() => {
                    info!("Shuting down gracefully...");
                    break;
                },
                e = update_rx.recv() => {
                    if let Some(e) = e {
                        self.handle_hot_reload(e).await;
                    }
                }
            }
        }

        info!("Shutdown");

        Ok(())
    }

    async fn handle_hot_reload(&mut self, e: ConfigChange) {
        match e {
            ConfigChange::ServerChange(server_change) => match server_change {
                ServerServiceChange::Add(cfg) => {
                    let hash = protocol::digest(cfg.name.as_bytes());
                    let mut wg = self.services.write().await;
                    let _ = wg.insert(hash, cfg);

                    let mut wg = self.control_channels.write().await;
                    let _ = wg.remove1(&hash);
                }
                ServerServiceChange::Delete(s) => {
                    let hash = protocol::digest(s.as_bytes());
                    let _ = self.services.write().await.remove(&hash);

                    let mut wg = self.control_channels.write().await;
                    let _ = wg.remove1(&hash);
                }
            },
            ignored => warn!("Ignored {:?} since running as a server", ignored),
        }
    }
}

// Handle connections to `server.bind_addr`
async fn handle_connection<T: 'static + Transport>(
    mut conn: T::Stream,
    services: Arc<RwLock<HashMap<ServiceDigest, ServerServiceConfig>>>,
    control_channels: Arc<RwLock<ControlChannelMap<T>>>,
    server_config: Arc<ServerConfig>,
    clients: ClientRegistry,
    pipeline_output_tx: mpsc::Sender<(ServiceDigest, ControlChannelCmd)>,
) -> Result<()> {
    // Read hello
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
                pipeline_output_tx,
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
    services: Arc<RwLock<HashMap<ServiceDigest, ServerServiceConfig>>>,
    control_channels: Arc<RwLock<ControlChannelMap<T>>>,
    service_digest: ServiceDigest,
    server_config: Arc<ServerConfig>,
    clients: ClientRegistry,
    pipeline_output_tx: mpsc::Sender<(ServiceDigest, ControlChannelCmd)>,
) -> Result<()> {
    info!("Try to handshake a control channel");

    T::hint(&conn, SocketOpts::for_control_channel());

    // Generate a nonce
    let mut nonce = vec![0u8; HASH_WIDTH_IN_BYTES];
    rand::thread_rng().fill_bytes(&mut nonce);

    // Send hello
    let hello_send = Hello::ControlChannelHello(
        protocol::CURRENT_PROTO_VERSION,
        nonce.clone().try_into().unwrap(),
    );
    protocol::write_hello(&mut conn, &hello_send).await?;

    // Lookup the service
    let service_config = match services.read().await.get(&service_digest) {
        Some(v) => v,
        None => {
            protocol::write_ack(&mut conn, &Ack::ServiceNotExist).await?;
            bail!("No such a service {}", hex::encode(service_digest));
        }
    }
    .to_owned();

    let service_name = &service_config.name;

    // Calculate the checksum
    let mut concat = Vec::from(service_config.token.as_ref().unwrap().as_bytes());
    concat.append(&mut nonce);

    // Read auth
    let protocol::Auth(d) = read_auth(&mut conn).await?;

    // Validate
    let session_key = protocol::digest(&concat);
    if session_key != d {
        protocol::write_ack(&mut conn, &Ack::AuthFailed).await?;
        debug!(
            "Expect {}, but got {}",
            hex::encode(session_key),
            hex::encode(d)
        );
        bail!("Service {} failed the authentication", service_name);
    } else {
        let mut h = control_channels.write().await;

        // If there's already a control channel for the service, then drop the old one.
        // Because a control channel doesn't report back when it's dead,
        // the handle in the map could be stall, dropping the old handle enables
        // the client to reconnect.
        if h.remove1(&service_digest).is_some() {
            warn!(
                "Dropping previous control channel for service {}",
                service_name
            );
        }

        // Send ack
        protocol::write_ack(&mut conn, &Ack::Ok).await?;

        info!(service = %service_config.name, "Control channel established");
        let handle =
            ControlChannelHandle::new(conn, service_config, server_config.heartbeat_interval, clients, service_digest, pipeline_output_tx);

        // Insert the new handle
        let _ = h.insert(service_digest, session_key, handle);
    }

    Ok(())
}

async fn do_data_channel_handshake<T: 'static + Transport>(
    conn: T::Stream,
    control_channels: Arc<RwLock<ControlChannelMap<T>>>,
    nonce: Nonce,
) -> Result<()> {
    debug!("Try to handshake a data channel");

    // Validate
    let control_channels_guard = control_channels.read().await;
    match control_channels_guard.get2(&nonce) {
        Some(handle) => {
            T::hint(&conn, SocketOpts::from_server_cfg(&handle.service));

            // Send the data channel to the corresponding control channel
            handle
                .data_ch_tx
                .send(conn)
                .await
                .with_context(|| "Data channel for a stale control channel")?;
        }
        None => {
            warn!("Data channel has incorrect nonce");
        }
    }
    Ok(())
}

pub struct ControlChannelHandle<T: Transport> {
    // Shutdown the control channel by dropping it
    _shutdown_tx: broadcast::Sender<bool>,
    data_ch_tx: mpsc::Sender<T::Stream>,
    service: ServerServiceConfig,
    /// Send pipeline commands (RunPipeline, CancelPipeline) to this client
    pub pipeline_tx: mpsc::Sender<ControlChannelCmd>,
}

impl<T> ControlChannelHandle<T>
where
    T: 'static + Transport,
{
    // Create a control channel handle, where the control channel handling task
    // and the connection pool task are created.
    #[instrument(name = "handle", skip_all, fields(service = %service.name))]
    fn new(
        conn: T::Stream,
        service: ServerServiceConfig,
        heartbeat_interval: u64,
        clients: ClientRegistry,
        service_digest: ServiceDigest,
        pipeline_output_tx: mpsc::Sender<(ServiceDigest, ControlChannelCmd)>,
    ) -> ControlChannelHandle<T> {
        // Create a shutdown channel
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);

        // Store data channels
        let (data_ch_tx, data_ch_rx) = mpsc::channel(CHAN_SIZE * 2);

        // Store data channel creation requests
        let (data_ch_req_tx, data_ch_req_rx) = mpsc::unbounded_channel();

        // Channel for receiving pipeline commands from admin
        let (pipeline_tx, pipeline_rx) = mpsc::channel::<ControlChannelCmd>(64);
        let pipeline_tx_for_registry = pipeline_tx.clone();

        // Cache some data channels for later use
        let pool_size = match service.service_type {
            ServiceType::Tcp => TCP_POOL_SIZE,
            ServiceType::Udp => UDP_POOL_SIZE,
            ServiceType::Http => TCP_POOL_SIZE, // HTTP runs over TCP
        };

        for _i in 0..pool_size {
            if let Err(e) = data_ch_req_tx.send(true) {
                error!("Failed to request data channel {}", e);
            };
        }

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
                    .with_context(|| "Failed to run TCP connection pool")
                    {
                        error!("{:#}", e);
                    }
                }
                .instrument(Span::current()),
            ),
            ServiceType::Http => tokio::spawn(
                async move {
                    if let Err(e) = run_tcp_connection_pool::<T>(
                        bind_addr,
                        data_ch_rx,
                        data_ch_req_tx,
                        shutdown_rx_clone,
                    )
                    .await
                    .with_context(|| "Failed to run HTTP connection pool")
                    {
                        error!("{:#}", e);
                    }
                }
                .instrument(Span::current()),
            ),
        };

        // Create the control channel
        let ch = ControlChannel::<T> {
            conn,
            shutdown_rx,
            data_ch_req_rx,
            heartbeat_interval,
            pipeline_rx,
            pipeline_tx: pipeline_tx_for_registry,
            pipeline_output_tx,
            service_digest: digest_for_drop,
            clients,
        };

        // Run the control channel
        tokio::spawn(
            async move {
                if let Err(err) = ch.run().await {
                    error!("{:#}", err);
                }
                // Clean up client registry on disconnect
                let _ = shutdown_tx_clone.send(true);
                registry::remove(&clients_for_drop, &digest_for_drop).await;
            }
            .instrument(Span::current()),
        );

        ControlChannelHandle {
            _shutdown_tx: shutdown_tx,
            data_ch_tx,
            service,
            pipeline_tx,
        }
    }
}

// Control channel, using T as the transport layer.
struct ControlChannel<T: Transport> {
    conn: T::Stream,                               // The connection of control channel
    shutdown_rx: broadcast::Receiver<bool>,        // Receives the shutdown signal
    data_ch_req_rx: mpsc::UnboundedReceiver<bool>, // Receives visitor connections
    heartbeat_interval: u64,                       // Application-layer heartbeat interval in secs
    pipeline_rx: mpsc::Receiver<ControlChannelCmd>, // Receive pipeline commands from admin
    pipeline_tx: mpsc::Sender<ControlChannelCmd>,   // Clone of handle's pipeline_tx (for registry)
    pipeline_output_tx: mpsc::Sender<(ServiceDigest, ControlChannelCmd)>, // Send output to admin
    service_digest: ServiceDigest,                 // Identifies this client
    clients: ClientRegistry,                       // For updating client info
}

impl<T: Transport> ControlChannel<T> {
    // Run a control channel — bidirectional:
    // - Read ReportStatus / PipelineOutput from client
    // - Write CreateDataChannel / HeartBeat / RunPipeline / CancelPipeline to client
    #[instrument(skip_all)]
    async fn run(mut self) -> Result<()> {
        // Split connection for concurrent read/write
        let (mut rd, wr) = io::split(self.conn);
        let wr = Arc::new(tokio::sync::Mutex::new(wr));

        // Wait for data channel requests, pipeline commands, and shutdown signal
        loop {
            tokio::select! {
                // Read commands from the client (ReportStatus, PipelineOutput)
                val = read_control_cmd(&mut rd) => {
                    match val {
                        Ok(ControlChannelCmd::ReportStatus { hostname, os, arch, ports }) => {
                            info!("Client status: {} {} {} ports:{:?}", hostname, os, arch, ports);
                            registry::upsert(
                                &self.clients,
                                self.service_digest,
                                hostname.clone(),
                                hostname,
                                os,
                                arch,
                                ports,
                                self.pipeline_tx.clone(),
                            ).await;
                        }
                        Ok(ControlChannelCmd::PipelineOutput { id, step, stdout, stderr, exit_code }) => {
                            // Forward to admin
                            let _ = self.pipeline_output_tx.send((
                                self.service_digest,
                                ControlChannelCmd::PipelineOutput { id, step, stdout, stderr, exit_code },
                            )).await;
                        }
                        Ok(ControlChannelCmd::PortsAssigned { .. }) => {
                            // Server sends this, should not receive
                            warn!("Server received unexpected PortsAssigned (server→client message)");
                        }
                        Ok(_) => {
                            // CreateDataChannel / HeartBeat — client should not send these
                            debug!("Unexpected control cmd from client");
                        }
                        Err(e) => {
                            debug!("Control channel read error: {:#}", e);
                            break;
                        }
                    }
                },
                // Visitor requested → send CreateDataChannel to client
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
                // Heartbeat
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
                // Admin pipeline command → forward to client
                cmd = self.pipeline_rx.recv() => {
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
                // Shutdown signal
                _ = self.shutdown_rx.recv() => {
                    break;
                }
            }
        }

        info!("Control channel shutdown");

        Ok(())
    }
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

        // Retry at least every 1s
        let mut backoff = ExponentialBackoff {
            max_interval: Duration::from_secs(1),
            max_elapsed_time: None,
            ..Default::default()
        };

        // Wait for visitors and the shutdown signal
        loop {
            tokio::select! {
                val = l.accept() => {
                    match val {
                        Err(e) => {
                            // `l` is a TCP listener so this must be a IO error
                            // Possibly a EMFILE. So sleep for a while
                            error!("{}. Sleep for a while", e);
                            if let Some(d) = backoff.next_backoff() {
                                time::sleep(d).await;
                            } else {
                                // This branch will never be reached for current backoff policy
                                error!("Too many retries. Aborting...");
                                break;
                            }
                        }
                        Ok((incoming, addr)) => {
                            // For every visitor, request to create a data channel
                            if data_ch_req_tx.send(true).with_context(|| "Failed to send data chan create request").is_err() {
                                // An error indicates the control channel is broken
                                // So break the loop
                                break;
                            }

                            backoff.reset();

                            debug!("New visitor from {}", addr);

                            // Send the visitor to the connection pool
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
                    // Current data channel is broken. Request for a new one
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
    // TODO: Load balance

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

    // Receive one data channel
    let mut conn = data_ch_rx
        .recv()
        .await
        .ok_or_else(|| anyhow!("No available data channels"))?;
    protocol::write_data_cmd(&mut conn, &DataChannelCmd::StartForwardUdp).await?;

    let mut buf = [0u8; UDP_BUFFER_SIZE];
    loop {
        tokio::select! {
            // Forward inbound traffic to the client
            val = l.recv_from(&mut buf) => {
                let (n, from) = val?;
                UdpTraffic::write_slice(&mut conn, from, &buf[..n]).await?;
            },

            // Forward outbound traffic from the client to the visitor
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
