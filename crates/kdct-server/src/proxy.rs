use pingora::proxy::{ProxyHttp, Session};
use pingora::proxy::http_proxy_service;
use pingora::upstreams::peer::HttpPeer;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Path prefix reserved for the admin panel + REST API.
/// Bridges may not use route paths under this prefix.
pub const ADMIN_PREFIX: &str = "/admin";

/// True if `path` falls under the reserved `/admin` prefix.
pub fn is_admin_path(path: &str) -> bool {
    path == ADMIN_PREFIX || path.starts_with("/admin/")
}

/// RouteTable maps path → localhost port (tunnel endpoint).
#[derive(Debug, Clone, Default)]
pub struct RouteTable {
    routes: HashMap<String, u16>,
}

impl RouteTable {
    pub fn new() -> Self {
        RouteTable { routes: HashMap::new() }
    }

    pub fn set(&mut self, path: &str, port: u16) -> anyhow::Result<()> {
        if is_admin_path(path) {
            anyhow::bail!("Path '{}' is reserved for the admin panel", path);
        }
        if self.routes.contains_key(path) {
            anyhow::bail!("Path '{}' is already in use", path);
        }
        self.routes.insert(path.to_string(), port);
        info!("Route added: {} → localhost:{}", path, port);
        Ok(())
    }

    pub fn remove_by_port(&mut self, port: u16) {
        self.routes.retain(|_path, p| *p != port);
    }

    pub fn resolve(&self, path: &str) -> Option<u16> {
        if let Some(&port) = self.routes.get(path) {
            return Some(port);
        }
        let mut best: Option<(&str, u16)> = None;
        for (route_path, &port) in &self.routes {
            if path.starts_with(route_path.as_str()) {
                match best {
                    Some((existing, _)) if route_path.len() > existing.len() => {
                        best = Some((route_path, port));
                    }
                    None => {
                        best = Some((route_path, port));
                    }
                    _ => {}
                }
            }
        }
        best.map(|(_, p)| p)
    }
}

pub struct KdctProxy {
    pub route_table: Arc<RwLock<RouteTable>>,
    pub domain: String,
    pub api_port: u16,
}

#[async_trait::async_trait]
impl ProxyHttp for KdctProxy {
    type CTX = ();
    fn new_ctx(&self) -> () {
        ()
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut (),
    ) -> pingora::Result<Box<HttpPeer>> {
        let header = session.req_header();
        let host = match header.uri.host() {
            Some(h) if !h.is_empty() => h.to_string(),
            _ => {
                let host_header = header
                    .headers
                    .get("host")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                host_header.split(':').next().unwrap_or("").to_string()
            }
        };
        let path = header.uri.path();

        if host != self.domain {
            warn!("Rejected request for unknown host: {}", host);
            return Err(pingora::Error::new(pingora::ErrorType::HTTPStatus(404)));
        }

        // Reserved /admin prefix → forward to the panel API
        if is_admin_path(path) {
            info!("Proxy: {} → admin panel (127.0.0.1:{})", path, self.api_port);
            let peer = Box::new(HttpPeer::new(
                ("127.0.0.1", self.api_port),
                false,
                self.domain.clone(),
            ));
            return Ok(peer);
        }

        let table = self.route_table.read().await;
        let port = table
            .resolve(path)
            .ok_or_else(|| pingora::Error::new(pingora::ErrorType::HTTPStatus(404)))?;

        info!("Proxy: {} → localhost:{}", path, port);

        let peer = Box::new(HttpPeer::new(
            ("127.0.0.1", port),
            false,
            self.domain.clone(),
        ));
        Ok(peer)
    }
}

/// TLS configuration resolved at startup.
pub struct TlsConfig {
    pub cert_path: String,
    pub key_path: String,
}

pub async fn run_proxy(
    route_table: Arc<RwLock<RouteTable>>,
    domain: String,
    api_port: u16,
    http_port: u16,
    https_port: u16,
    tls: Option<TlsConfig>,
) -> anyhow::Result<()> {
    let proxy = KdctProxy {
        route_table,
        domain: domain.clone(),
        api_port,
    };

    let mut my_server = pingora::server::Server::new(None)?;
    my_server.bootstrap();

    let mut service = http_proxy_service(&my_server.configuration, proxy);

    let listen_summary = match tls {
        Some(tls) => {
            let addr = format!("0.0.0.0:{}", https_port);
            service
                .add_tls(&addr, &tls.cert_path, &tls.key_path)
                .map_err(|e| anyhow::anyhow!("Failed to enable TLS on {}: {}", addr, e))?;
            format!("https://{} (cert={}, key={})", addr, tls.cert_path, tls.key_path)
        }
        None => {
            let addr = format!("0.0.0.0:{}", http_port);
            service.add_tcp(&addr);
            format!("http://{}", addr)
        }
    };

    info!("Pingora proxy listening on {} for domain {}", listen_summary, domain);

    my_server.add_service(service);

    tokio::task::spawn_blocking(move || {
        my_server.run_forever();
    })
    .await?;

    Ok(())
}
