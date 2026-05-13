use pingora::proxy::{ProxyHttp, Session};
use pingora::proxy::http_proxy_service;
use pingora::upstreams::peer::HttpPeer;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

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
    /// Public hostname to enforce on the Host header. `None` means accept
    /// any Host (nginx default_server behavior — useful for IP-only setups).
    pub domain: Option<String>,
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

        if let Some(expected) = &self.domain {
            if host != *expected {
                warn!("Rejected request for unknown host: {}", host);
                return Err(pingora::Error::new(pingora::ErrorType::HTTPStatus(404)));
            }
        }

        // SNI/authority for the upstream connection. We always speak plain
        // HTTP to the local tunnel endpoint, so the value is unused — just
        // pick something stable.
        let upstream_sni = self.domain.clone().unwrap_or_else(|| host.clone());

        // Reserved /admin prefix → forward to the panel API
        if is_admin_path(path) {
            info!("Proxy: {} → admin panel (127.0.0.1:{})", path, self.api_port);
            let peer = Box::new(HttpPeer::new(
                ("127.0.0.1", self.api_port),
                false,
                upstream_sni,
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
            upstream_sni,
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
    domain: Option<String>,
    api_port: u16,
    http_port: u16,
    https_port: u16,
    tls: Option<TlsConfig>,
    acme_challenges: Option<crate::acme::ChallengeMap>,
) -> anyhow::Result<()> {
    let proxy = KdctProxy {
        route_table,
        domain: domain.clone(),
        api_port,
    };

    let mut my_server = pingora::server::Server::new(None)?;
    my_server.bootstrap();

    let mut service = http_proxy_service(&my_server.configuration, proxy);

    let tls_on = tls.is_some();
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

    // When TLS is on, also bind http_port and 301 every request to https.
    // This is hardcoded — a server with TLS enabled never serves plain HTTP.
    // The same listener also serves `/.well-known/acme-challenge/*` so
    // background ACME renewals can run without fighting us for http_port.
    if tls_on {
        let redirect_addr = format!("0.0.0.0:{}", http_port);
        let redirect_https_port = https_port;
        let challenges = acme_challenges.clone();
        tokio::spawn(async move {
            if let Err(e) =
                run_https_redirect(&redirect_addr, redirect_https_port, challenges).await
            {
                error!("HTTPS redirect listener exited: {:#}", e);
            }
        });
        info!("HTTPS redirect: http://0.0.0.0:{} → https://...", http_port);
    }

    match &domain {
        Some(d) => info!("Pingora proxy listening on {} for domain {}", listen_summary, d),
        None => info!(
            "Pingora proxy listening on {} (no domain configured — accepting any Host)",
            listen_summary
        ),
    }

    my_server.add_service(service);

    tokio::task::spawn_blocking(move || {
        my_server.run_forever();
    })
    .await?;

    Ok(())
}

/// Minimal HTTP/1.x listener that 301-redirects every request to HTTPS on
/// the configured port. No keepalive, no body — just status line + a few
/// headers. Pingora is already serving on https_port; this is the matching
/// plain-HTTP companion that ensures `http://example.com/x` doesn't dead-end.
async fn run_https_redirect(
    addr: &str,
    https_port: u16,
    challenges: Option<crate::acme::ChallengeMap>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind HTTPS redirect on {}: {}", addr, e))?;
    info!("HTTPS redirect listening on {}", addr);

    loop {
        let (mut sock, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("Redirect accept error: {:#}", e);
                continue;
            }
        };
        let challenges = challenges.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_redirect(&mut sock, https_port, challenges).await {
                debug!("Redirect handler ({}) ended: {:#}", peer, e);
            }
        });
    }
}

async fn handle_redirect(
    sock: &mut tokio::net::TcpStream,
    https_port: u16,
    challenges: Option<crate::acme::ChallengeMap>,
) -> anyhow::Result<()> {
    use tokio::time::{timeout, Duration};

    // We only need the request line + Host header; cap reads to avoid being
    // used as a memory hog by malformed clients.
    let mut buf = [0u8; 4096];
    let mut filled: usize = 0;
    loop {
        if filled == buf.len() { break; }
        let n = match timeout(Duration::from_secs(5), sock.read(&mut buf[filled..])).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => break,
        };
        filled += n;
        if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }

    let head = std::str::from_utf8(&buf[..filled]).unwrap_or("");
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    // request line: METHOD SP target SP HTTP/version
    let mut parts = request_line.split(' ');
    let _method = parts.next().unwrap_or("GET");
    let target = parts.next().unwrap_or("/");

    // ACME HTTP-01: when a renewal is in flight, the challenges map holds
    // `token → key_authorization`. Serve those before redirecting so LE's
    // validator (which can't follow a 301 to HTTPS) gets the plaintext
    // response it needs.
    const ACME_PREFIX: &str = "/.well-known/acme-challenge/";
    if let Some(token) = target.strip_prefix(ACME_PREFIX) {
        if let Some(map) = &challenges {
            if let Some(key_auth) = map.read().await.get(token).cloned() {
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    key_auth.len(),
                    key_auth
                );
                sock.write_all(resp.as_bytes()).await?;
                sock.shutdown().await.ok();
                return Ok(());
            }
        }
        // Unknown token (or no ACME running) — 404 rather than redirecting
        // an ACME validator into HTTPS, which it won't follow.
        let resp = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        sock.write_all(resp).await?;
        sock.shutdown().await.ok();
        return Ok(());
    }

    // Find Host header
    let mut host_value: Option<&str> = None;
    for line in lines {
        if line.is_empty() { break; }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("host") {
                host_value = Some(value.trim());
                break;
            }
        }
    }

    // Host comes in as "example.com" or "example.com:80"; strip the port.
    let host = host_value
        .and_then(|h| h.split(':').next())
        .filter(|h| !h.is_empty())
        .unwrap_or("");

    let target = if target.is_empty() { "/" } else { target };
    let location = if host.is_empty() {
        // No Host header — best-effort: redirect to root on https with
        // whatever the client used. We can't construct an absolute URL
        // without a host, so fall back to a 400.
        let resp = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        sock.write_all(resp).await?;
        return Ok(());
    } else if https_port == 443 {
        format!("https://{}{}", host, target)
    } else {
        format!("https://{}:{}{}", host, https_port, target)
    };

    let resp = format!(
        "HTTP/1.1 301 Moved Permanently\r\n\
         Location: {}\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         \r\n",
        location
    );
    sock.write_all(resp.as_bytes()).await?;
    sock.shutdown().await.ok();
    Ok(())
}
