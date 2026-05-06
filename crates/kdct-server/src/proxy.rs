use pingora::proxy::{ProxyHttp, Session};
use pingora::proxy::http_proxy_service;
use pingora::upstreams::peer::HttpPeer;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Logical route entry.
#[derive(Debug, Clone)]
pub struct Route {
    pub path: String,
    pub port: u16,
}

/// RouteTable maps path → localhost port (rathole tunnel endpoint).
#[derive(Debug, Clone, Default)]
pub struct RouteTable {
    routes: HashMap<String, u16>,
}

impl RouteTable {
    pub fn new() -> Self {
        RouteTable {
            routes: HashMap::new(),
        }
    }

    pub fn set(&mut self, path: &str, port: u16) -> anyhow::Result<()> {
        if self.routes.contains_key(path) {
            anyhow::bail!("Path '{}' is already in use", path);
        }
        self.routes.insert(path.to_string(), port);
        info!("Route added: {} → localhost:{}", path, port);
        Ok(())
    }

    pub fn remove(&mut self, path: &str) {
        self.routes.remove(path);
        info!("Route removed: {}", path);
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

    pub fn dump(&self) -> Vec<Route> {
        self.routes
            .iter()
            .map(|(path, &port)| Route {
                path: path.clone(),
                port,
            })
            .collect()
    }
}

pub struct KdctProxy {
    pub route_table: Arc<RwLock<RouteTable>>,
    pub domain: String,
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

pub async fn run_proxy(
    route_table: Arc<RwLock<RouteTable>>,
    domain: String,
    http_port: u16,
    https_port: u16,
) -> anyhow::Result<()> {
    let proxy = KdctProxy {
        route_table,
        domain: domain.clone(),
    };

    let mut my_server = pingora::server::Server::new(None)?;
    my_server.bootstrap();

    let mut service = http_proxy_service(&my_server.configuration, proxy);

    let mut addrs = vec![format!("0.0.0.0:{}", http_port)];
    if https_port != 0 {
        addrs.push(format!("0.0.0.0:{}", https_port));
    }
    service.add_tcp(&addrs.join(","));

    info!("Pingora proxy listening on {} for domain {}", addrs.join(","), domain);

    my_server.add_service(service);

    tokio::task::spawn_blocking(move || {
        my_server.run_forever();
    })
    .await?;

    Ok(())
}
