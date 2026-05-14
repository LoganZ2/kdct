mod acme;
mod api;
mod db;
mod deploy;
mod deployment_tracker;
mod image;
mod proxy;

use anyhow::Result;
use clap::Parser;
use tunnel::config::{Config, TransportType};
use tunnel::port_pool::PortPool;
use tunnel::server::Server;
use tunnel::transport::TcpTransport;
use std::path::PathBuf;
use std::sync::Arc;
use std::collections::HashMap;
use tokio::sync::{broadcast, RwLock};
use tracing_subscriber::EnvFilter;

/// Replace the current process image with a fresh copy of ourselves
/// (same exe, same argv). Uses Unix `execv` so the PID is preserved —
/// systemd / openrc / runit / nohup all keep their supervisor handle.
/// A plain `spawn` + `exit` would orphan the new process and break
/// `Type=simple` units that expect the tracked PID to stay alive.
pub(crate) fn self_restart() -> ! {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().expect("current_exe");
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let err = std::process::Command::new(&exe).args(&args).exec();
    // `exec` only returns on failure.
    tracing::error!("Failed to exec kdcts: {}", err);
    std::process::exit(1);
}

/// Spawn a background thread that waits 500ms (so the HTTP response has
/// time to flush) and then `exec`s a fresh copy of ourselves in place.
pub(crate) fn delayed_restart() {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        self_restart();
    });
}

use crate::db::Database;
use crate::proxy::RouteTable;
use tunnel::node_update::NodeEvent;

#[derive(Parser)]
#[command(name = "kdcts")]
struct Args {
    /// Path to server config TOML
    #[arg(long, default_value = "server.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let args = Args::parse();
    start_server(args.config).await?;

    Ok(())
}

async fn start_server(config_path: PathBuf) -> Result<()> {
    let _ = fdlimit::raise_fd_limit();
    let config = Config::from_file(&config_path).await?;
    let server_config = config
        .server
        .ok_or_else(|| anyhow::anyhow!("Missing [server] section in config"))?;

    // Domain is optional and may also be overridden from the DB below.
    // When unset, the proxy accepts any Host header (like nginx with no
    // `server_name`). TLS is force-disabled without one.

    let http_port = server_config.http_port;
    let https_port = server_config.https_port;
    let api_port = server_config.api_port;
    let db_path = PathBuf::from("kdct.db");

    let db = Database::open(&db_path)?;

    // Wizard-managed identity settings (default_token, domain, admin auth,
    // TLS cert/key paths) live in `server_config` and override server.toml.
    // Same precedence rule as ACME: anything the panel has written wins.
    let mut server_config = server_config;
    if let Ok(Some(v)) = db.get_setting("setup_token") {
        if !v.is_empty() {
            server_config.default_token = Some(v.as_str().into());
        }
    }
    if let Ok(Some(v)) = db.get_setting("setup_domain") {
        if !v.is_empty() {
            server_config.domain = Some(v);
        }
    }
    if let Ok(Some(v)) = db.get_setting("setup_admin_user") {
        if !v.is_empty() {
            server_config.admin_user = Some(v);
        }
    }
    if let Ok(Some(v)) = db.get_setting("setup_admin_password") {
        if !v.is_empty() {
            server_config.admin_password = Some(v);
        }
    }
    if let Ok(Some(v)) = db.get_setting("setup_tls_cert_path") {
        if !v.is_empty() {
            server_config.tls_cert_path = Some(v);
        }
    }
    if let Ok(Some(v)) = db.get_setting("setup_tls_key_path") {
        if !v.is_empty() {
            server_config.tls_key_path = Some(v);
        }
    }
    // domain needs to be reread now that we may have overridden it.
    let domain = server_config.domain.as_ref().cloned();

    // First-run setup is "complete" when a token is configured — either
    // in server.toml directly or stored by the wizard in the DB (already
    // overlaid above). Without a token kdcts can't accept tunnel
    // connections, so it always runs in setup mode until one is set.
    // No persisted flag — the check is purely "scan config, decide".
    let setup_complete = server_config.default_token.is_some();
    if !setup_complete {
        tracing::warn!("");
        tracing::warn!("  ╔══════════════════════════════════════════════════════════════════╗");
        tracing::warn!("  ║  SETUP REQUIRED                                                  ║");
        tracing::warn!("  ║  kdcts is running in setup mode — no auth token is configured.   ║");
        tracing::warn!("  ║  Open http://<this-host>:{:<5}/setup to finish configuration.    ║", http_port);
        tracing::warn!("  ╚══════════════════════════════════════════════════════════════════╝");
        tracing::warn!("");
    }

    // ACME / Let's Encrypt auto-TLS. When enabled, replaces the manual
    // tls_cert_path / tls_key_path with files we manage under a state dir.
    //
    // Precedence: if the panel has ever written ACME settings to the DB
    // (key `acme_enabled` present, regardless of value), the DB owns the
    // config — that way disabling via the panel actually disables ACME
    // even if server.toml still has `[server.acme] enabled = true`. If
    // the panel has never touched these settings, server.toml wins.
    // `directory_url` is only configurable from server.toml since the
    // panel doesn't expose it; we carry it forward in DB-mode.
    let db_acme_present = matches!(db.get_setting("acme_enabled"), Ok(Some(_)));
    let acme_cfg = if db_acme_present {
        let toml_base = server_config.acme.clone().unwrap_or_default();
        let enabled = db
            .get_setting("acme_enabled")
            .ok()
            .flatten()
            .map(|v| v == "true")
            .unwrap_or(false);
        let email = db
            .get_setting("acme_email")
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .or(toml_base.email);
        let staging = db
            .get_setting("acme_staging")
            .ok()
            .flatten()
            .map(|v| v == "true")
            .unwrap_or(toml_base.staging);
        let state_dir = db
            .get_setting("acme_state_dir")
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .or(toml_base.state_dir);
        Some(tunnel::config::AcmeConfig {
            enabled,
            email,
            staging,
            directory_url: toml_base.directory_url,
            state_dir,
        })
    } else {
        server_config.acme.clone()
    };
    let acme_manager: Option<Arc<acme::AcmeManager>> = match (&domain, &acme_cfg) {
        (Some(d), Some(cfg)) if cfg.enabled => match acme::AcmeManager::from_config(d, cfg) {
            Ok(m) => Some(Arc::new(m)),
            Err(e) => {
                tracing::error!("ACME config rejected: {:#}", e);
                None
            }
        },
        (None, Some(cfg)) if cfg.enabled => {
            tracing::error!(
                "acme.enabled = true requires `domain` to be set in server.toml"
            );
            None
        }
        _ => None,
    };

    // If ACME is on, run the issuance/renewal flow before Pingora binds.
    // After a successful flow, override the cert/key paths used by Pingora.
    let (tls_cert_path_eff, tls_key_path_eff) = match &acme_manager {
        Some(mgr) => {
            if mgr.cert_needs_issue_or_renew() {
                tracing::info!(
                    "Obtaining TLS cert via ACME for {} (state dir: {})",
                    mgr.domain,
                    mgr.state_dir.display()
                );
                // Pingora isn't up yet, so own http_port for the
                // duration of the flow.
                if let Err(e) = mgr.obtain_or_renew(Some(http_port)).await {
                    tracing::error!("ACME initial issuance failed: {:#}", e);
                }
            } else if let Some(days) = mgr.cert_days_remaining() {
                tracing::info!(
                    "Reusing on-disk ACME cert for {} ({} days remaining)",
                    mgr.domain,
                    days
                );
            }
            let cert = mgr.cert_path().to_string_lossy().to_string();
            let key = mgr.key_path().to_string_lossy().to_string();
            (Some(cert), Some(key))
        }
        None => (
            server_config.tls_cert_path.clone(),
            server_config.tls_key_path.clone(),
        ),
    };

    // Resolve TLS configuration: persisted toggle in DB + cert/key paths
    // (either from acme manager or from config). TLS additionally requires
    // a configured `domain`.
    let tls_paths_ok = match (tls_cert_path_eff.as_deref(), tls_key_path_eff.as_deref()) {
        (Some(c), Some(k)) if !c.is_empty() && !k.is_empty() => {
            std::path::Path::new(c).is_file() && std::path::Path::new(k).is_file()
        }
        _ => false,
    };
    let tls_configurable = domain.is_some() && tls_paths_ok;
    // ACME implies the user wants TLS. If ACME is enabled and our paths are
    // good, force the toggle on so the user doesn't need to flip it manually.
    let mut tls_persisted = db
        .get_setting("tls_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    if acme_manager.is_some() && tls_paths_ok && !tls_persisted {
        tls_persisted = true;
        let _ = db.set_setting("tls_enabled", "true");
        tracing::info!("ACME enabled — turning on persisted TLS toggle");
    }
    let tls_live = tls_persisted && tls_configurable;
    if tls_persisted && !tls_configurable {
        tracing::warn!(
            "tls_enabled=true is persisted but tls_cert_path/tls_key_path are missing or invalid \
             — falling back to plain HTTP. Fix the paths in server.toml and restart."
        );
    }
    let tls_for_proxy = if tls_live {
        Some(crate::proxy::TlsConfig {
            cert_path: tls_cert_path_eff.clone().unwrap_or_default(),
            key_path: tls_key_path_eff.clone().unwrap_or_default(),
        })
    } else {
        None
    };

    // Spawn renewal task once TLS is up. Runs once a day; only acts when
    // <30 days remain. The proxy's HTTPS-redirect listener serves the
    // ACME challenge path from the shared `mgr.challenges` map, so we
    // don't need to hand the renewal task a port to bind.
    if tls_live {
        if let Some(mgr) = acme_manager.clone() {
            acme::spawn_renewal_task(mgr);
        }
    }

    let route_table = Arc::new(RwLock::new(RouteTable::new()));

    let pool = PortPool::new(&server_config.port_pool).await?;
    let tracker = crate::deployment_tracker::new_tracker();

    // Mark all nodes offline on startup (they'll come back online when they reconnect)
    db.mark_all_offline()?;

    match &domain {
        Some(d) => tracing::info!("Starting KDCT server with domain: {}", d),
        None => tracing::info!(
            "Starting KDCT server without a domain — proxy will accept any Host (TLS disabled)"
        ),
    }

    match server_config.transport.transport_type {
        TransportType::Tcp => {
            let (node_update_tx, mut node_update_rx) =
                tokio::sync::mpsc::channel::<tunnel::node_update::NodeUpdate>(1024);

            // Seed the tunnel server's binding map (service_digest → node_uuid)
            // from SQLite so the spoof-prevention check survives restarts.
            let bindings = tunnel::registry::new_bindings();
            {
                let mut guard = bindings.write().await;
                for (uuid, digest) in db.load_bindings().unwrap_or_default() {
                    guard.insert(uuid, digest);
                }
                tracing::info!("Loaded {} node binding(s) from DB", guard.len());
            }

            let mut server = Server::<TcpTransport>::from(
                server_config.clone(),
                Some(pool.clone()),
                node_update_tx,
                bindings,
            )
            .await?;
            let clients = server.clients.clone();
            let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);

            // Shared map for Docker command results (container_name → result)
            let docker_results: Arc<RwLock<HashMap<String, Result<Vec<u16>, String>>>> =
                Arc::new(RwLock::new(HashMap::new()));

            // Sync node updates to SQLite and handle disconnect cleanup
            let sync_db_path = db_path.clone();
            let sync_rt = route_table.clone();
            let sync_pool = pool.clone();
            let sync_tracker = tracker.clone();
            let sync_docker_results = docker_results.clone();
            tokio::spawn(async move {
                while let Some(update) = node_update_rx.recv().await {
                    match update.event {
                        NodeEvent::Connected { service_digest, hostname, os, arch, docker_version, port_range_start, port_range_end, cpu_cores, memory_mb, running_containers: _ } => {
                            if let Ok(d) = Database::open(&sync_db_path) {
                                let _ = d.upsert_node(
                                    &update.uuid,
                                    &service_digest,
                                    &hostname, &os, &arch,
                                    &docker_version,
                                    port_range_start as i64,
                                    port_range_end as i64,
                                    cpu_cores as i64,
                                    memory_mb as i64,
                                );
                                // Node goes online — auto-check will be done by frontend polling
                            }
                        }
                        NodeEvent::Disconnected { hostname: _ } => {
                            if let Ok(d) = Database::open(&sync_db_path) {
                                // Mark connections as pending for this node
                                let _ = d.set_node_offline(&update.uuid);
                                if let Ok(Some(node)) = d.get_node_by_uuid(&update.uuid) {
                                    if let Ok(conn_ids) = d.get_connection_ids_for_node(node.id) {
                                        for cid in conn_ids {
                                            let _ = d.update_connection_node(cid, Some(node.id), "pending", None);
                                        }
                                    }
                                }
                            }
                            crate::deployment_tracker::remove_by_node(
                                &sync_tracker,
                                &sync_rt,
                                &sync_pool,
                                &update.uuid,
                            ).await;
                        }
                        NodeEvent::ContainerStarted { container_name, ports } => {
                            tracing::info!("docker_results insert: ContainerStarted {}", container_name);
                            let mut results = sync_docker_results.write().await;
                            results.insert(container_name, Ok(ports));
                        }
                        NodeEvent::ContainerStopped { container_name } => {
                            tracing::info!("docker_results insert: ContainerStopped {}", container_name);
                            let mut results = sync_docker_results.write().await;
                            results.insert(container_name, Ok(vec![]));
                        }
                        NodeEvent::ContainerError { container_name, error } => {
                            tracing::info!("docker_results insert: ContainerError {} — {}", container_name, error);
                            let mut results = sync_docker_results.write().await;
                            results.insert(container_name, Err(error));
                        }
                    }
                }
            });

            // Pingora reverse proxy
            let proxy_rt = route_table.clone();
            let proxy_domain = domain.clone();
            let proxy_acme_challenges = acme_manager.as_ref().map(|m| m.challenges.clone());
            tokio::spawn(async move {
                if let Err(e) = proxy::run_proxy(
                    proxy_rt,
                    proxy_domain,
                    api_port,
                    http_port,
                    https_port,
                    tls_for_proxy,
                    proxy_acme_challenges,
                )
                .await
                {
                    tracing::error!("Proxy error: {:#}", e);
                }
            });

            // HTTP API (serves web UI and REST API)
            let api_db = Arc::new(db);
            let api_clients = clients.clone();
            let api_rt = route_table.clone();
            let api_pool = pool.clone();
            let api_tracker = tracker.clone();
            let api_docker_results = docker_results.clone();
            let api_settings = api::ApiSettings {
                live_tls_enabled: tls_live,
                tls_configurable,
                domain_configured: domain.is_some(),
                http_port,
                https_port,
                api_port,
                admin_user: server_config.admin_user.clone(),
                admin_password: server_config.admin_password.clone(),
                setup_complete,
            };
            tokio::spawn(async move {
                if let Err(e) = api::run_api(
                    api_db,
                    api_clients,
                    api_rt,
                    api_pool,
                    api_tracker,
                    api_docker_results,
                    Default::default(),
                    api_settings,
                )
                .await
                {
                    tracing::error!("API error: {:#}", e);
                }
            });

            if setup_complete {
                server.run(shutdown_rx).await?;
            } else {
                // Setup mode: keep the panel + API up but don't bind the
                // tunnel listener. The wizard will exec-restart us once the
                // operator clicks Save, and we'll come back through this
                // branch with setup_complete=true.
                tracing::info!(
                    "Tunnel listener skipped (setup mode). Visit /setup to configure."
                );
                drop(server);
                let mut rx = shutdown_rx;
                let _ = rx.recv().await;
            }
            drop(shutdown_tx);
            Ok(())
        }
    }
}
