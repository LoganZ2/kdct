//! ACME (Let's Encrypt) auto-TLS.
//!
//! Issues and renews a certificate for the configured domain via HTTP-01
//! challenges. Persists the ACME account, cert, and private key under a
//! state directory so they survive restarts.
//!
//! Lifecycle:
//! 1. At startup (before Pingora binds), if a fresh cert is already on disk
//!    we skip the flow. Otherwise we bind a temporary HTTP listener on
//!    `http_port` that serves only `/.well-known/acme-challenge/*`, run the
//!    HTTP-01 flow, write `cert.pem` + `key.pem`, and drop the listener.
//! 2. A background task re-checks the cert daily. If it's within the
//!    renewal window it runs the same flow (port 80 is free at that point
//!    because Pingora is on `https_port`) and rewrites the on-disk files.
//!    Pingora picks the new cert up on next restart — see TLS docs.

use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{RwLock, Semaphore};
use tokio::time::timeout;
use tracing::{error, info, warn};

use tunnel::config::AcmeConfig;

const LE_DIRECTORY_PROD: &str = "https://acme-v02.api.letsencrypt.org/directory";
const LE_DIRECTORY_STAGING: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";

/// Renew when fewer than this many days of validity remain.
const RENEW_WITHIN_DAYS: i64 = 30;

#[derive(Clone)]
pub struct AcmeManager {
    pub domain: String,
    pub email: String,
    pub directory_url: String,
    pub state_dir: PathBuf,
}

impl AcmeManager {
    pub fn from_config(domain: &str, cfg: &AcmeConfig) -> Result<Self> {
        if !cfg.enabled {
            return Err(anyhow!("ACME is not enabled"));
        }
        let email = cfg
            .email
            .clone()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("acme.email is required when acme.enabled = true"))?;
        let directory_url = cfg.directory_url.clone().unwrap_or_else(|| {
            if cfg.staging { LE_DIRECTORY_STAGING.into() } else { LE_DIRECTORY_PROD.into() }
        });
        let state_dir_raw = cfg
            .state_dir
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("kdct-state").join("acme").join(domain));
        // Resolve to absolute so logs and renewal don't depend on CWD —
        // under systemd CWD is `/` and a relative path would surprise.
        let state_dir = if state_dir_raw.is_absolute() {
            state_dir_raw
        } else {
            std::env::current_dir()
                .map(|d| d.join(&state_dir_raw))
                .unwrap_or(state_dir_raw)
        };
        Ok(Self { domain: domain.to_string(), email, directory_url, state_dir })
    }

    pub fn cert_path(&self) -> PathBuf { self.state_dir.join("cert.pem") }
    pub fn key_path(&self) -> PathBuf { self.state_dir.join("key.pem") }
    pub fn account_path(&self) -> PathBuf { self.state_dir.join("account.json") }

    /// Returns `Some(days_until_expiry)` if a cert exists on disk and parses,
    /// or `None` if no cert is present or it can't be parsed.
    pub fn cert_days_remaining(&self) -> Option<i64> {
        let pem = std::fs::read(self.cert_path()).ok()?;
        let pem_str = std::str::from_utf8(&pem).ok()?;
        let der = first_cert_pem_to_der(pem_str)?;
        let (_, parsed) = x509_parser::parse_x509_certificate(&der).ok()?;
        let not_after = parsed.validity().not_after.timestamp();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs() as i64;
        Some((not_after - now) / 86400)
    }

    pub fn cert_needs_issue_or_renew(&self) -> bool {
        match self.cert_days_remaining() {
            None => true,
            Some(d) => d < RENEW_WITHIN_DAYS,
        }
    }

    /// Run the ACME flow against the configured directory. Binds `http_port`
    /// for the duration of the flow to serve HTTP-01 challenges, then drops
    /// the listener.
    pub async fn obtain_or_renew(&self, http_port: u16) -> Result<()> {
        tokio::fs::create_dir_all(&self.state_dir)
            .await
            .with_context(|| format!("Failed to create ACME state dir {}", self.state_dir.display()))?;
        // The state dir holds the ACME account key and the cert private
        // key — neither should be world-readable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(
                &self.state_dir,
                std::fs::Permissions::from_mode(0o700),
            )
            .await;
        }

        let challenges: Arc<RwLock<HashMap<String, String>>> = Arc::new(RwLock::new(HashMap::new()));

        let listen_addr = format!("0.0.0.0:{}", http_port);
        let listener = TcpListener::bind(&listen_addr)
            .await
            .with_context(|| format!("Failed to bind ACME challenge listener on {}", listen_addr))?;
        info!("ACME challenge listener on {}", listen_addr);

        let conn_semaphore: Arc<Semaphore> = Arc::new(Semaphore::new(64));

        let challenges_for_task = challenges.clone();
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        let listener_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    accept = listener.accept() => {
                        match accept {
                            Ok((sock, _)) => {
                                let challenges = challenges_for_task.clone();
                                let sem = conn_semaphore.clone();
                                tokio::spawn(async move {
                                    let _permit = sem.acquire().await;
                                    if let Err(e) = serve_challenge(sock, challenges).await {
                                        warn!("ACME challenge serve error: {:#}", e);
                                    }
                                });
                            }
                            Err(e) => {
                                warn!("ACME accept error: {:#}", e);
                            }
                        }
                    }
                    _ = &mut stop_rx => break,
                }
            }
        });

        let result = match timeout(Duration::from_secs(120), self.run_acme_flow(challenges)).await {
            Ok(inner) => inner,
            Err(_) => Err(anyhow!("ACME flow timed out after 2 minutes")),
        };
        let _ = stop_tx.send(());
        let _ = listener_handle.await;
        result
    }

    async fn run_acme_flow(&self, challenges: Arc<RwLock<HashMap<String, String>>>) -> Result<()> {
        use instant_acme::{
            Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier,
            NewAccount, NewOrder, OrderStatus,
        };

        // Load or create account credentials.
        let account = match tokio::fs::read(self.account_path()).await {
            Ok(bytes) => {
                let creds: AccountCredentials = serde_json::from_slice(&bytes)
                    .context("Failed to parse stored ACME account credentials")?;
                let builder = Account::builder().context("Failed to build ACME account")?;
                builder
                    .from_credentials(creds)
                    .await
                    .context("Failed to restore ACME account from credentials")?
            }
            Err(_) => {
                info!("Creating new ACME account at {}", self.directory_url);
                let contact = format!("mailto:{}", self.email);
                let builder = Account::builder().context("Failed to build ACME account")?;
                let (account, creds) = builder
                    .create(
                        &NewAccount {
                            contact: &[&contact],
                            terms_of_service_agreed: true,
                            only_return_existing: false,
                        },
                        self.directory_url.clone(),
                        None,
                    )
                    .await
                    .context("Failed to create ACME account")?;
                let json = serde_json::to_vec_pretty(&creds)
                    .context("Failed to serialize ACME credentials")?;
                let acct_path = self.account_path();
                tokio::fs::write(&acct_path, json)
                    .await
                    .context("Failed to persist ACME account credentials")?;
                restrict_perms(&acct_path).await;
                account
            }
        };

        let identifiers = [Identifier::Dns(self.domain.clone())];
        let mut order = account
            .new_order(&NewOrder::new(&identifiers))
            .await
            .context("Failed to create ACME order")?;

        info!("ACME order created for {}", self.domain);

        // For each pending authorization, pick the HTTP-01 challenge,
        // register the key authorization in the shared map so the listener
        // can serve it, then notify the ACME server.
        {
            let mut authorizations = order.authorizations();
            while let Some(authz_result) = authorizations.next().await {
                let mut authz = authz_result.context("Failed to fetch authorization")?;
                if authz.status != AuthorizationStatus::Pending {
                    continue;
                }
                let mut challenge = authz
                    .challenge(ChallengeType::Http01)
                    .ok_or_else(|| anyhow!("No HTTP-01 challenge offered for authorization"))?;

                let token = challenge.token.clone();
                let key_auth = challenge.key_authorization().as_str().to_string();
                challenges.write().await.insert(token, key_auth);
                challenge
                    .set_ready()
                    .await
                    .context("Failed to mark HTTP-01 challenge ready")?;
            }
        }

        // Poll for the order to become Ready (challenges validated).
        let mut attempt = 0u32;
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let state = order.refresh().await.context("Failed to refresh ACME order")?;
            match state.status {
                OrderStatus::Ready => break,
                OrderStatus::Pending => {
                    attempt += 1;
                    if attempt > 60 {
                        return Err(anyhow!("ACME order pending after ~2 minutes; aborting"));
                    }
                }
                OrderStatus::Invalid => {
                    return Err(anyhow!("ACME order became invalid: {:?}", state));
                }
                other => {
                    return Err(anyhow!("Unexpected ACME order state: {:?}", other));
                }
            }
        }

        // Finalize: instant-acme's `finalize()` (rcgen feature) generates a
        // CSR for the order's identifiers and returns the private key as PEM.
        let key_pem = order
            .finalize()
            .await
            .context("Failed to finalize ACME order")?;

        // Poll for the certificate.
        let mut attempt = 0u32;
        let cert_chain_pem = loop {
            match order.certificate().await {
                Ok(Some(c)) => break c,
                Ok(None) => {
                    attempt += 1;
                    if attempt > 30 {
                        return Err(anyhow!("ACME certificate not issued after ~1 minute"));
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(e) => return Err(anyhow!("Failed to fetch ACME certificate: {}", e)),
            }
        };

        write_atomic(&self.cert_path(), cert_chain_pem.as_bytes())
            .await
            .with_context(|| format!("Failed to write {}", self.cert_path().display()))?;
        write_atomic(&self.key_path(), key_pem.as_bytes())
            .await
            .with_context(|| format!("Failed to write {}", self.key_path().display()))?;
        restrict_perms(&self.key_path()).await;

        info!(
            "ACME cert written to {} (key: {})",
            self.cert_path().display(),
            self.key_path().display()
        );
        Ok(())
    }
}

/// Spawn a background task that wakes once a day, checks cert expiry, and
/// runs the renewal flow when within the renewal window. The renewal flow
/// briefly binds `http_port` (Pingora is on `https_port` while TLS is on,
/// so the port is free).
///
/// New cert bytes are written to disk; the running Pingora keeps using its
/// in-memory copy until the next restart. For LE this is a 60+ day window
/// after renewal completes — plenty of lead time.
pub fn spawn_renewal_task(manager: Arc<AcmeManager>, http_port: u16) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(86400));
        // Skip the first immediate tick — we just issued at startup.
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Some(days) = manager.cert_days_remaining() {
                if days >= RENEW_WITHIN_DAYS {
                    info!("ACME cert has {} days remaining; no renewal needed", days);
                    continue;
                }
                info!("ACME cert has {} days remaining — renewing", days);
            } else {
                warn!("ACME cert is unreadable on disk — attempting fresh issuance");
            }
            match manager.obtain_or_renew(http_port).await {
                Ok(()) => {
                    info!("ACME cert renewed successfully — restarting to apply");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    crate::self_restart();
                }
                Err(e) => error!("ACME renewal failed: {:#}", e),
            }
        }
    });
}

async fn serve_challenge(
    mut sock: tokio::net::TcpStream,
    challenges: Arc<RwLock<HashMap<String, String>>>,
) -> Result<()> {
    let mut buf = [0u8; 4096];
    let mut filled = 0usize;
    loop {
        if filled == buf.len() { break; }
        let n = match timeout(Duration::from_secs(5), sock.read(&mut buf[filled..])).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => break,
        };
        filled += n;
        if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n") { break; }
    }
    let head = std::str::from_utf8(&buf[..filled]).unwrap_or("");
    let request_line = head.split("\r\n").next().unwrap_or("");
    let mut parts = request_line.split(' ');
    let _method = parts.next().unwrap_or("GET");
    let target = parts.next().unwrap_or("/");

    const PREFIX: &str = "/.well-known/acme-challenge/";
    let token = target.strip_prefix(PREFIX);
    let body = match token {
        Some(t) => challenges.read().await.get(t).cloned(),
        None => None,
    };
    match body {
        Some(b) => {
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                b.len(),
                b
            );
            sock.write_all(resp.as_bytes()).await?;
        }
        None => {
            let resp = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            sock.write_all(resp).await?;
        }
    }
    sock.shutdown().await.ok();
    Ok(())
}

async fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, data).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// Restrict a file to 0o600 on Unix. Used for the private key and the
/// ACME account credentials. Best-effort: ignored on non-Unix.
async fn restrict_perms(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await;
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Extract the first PEM block's DER bytes from a string. Used for the
/// leaf cert when computing remaining validity.
fn first_cert_pem_to_der(pem: &str) -> Option<Vec<u8>> {
    let begin = pem.find("-----BEGIN CERTIFICATE-----")?;
    let after_begin = &pem[begin..];
    let end_marker = "-----END CERTIFICATE-----";
    let end = after_begin.find(end_marker)?;
    let body = &after_begin[
        "-----BEGIN CERTIFICATE-----".len()..end
    ];
    let cleaned: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(cleaned).ok()
}
