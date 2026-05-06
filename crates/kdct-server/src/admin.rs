//! Local TCP admin API for kdct-server and CLI helpers.
//!
//! Protocol: JSON-lines over TCP (one JSON object per line, newline-delimited).
//! Only listens on 127.0.0.1 (localhost) for security.

use rathole::protocol::{ControlChannelCmd, PipelineStep};
use rathole::registry::ClientRegistry;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, RwLock};

pub const DEFAULT_ADMIN_PORT: u16 = 9921;

// ── Shared wire types (used by both server and CLI) ─────────────

#[derive(Deserialize, Debug)]
#[serde(tag = "cmd")]
pub enum AdminCommand {
    #[serde(rename = "list")]
    List,
    #[serde(rename = "pipeline")]
    Pipeline {
        client: String,
        steps: Vec<PipelineStep>,
    },
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum AdminResponse {
    #[serde(rename = "clients")]
    Clients { data: Vec<ClientInfo> },
    #[serde(rename = "pipeline_sent")]
    PipelineSent { id: String },
    #[serde(rename = "pipeline_output")]
    PipelineOutput {
        id: String,
        step: String,
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
    },
    #[serde(rename = "error")]
    Error { msg: String },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ClientInfo {
    pub name: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub ports: Vec<String>,
}

type Subscribers = Arc<RwLock<Vec<mpsc::UnboundedSender<String>>>>;

// ── Admin server (runs inside kdct-server start) ─────────────────

pub async fn run_admin(
    port: u16,
    registry: ClientRegistry,
    mut pipeline_output_rx: mpsc::Receiver<(rathole::protocol::Digest, ControlChannelCmd)>,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<bool>,
    web_output_buf: Option<Arc<RwLock<Vec<String>>>>,
    log_dir: Option<PathBuf>,
) -> anyhow::Result<()> {
    use tracing::{error, info, warn};

    let addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind admin on {}: {}", addr, e))?;
    info!("Admin API listening on {}", addr);

    let registry = Arc::new(registry);
    let registry_for_log = registry.clone();

    // Subscribers for pipeline output streaming
    let output_subscribers: Subscribers = Arc::new(RwLock::new(Vec::new()));

    // Forward pipeline output to server stdout AND all subscribers AND web buffer AND log files
    {
        let subscribers = output_subscribers.clone();
        tokio::spawn(async move {
            while let Some((digest, cmd)) = pipeline_output_rx.recv().await {
                if let ControlChannelCmd::PipelineOutput {
                    id,
                    step,
                    stdout,
                    stderr,
                    exit_code,
                } = cmd
                {
                    let stdout_str = String::from_utf8_lossy(&stdout).into_owned();
                    let stderr_str = String::from_utf8_lossy(&stderr).into_owned();

                    if !stdout_str.is_empty() {
                        print!("{}", stdout_str);
                    }
                    if !stderr_str.is_empty() {
                        eprint!("{}", stderr_str);
                    }

                    // Write to per-client log file
                    if let Some(ref dir) = log_dir {
                        let client_name = registry_for_log.read().await
                            .get(&digest)
                            .map(|e| e.service_name.clone())
                            .unwrap_or_else(|| "unknown".to_string());
                        let log_path = dir.join(format!("{}.log", client_name));
                        // Format: [HH:MM:SS] [step] output
                        let now = {
                            let t = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default();
                            let secs = t.as_secs();
                            format!("{:02}:{:02}:{:02}", (secs/3600)%24, (secs/60)%60, secs%60)
                        };
                        let mut entry = String::new();
                        if !stdout_str.is_empty() {
                            entry.push_str(&format!("[{}] [{}] {}\n", now, step, stdout_str.trim_end()));
                        }
                        if !stderr_str.is_empty() {
                            entry.push_str(&format!("[{}] [{}] [stderr] {}\n", now, step, stderr_str.trim_end()));
                        }
                        if let Some(code) = exit_code {
                            entry.push_str(&format!("[{}] [{}] exit={}\n", now, step, code));
                        }
                        if !entry.is_empty() {
                            if let Ok(mut f) = tokio::fs::OpenOptions::new()
                                .create(true).append(true).open(&log_path).await
                            {
                                let _ = tokio::io::AsyncWriteExt::write_all(&mut f, entry.as_bytes()).await;
                            }
                        }
                    }

                    let resp = AdminResponse::PipelineOutput {
                        id,
                        step,
                        stdout: stdout_str,
                        stderr: stderr_str,
                        exit_code,
                    };
                    if let Ok(json) = serde_json::to_string(&resp) {
                        let line = json + "\n";
                        for tx in subscribers.read().await.iter() {
                            let _ = tx.send(line.clone());
                        }
                        // Also write to web output buffer
                        if let Some(ref buf) = web_output_buf {
                            buf.write().await.push(line);
                        }
                    }
                }
            }
        });
    }

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        info!("Admin connection from {}", addr);
                        let registry = registry.clone();
                        let subscribers = output_subscribers.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_admin_conn(stream, registry, subscribers).await {
                                warn!("Admin error: {:#}", e);
                            }
                        });
                    }
                    Err(e) => error!("Admin accept error: {:#}", e),
                }
            }
            _ = shutdown_rx.recv() => {
                info!("Admin server shutting down");
                break;
            }
        }
    }

    Ok(())
}

async fn handle_admin_conn(
    stream: TcpStream,
    registry: Arc<ClientRegistry>,
    output_subscribers: Subscribers,
) -> anyhow::Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let cmd: AdminCommand = match serde_json::from_str(&line) {
            Ok(c) => c,
            Err(e) => {
                let resp = AdminResponse::Error {
                    msg: format!("Invalid command: {}", e),
                };
                let _ = send_response(&mut writer, &resp).await;
                continue;
            }
        };

        match cmd {
            AdminCommand::List => {
                let guard: tokio::sync::RwLockReadGuard<'_, _> = registry.read().await;
                let clients: Vec<ClientInfo> = guard
                    .values()
                    .map(|e| ClientInfo {
                        name: e.service_name.clone(),
                        hostname: e.hostname.clone(),
                        os: e.os.clone(),
                        arch: e.arch.clone(),
                        ports: e.ports.clone(),
                    })
                    .collect();
                drop(guard);
                let resp = AdminResponse::Clients { data: clients };
                let _ = send_response(&mut writer, &resp).await;
            }
            AdminCommand::Pipeline { client, steps } => {
                let guard = registry.read().await;
                let entry = match guard.values().find(|e: &&rathole::registry::ClientEntry| e.service_name == client) {
                    Some(e) => e.clone(),
                    None => {
                        drop(guard);
                        let resp = AdminResponse::Error {
                            msg: format!("Client not found: {}", client),
                        };
                        let _ = send_response(&mut writer, &resp).await;
                        continue;
                    }
                };
                drop(guard);

                let pipeline_id = simple_uid();

                let (sub_tx, mut sub_rx): (mpsc::UnboundedSender<String>, mpsc::UnboundedReceiver<String>) =
                    mpsc::unbounded_channel();
                output_subscribers.write().await.push(sub_tx);

                let _ = entry
                    .pipeline_tx
                    .send(ControlChannelCmd::RunPipeline {
                        id: pipeline_id.clone(),
                        steps,
                    })
                    .await;

                let ack = AdminResponse::PipelineSent {
                    id: pipeline_id.clone(),
                };
                let _ = send_response(&mut writer, &ack).await;

                while let Some(line) = sub_rx.recv().await {
                    if writer.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

async fn send_response(
    writer: &mut tokio::io::WriteHalf<TcpStream>,
    resp: &AdminResponse,
) -> anyhow::Result<()> {
    let json = serde_json::to_string(resp)?;
    writer.write_all((json + "\n").as_bytes()).await?;
    Ok(())
}

fn simple_uid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    format!("{:x}", ts as u64)
}

// ── CLI client helpers ──────────────────────────────────────────

/// Connect to the admin server, send a command, print responses.
pub async fn admin_request(port: u16, cmd_json: &str) -> anyhow::Result<()> {
    use tokio::io::AsyncBufReadExt;
    use tokio::io::AsyncWriteExt;

    let addr = format!("127.0.0.1:{}", port);
    let mut stream = TcpStream::connect(&addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to admin server on {}: {}", addr, e))?;

    // Send command
    let mut buf = cmd_json.as_bytes().to_vec();
    buf.push(b'\n');
    stream.write_all(&buf).await?;
    stream.flush().await?;

    // Read responses
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        match serde_json::from_str::<AdminResponse>(trimmed) {
            Ok(resp) => match resp {
                AdminResponse::Clients { data } => {
                    if data.is_empty() {
                        println!("No connected clients.");
                    } else {
                        println!("{:<20} {:<20} {:<12} {:<8} {}", "NAME", "HOSTNAME", "OS", "ARCH", "PORTS");
                        for c in &data {
                            println!("{:<20} {:<20} {:<12} {:<8} {:?}", c.name, c.hostname, c.os, c.arch, c.ports);
                        }
                    }
                    break;
                }
                AdminResponse::PipelineSent { id } => {
                    println!("Pipeline {} dispatched. Streaming output:\n", id);
                }
                AdminResponse::PipelineOutput {
                    id: _,
                    step,
                    stdout,
                    stderr,
                    exit_code,
                } => {
                    if !stdout.is_empty() {
                        print!("{}", stdout);
                    }
                    if !stderr.is_empty() {
                        eprint!("{}", stderr);
                    }
                    if let Some(code) = exit_code {
                        println!("\n--- Step '{}' finished (exit: {}) ---", step, code);
                        if code != 0 {
                            break;
                        }
                    }
                }
                AdminResponse::Error { msg } => {
                    eprintln!("Error: {}", msg);
                    break;
                }
            },
            Err(_) => {
                println!("{}", trimmed);
            }
        }
    }

    Ok(())
}
