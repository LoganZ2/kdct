use anyhow::Result;
use serde_json::json;
use std::collections::HashMap;
use std::io::Cursor;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::runtime::Handle;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::db::Database;
use crate::deploy;
use crate::deployment_tracker::DeploymentTracker;
use crate::image;
use crate::proxy::RouteTable;
use tunnel::port_pool::PortPool;
use tunnel::registry::ClientRegistry;

const API_PORT: u16 = 9933;

type Resp = tiny_http::Response<Cursor<Vec<u8>>>;

#[derive(Clone)]
pub struct LoadJob {
    pub logs: Vec<String>,
    pub status: String,
    pub result: String,
}

pub type JobRegistry = Arc<Mutex<HashMap<String, LoadJob>>>;

fn panel_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../apps/kdct-panel/build")
}

pub async fn run_api(
    db: Arc<Database>,
    registry: ClientRegistry,
    route_table: Arc<RwLock<RouteTable>>,
    pool: Arc<PortPool>,
    tracker: DeploymentTracker,
    _docker_results: Arc<RwLock<HashMap<String, Result<Vec<u16>, String>>>>,
    job_registry: JobRegistry,
) -> Result<()> {
    let listener = TcpListener::bind(format!("127.0.0.1:{}", API_PORT))?;
    let server = tiny_http::Server::from_listener(listener, None)
        .map_err(|e| anyhow::anyhow!("{}", e))?;
    let panel = panel_dir();
    let job_registry = job_registry.clone();
    info!("Panel API listening on http://127.0.0.1:{}", API_PORT);
    info!("Serving panel from: {}", panel.display());

    let active_forwards: Arc<RwLock<Vec<(u16, tokio::sync::broadcast::Sender<bool>)>>> =
        Arc::new(RwLock::new(Vec::new()));

    tokio::task::spawn_blocking(move || {
        loop {
            let mut request = match server.recv() {
                Ok(r) => r,
                Err(e) => {
                    error!("API recv error: {}", e);
                    continue;
                }
            };

            let raw_path = request.url().to_string();
            let method = request.method();
            let path = raw_path.split('?').next().unwrap_or(&raw_path).to_string();
            let handle = Handle::current();

            let response = match (method, path.as_str()) {
                // ── Nodes ────────────────────────────────────────
                (&tiny_http::Method::Get, "/api/nodes") => {
                    match db.list_nodes() {
                        Ok(nodes) => {
                            let list: Vec<serde_json::Value> = nodes.iter().map(|n| json!({
                                "id": n.id, "hostname": n.hostname, "os": n.os, "arch": n.arch,
                                "docker_version": n.docker_version,
                                "port_range_start": n.port_range_start, "port_range_end": n.port_range_end,
                                "cpu_cores": n.cpu_cores, "memory_mb": n.memory_mb,
                                "status": n.status, "last_seen": n.last_seen,
                            })).collect();
                            respond_json(&json!(list))
                        }
                        Err(e) => error_json(&format!("{}", e), 500),
                    }
                }

                // ── Images ───────────────────────────────────────
                (&tiny_http::Method::Get, "/api/images") => {
                    match db.list_images() {
                        Ok(images) => {
                            let list: Vec<serde_json::Value> = images.iter().map(|i| json!({
                                "id": i.id, "name": i.name, "source": i.source,
                                "source_type": i.source_type, "status": i.status, "created_at": i.created_at,
                            })).collect();
                            respond_json(&json!(list))
                        }
                        Err(e) => error_json(&format!("{}", e), 500),
                    }
                }

                // ── Bridges ──────────────────────────────────────
                (&tiny_http::Method::Get, "/api/bridges") => {
                    match db.list_bridges() {
                        Ok(list) => respond_json(&json!(list)),
                        Err(e) => error_json(&format!("{}", e), 500),
                    }
                }

                (&tiny_http::Method::Get, p) if p.starts_with("/api/bridges/") && !p.contains("/port") && !p.contains("/env") => {
                    let rest = &p["/api/bridges/".len()..];
                    let (id_str, _) = rest.split_once('/').unwrap_or((rest, ""));
                    let bridge_id: i64 = match id_str.parse() {
                        Ok(id) => id,
                        Err(_) => return error_json("Invalid bridge id", 400),
                    };
                    match db.get_bridge_by_id(bridge_id) {
                        Ok(Some(b)) => {
                            let ports = db.get_bridge_ports(bridge_id).unwrap_or_default();
                            let envs = db.get_bridge_envs(bridge_id).unwrap_or_default();
                            let port_list: Vec<serde_json::Value> = ports.iter().map(|p| json!({
                                "id": p.id, "container_port": p.container_port,
                                "mode": p.mode, "route_path": p.route_path, "protocols": p.protocols,
                            })).collect();
                            let env_list: Vec<serde_json::Value> = envs.iter().map(|(k, v)| json!({
                                "key": k, "value": v,
                            })).collect();
                            respond_json(&json!({ "bridge": b, "ports": port_list, "envs": env_list }))
                        }
                        Ok(None) => error_json("Bridge not found", 404),
                        Err(e) => error_json(&format!("{}", e), 500),
                    }
                }

                (&tiny_http::Method::Post, "/api/bridges") => {
                    let mut body = String::new();
                    if request.as_reader().read_to_string(&mut body).is_err() {
                        return error_json("Failed to read body", 400);
                    }
                    let parsed: serde_json::Value = match serde_json::from_str(&body) {
                        Ok(v) => v,
                        Err(e) => return error_json(&format!("Invalid JSON: {}", e), 400),
                    };
                    let bridge_name = parsed["name"].as_str().unwrap_or("");
                    if bridge_name.is_empty() {
                        return error_json("Missing 'name'", 400);
                    }
                    match db.insert_bridge(bridge_name) {
                        Ok(id) => respond_json(&json!({"id": id, "name": bridge_name})),
                        Err(e) => error_json(&format!("{}", e), 500),
                    }
                }

                (&tiny_http::Method::Delete, p) if p.starts_with("/api/bridges/") && !p.contains("/port") => {
                    let rest = &p["/api/bridges/".len()..];
                    let (id_str, _) = rest.split_once('/').unwrap_or((rest, ""));
                    let bridge_id: i64 = match id_str.parse() { Ok(id) => id, Err(_) => return error_json("Invalid bridge id", 400) };
                    match db.delete_bridge(bridge_id) {
                        Ok(_) => respond_json(&json!({"ok": true})),
                        Err(e) => error_json(&format!("{}", e), 500),
                    }
                }

                (&tiny_http::Method::Post, p) if p.ends_with("/port") => {
                    let bridge_id = extract_bridge_id(p, "/port");
                    let bridge_id = match bridge_id { Some(id) => id, None => return error_json("Invalid bridge id", 400) };
                    let mut body = String::new();
                    if request.as_reader().read_to_string(&mut body).is_err() { return error_json("Failed to read body", 400); }
                    let parsed: serde_json::Value = match serde_json::from_str(&body) { Ok(v) => v, Err(e) => return error_json(&format!("Invalid JSON: {}", e), 400) };
                    let container_port = parsed["container_port"].as_i64().unwrap_or(0);
                    let mode = parsed["mode"].as_str().unwrap_or("route");
                    let route_path = parsed["route_path"].as_str();
                    let protocols = parsed["protocols"].as_array().map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(","));
                    let protocols_str = protocols.as_deref();
                    if container_port == 0 || (mode == "route" && route_path.is_none()) {
                        return error_json("Missing 'container_port' or 'route_path'", 400);
                    }
                    match db.insert_bridge_port(bridge_id, container_port, mode, route_path, protocols_str) {
                        Ok(_) => respond_json(&json!({"ok": true})),
                        Err(e) => error_json(&format!("{}", e), 500),
                    }
                }

                (&tiny_http::Method::Delete, p) if p.contains("/port/") => {
                    let rest = &p["/api/bridges/".len()..];
                    let parts: Vec<&str> = rest.split('/').collect();
                    if parts.len() < 3 { return error_json("Invalid path", 400); }
                    let bridge_id: i64 = match parts[0].parse() { Ok(id) => id, Err(_) => return error_json("Invalid bridge id", 400) };
                    let container_port: i64 = match parts[2].parse() { Ok(p) => p, Err(_) => return error_json("Invalid port", 400) };
                    match db.delete_bridge_port(bridge_id, container_port) {
                        Ok(_) => respond_json(&json!({"ok": true})),
                        Err(e) => error_json(&format!("{}", e), 500),
                    }
                }

                (&tiny_http::Method::Post, p) if p.ends_with("/env") => {
                    let bridge_id = extract_bridge_id(p, "/env");
                    let bridge_id = match bridge_id { Some(id) => id, None => return error_json("Invalid bridge id", 400) };
                    let mut body = String::new();
                    if request.as_reader().read_to_string(&mut body).is_err() { return error_json("Failed to read body", 400); }
                    let parsed: serde_json::Value = match serde_json::from_str(&body) { Ok(v) => v, Err(e) => return error_json(&format!("Invalid JSON: {}", e), 400) };
                    let envs = parsed["envs"].as_array();
                    if envs.is_none() { return error_json("Missing 'envs' array", 400); }
                    let pairs: Vec<(String, String)> = envs.unwrap().iter().filter_map(|e| {
                        let key = e["key"].as_str()?; let value = e["value"].as_str()?;
                        Some((key.to_string(), value.to_string()))
                    }).collect();
                    match db.set_bridge_envs(bridge_id, &pairs) {
                        Ok(_) => respond_json(&json!({"ok": true})),
                        Err(e) => error_json(&format!("{}", e), 500),
                    }
                }

                // ── Connections ───────────────────────────────────
                (&tiny_http::Method::Get, "/api/connections") => {
                    match db.list_connections() {
                        Ok(list) => respond_json(&json!(list)),
                        Err(e) => error_json(&format!("{}", e), 500),
                    }
                }

                (&tiny_http::Method::Post, "/api/connections") => {
                    let mut body = String::new();
                    if request.as_reader().read_to_string(&mut body).is_err() { return error_json("Failed to read body", 400); }
                    let parsed: serde_json::Value = match serde_json::from_str(&body) { Ok(v) => v, Err(e) => return error_json(&format!("Invalid JSON: {}", e), 400) };
                    let name = parsed["name"].as_str().unwrap_or("connection").to_string();
                    match db.insert_connection(&name) {
                        Ok(id) => {
                            let bridge_id = parsed["bridge_id"].as_i64();
                            let image_id = parsed["image_id"].as_i64();
                            let node_id = parsed["node_id"].as_i64();
                            if bridge_id.is_some() || image_id.is_some() || node_id.is_some() {
                                let _ = db.update_connection(id, bridge_id, image_id, node_id);
                            }
                            let db2 = db.clone();
                            let registry2 = registry.clone();
                            let rt2 = route_table.clone();
                            let pool2 = pool.clone();
                            let tracker2 = tracker.clone();
                            let fw2 = active_forwards.clone();
                            handle.spawn(async move {
                                try_auto_deploy(&db2, &registry2, &rt2, &pool2, &tracker2, &fw2, id).await;
                            });
                            respond_json(&json!({"id": id, "name": name}))
                        }
                        Err(e) => error_json(&format!("{}", e), 500),
                    }
                }

                (&tiny_http::Method::Patch, p) if p.starts_with("/api/connections/") => {
                    let rest = &p["/api/connections/".len()..];
                    let (id_str, _) = rest.split_once('/').unwrap_or((rest, ""));
                    let id: i64 = match id_str.parse() { Ok(id) => id, Err(_) => return error_json("Invalid id", 400) };
                    let mut body = String::new();
                    if request.as_reader().read_to_string(&mut body).is_err() { return error_json("Failed to read body", 400); }
                    let parsed: serde_json::Value = match serde_json::from_str(&body) { Ok(v) => v, Err(e) => return error_json(&format!("Invalid JSON: {}", e), 400) };
                    let bridge_id = parsed["bridge_id"].as_i64();
                    let image_id = parsed["image_id"].as_i64();
                    let node_id = parsed["node_id"].as_i64();
                    let current_bridge = parsed.get("bridge_id").is_some();
                    let current_image = parsed.get("image_id").is_some();
                    let current_node = parsed.get("node_id").is_some();
                    // If node changed and we were deployed, stop first
                    if current_node {
                        if let Ok(Some(conn)) = db.get_connection(id) {
                            if conn["status"].as_str() == Some("deployed") {
                                let _ = handle.block_on(stop_connection_safe(&db, &registry, &route_table, &pool, &tracker, &active_forwards, id));
                            }
                        }
                    }
                    match db.update_connection(id, bridge_id, image_id, node_id) {
                        Ok(_) => {
                            let db2 = db.clone();
                            let registry2 = registry.clone();
                            let rt2 = route_table.clone();
                            let pool2 = pool.clone();
                            let tracker2 = tracker.clone();
                            let fw2 = active_forwards.clone();
                            handle.spawn(async move {
                                try_auto_deploy(&db2, &registry2, &rt2, &pool2, &tracker2, &fw2, id).await;
                            });
                            respond_json(&json!({"ok": true}))
                        }
                        Err(e) => error_json(&format!("{}", e), 500),
                    }
                }

                (&tiny_http::Method::Delete, p) if p.starts_with("/api/connections/") => {
                    let rest = &p["/api/connections/".len()..];
                    let (id_str, _) = rest.split_once('/').unwrap_or((rest, ""));
                    let id: i64 = match id_str.parse() { Ok(id) => id, Err(_) => return error_json("Invalid id", 400) };
                    // Stop if deployed
                    if let Ok(Some(conn)) = db.get_connection(id) {
                        if conn["status"].as_str() == Some("deployed") {
                            let _ = handle.block_on(stop_connection_safe(&db, &registry, &route_table, &pool, &tracker, &active_forwards, id));
                        }
                    }
                    match db.delete_connection(id) {
                        Ok(_) => respond_json(&json!({"ok": true})),
                        Err(e) => error_json(&format!("{}", e), 500),
                    }
                }

                (&tiny_http::Method::Post, "/api/auto-check") => {
                    let db2 = db.clone();
                    let registry2 = registry.clone();
                    let rt2 = route_table.clone();
                    let pool2 = pool.clone();
                    let tracker2 = tracker.clone();
                    let fw2 = active_forwards.clone();
                    handle.block_on(async move {
                        auto_check(&db2, &registry2, &rt2, &pool2, &tracker2, &fw2).await;
                    });
                    respond_json(&json!({"ok": true}))
                }

                // ── Overview ──────────────────────────────────────
                (&tiny_http::Method::Get, "/api/overview") => {
                    let nodes = db.list_nodes().unwrap_or_default();
                    let images = db.list_images().unwrap_or_default();
                    let bridges = db.list_bridges().unwrap_or_default();
                    let connections = db.list_connections().unwrap_or_default();
                    let online = nodes.iter().filter(|n| n.status == "online").count();
                    let deployed_count = connections.iter().filter(|c| c["status"].as_str() == Some("deployed")).count();
                    let guard = handle.block_on(registry.read());
                    let container_count: usize = guard.iter().map(|(_, e)| e.running_containers.len()).sum();
                    drop(guard);
                    let pool_total = pool.total();
                    let pool_free = handle.block_on(pool.free_count());
                    respond_json(&json!({
                        "node_count": nodes.len(), "online_count": online,
                        "image_count": images.len(), "bridge_count": bridges.len(),
                        "connection_count": connections.len(), "deployed_count": deployed_count,
                        "container_count": container_count,
                        "pool_total": pool_total, "pool_free": pool_free,
                    }))
                }

                (&tiny_http::Method::Get, "/api/ping") => { respond_json(&json!({"ok": true})) }

                // ── Docker Hub search ────────────────────────────
                (&tiny_http::Method::Get, p) if p.starts_with("/api/search") => {
                    let query = raw_path.split('?').nth(1).and_then(|qs| qs.split('&').find_map(|pair| {
                        let mut kv = pair.splitn(2, '=');
                        if kv.next()? == "q" { kv.next().map(|s| s.to_string()) } else { None }
                    })).unwrap_or_default();
                    if query.is_empty() { respond_json(&json!([])) }
                    else {
                        match search_docker_hub(&query) {
                            Ok(results) => respond_json(&json!(results)),
                            Err(e) => error_json(&format!("{:#}", e), 500),
                        }
                    }
                }

                // ── Docker Hub tags ──────────────────────────────
                (&tiny_http::Method::Get, p) if p.starts_with("/api/tags") => {
                    let params: HashMap<String, String> = raw_path.split('?').nth(1).map(|qs| qs.split('&').filter_map(|pair| {
                        let mut kv = pair.splitn(2, '=');
                        Some((kv.next()?.to_string(), kv.next().unwrap_or("").to_string()))
                    }).collect()).unwrap_or_default();
                    let repo = params.get("repo").cloned().unwrap_or_default();
                    let page: u32 = params.get("page").and_then(|s| s.parse().ok()).unwrap_or(1);
                    if repo.is_empty() { respond_json(&json!({"tags": [], "next": null})) }
                    else {
                        match fetch_tags(&repo, page) {
                            Ok(result) => respond_json(&json!(result)),
                            Err(e) => error_json(&format!("{:#}", e), 500),
                        }
                    }
                }

                // ── Image load ───────────────────────────────────
                (&tiny_http::Method::Post, "/api/image/load") => {
                    let mut body = String::new();
                    if request.as_reader().read_to_string(&mut body).is_err() { return error_json("Failed to read body", 400); }
                    let parsed: serde_json::Value = match serde_json::from_str(&body) { Ok(v) => v, Err(e) => return error_json(&format!("Invalid JSON: {}", e), 400) };
                    let source = parsed["source"].as_str().unwrap_or("");
                    let custom_name = parsed["name"].as_str().filter(|s| !s.is_empty());
                    if source.is_empty() { return error_json("Missing 'source'", 400); }
                    let job_id = format!("{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
                    {
                        let mut jobs = job_registry.lock().unwrap();
                        jobs.insert(job_id.clone(), LoadJob { logs: vec!["Starting image load...".into()], status: "running".into(), result: String::new() });
                    }
                    let source = source.to_string();
                    let custom_name = custom_name.map(|s| s.to_string());
                    let db = db.clone();
                    let jobs = job_registry.clone();
                    let jid = job_id.clone();
                    handle.spawn(async move {
                        let mut log_line = |line: &str| {
                            if let Ok(mut j) = jobs.lock() { if let Some(job) = j.get_mut(&jid) { job.logs.push(line.to_string()); } }
                        };
                        log_line(&format!("Pulling: {}", source));
                        let is_docker = !(source.starts_with("http") || source.ends_with(".git"));
                        if is_docker {
                            match tokio::process::Command::new("docker").args(["pull", &source]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
                                Ok(mut child) => {
                                    if let Some(stderr) = child.stderr.take() {
                                        let reader = BufReader::new(stderr);
                                        let mut lines = reader.lines();
                                        while let Ok(Some(line)) = lines.next_line().await {
                                            let trimmed = line.trim().to_string();
                                            if !trimmed.is_empty() { log_line(&trimmed); }
                                        }
                                    }
                                    let status = child.wait().await;
                                    match status {
                                        Ok(s) if s.success() => log_line("Pull complete"),
                                        Ok(s) => log_line(&format!("Pull failed with code: {}", s.code().unwrap_or(-1))),
                                        Err(e) => log_line(&format!("Pull error: {}", e)),
                                    }
                                }
                                Err(e) => {
                                    log_line(&format!("Failed to start docker: {}", e));
                                    if let Ok(mut j) = jobs.lock() { if let Some(job) = j.get_mut(&jid) { job.status = "error".into(); job.result = format!("{}", e); } }
                                    return;
                                }
                            }
                        }
                        match image::load_image(db.as_ref(), &source, custom_name.as_deref()).await {
                            Ok(name) => {
                                if let Ok(mut j) = jobs.lock() { if let Some(job) = j.get_mut(&jid) { job.status = "done".into(); job.result = format!("Image {} loaded successfully", name); } }
                            }
                            Err(e) => {
                                log_line(&format!("{:#}", e));
                                if let Ok(mut j) = jobs.lock() { if let Some(job) = j.get_mut(&jid) { job.status = "error".into(); job.result = format!("{:#}", e); } }
                            }
                        }
                    });
                    respond_json(&json!({"job_id": job_id}))
                }

                (&tiny_http::Method::Get, p) if p.starts_with("/api/image/load/progress") => {
                    let job_id = raw_path.split('?').nth(1).and_then(|qs| qs.split('&').find_map(|pair| {
                        let mut kv = pair.splitn(2, '=');
                        if kv.next()? == "job" { kv.next().map(|s| s.to_string()) } else { None }
                    })).unwrap_or_default();
                    let jobs = job_registry.lock().unwrap();
                    match jobs.get(&job_id) {
                        Some(job) => respond_json(&json!({ "status": job.status, "logs": job.logs, "result": job.result })),
                        None => error_json("Job not found", 404),
                    }
                }

                // ── Image ports inspection ──────────────────────────
                (&tiny_http::Method::Get, p) if p.starts_with("/api/image/ports") => {
                    let query = raw_path.split('?').nth(1).and_then(|qs| qs.split('&').find_map(|pair| {
                        let mut kv = pair.splitn(2, '=');
                        if kv.next()? == "image" { kv.next().map(|s| s.to_string()) } else { None }
                    })).unwrap_or_default();
                    if query.is_empty() { respond_json(&json!([])) }
                    else {
                        match handle.block_on(image::inspect_exposed_ports(&query)) {
                            Ok(ports) => respond_json(&json!(ports)),
                            Err(e) => error_json(&format!("{:#}", e), 500),
                        }
                    }
                }

                _ => { serve_static(&panel, &path, &registry) }
            };

            if let Err(e) = request.respond(response) {
                error!("Failed to send response: {}", e);
            }
        }
    });

    Ok(())
}

async fn try_auto_deploy(
    db: &Arc<Database>, registry: &ClientRegistry, route_table: &Arc<RwLock<RouteTable>>,
    pool: &Arc<PortPool>, tracker: &DeploymentTracker,
    active_forwards: &Arc<RwLock<Vec<(u16, tokio::sync::broadcast::Sender<bool>)>>>,
    connection_id: i64,
) {
    if let Ok(Some(conn)) = db.get_connection(connection_id) {
        if conn["bridge_id"].is_null() || conn["image_id"].is_null() || conn["node_id"].is_null() { return; }
        if conn["status"].as_str() != Some("pending") { return; }
        if conn["node_status"].as_str() != Some("online") { return; }
        info!("Auto-deploying connection {}", connection_id);
        if let Err(e) = deploy::deploy_connection(db, registry, route_table, pool, tracker, connection_id, active_forwards).await {
            warn!("Auto-deploy connection {} failed: {:#}", connection_id, e);
        }
    }
}

async fn auto_check(
    db: &Arc<Database>, registry: &ClientRegistry, route_table: &Arc<RwLock<RouteTable>>,
    pool: &Arc<PortPool>, tracker: &DeploymentTracker,
    active_forwards: &Arc<RwLock<Vec<(u16, tokio::sync::broadcast::Sender<bool>)>>>,
) {
    if let Ok(list) = db.get_connectable_connections() {
        for c in list {
            let id = c["id"].as_i64().unwrap_or(0);
            if id > 0 {
                try_auto_deploy(db, registry, route_table, pool, tracker, active_forwards, id).await;
            }
        }
    }
}

async fn stop_connection_safe(
    db: &Database, registry: &ClientRegistry, route_table: &Arc<RwLock<RouteTable>>,
    pool: &Arc<PortPool>, tracker: &DeploymentTracker,
    active_forwards: &Arc<RwLock<Vec<(u16, tokio::sync::broadcast::Sender<bool>)>>>,
    connection_id: i64,
) {
    if let Err(e) = deploy::stop_connection(db, registry, route_table, pool, tracker, connection_id, active_forwards).await {
        warn!("Stop connection {} failed: {:#}", connection_id, e);
    }
}

fn extract_bridge_id(path: &str, suffix: &str) -> Option<i64> {
    let strip = path.strip_prefix("/api/bridges/")?.strip_suffix(suffix)?;
    strip.parse().ok()
}

fn respond_json(data: &serde_json::Value) -> Resp {
    let body = data.to_string();
    tiny_http::Response::from_string(body)
        .with_header(tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap())
}

fn error_json(msg: &str, code: u16) -> Resp {
    let body = json!({"error": msg}).to_string();
    let code = tiny_http::StatusCode(match code { 400 => 400, 404 => 404, _ => 500 });
    tiny_http::Response::from_string(body).with_status_code(code)
        .with_header(tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap())
}

fn serve_static(panel_dir: &PathBuf, path: &str, _registry: &ClientRegistry) -> Resp {
    let path = path.trim_start_matches('/');
    if path.is_empty() { return serve_file_or_fallback(panel_dir, "index.html"); }
    let file_path = panel_dir.join(path);
    if file_path.exists() && file_path.is_file() { return serve_file(&file_path, path); }
    let html_path = panel_dir.join(format!("{}.html", path));
    if html_path.exists() { return serve_file(&html_path, &format!("{}.html", path)); }
    let index_path = panel_dir.join(path).join("index.html");
    if index_path.exists() { return serve_file(&index_path, "index.html"); }
    serve_file_or_fallback(panel_dir, "index.html")
}

fn serve_file_or_fallback(panel_dir: &PathBuf, name: &str) -> Resp {
    let file_path = panel_dir.join(name);
    match std::fs::read(&file_path) {
        Ok(data) => tiny_http::Response::from_data(data).with_header(tiny_http::Header::from_bytes("Content-Type", "text/html").unwrap()),
        Err(_) => tiny_http::Response::from_string("Not Found").with_status_code(tiny_http::StatusCode(404)),
    }
}

fn serve_file(file_path: &PathBuf, name: &str) -> Resp {
    match std::fs::read(file_path) {
        Ok(data) => {
            let ct = content_type(name);
            tiny_http::Response::from_data(data).with_header(tiny_http::Header::from_bytes("Content-Type", ct).unwrap())
        }
        Err(_) => tiny_http::Response::from_string("Not Found").with_status_code(tiny_http::StatusCode(404)),
    }
}

fn content_type(path: &str) -> &'static str {
    if path.ends_with(".html") { "text/html" }
    else if path.ends_with(".css") { "text/css" }
    else if path.ends_with(".js") { "application/javascript" }
    else if path.ends_with(".json") { "application/json" }
    else if path.ends_with(".png") { "image/png" }
    else if path.ends_with(".svg") { "image/svg+xml" }
    else if path.ends_with(".woff2") { "font/woff2" }
    else { "application/octet-stream" }
}

fn percent_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex_s = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00");
            if let Ok(hex) = u8::from_str_radix(hex_s, 16) { result.push(hex as char); i += 3; continue; }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

fn search_docker_hub(query: &str) -> Result<Vec<serde_json::Value>> {
    let url = format!("https://hub.docker.com/v2/search/repositories/?query={}&page_size=15", urlencoding(query));
    let resp: ureq::Response = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(5)).timeout_read(std::time::Duration::from_secs(5))
        .build().get(&url).set("User-Agent", "kdct/0.1").set("Accept", "application/json").call()
        .map_err(|e| anyhow::anyhow!("Docker Hub request failed: {}", e))?;
    let body = resp.into_string().map_err(|e| anyhow::anyhow!("Failed to read response: {}", e))?;
    let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("Invalid response: {}", e))?;
    let results: Vec<serde_json::Value> = parsed["results"].as_array().cloned().unwrap_or_default().iter().filter_map(|r| {
        let name = r["repo_name"].as_str()?;
        Some(json!({
            "name": name, "description": r["short_description"].as_str().unwrap_or(""),
            "pull_count": r["pull_count"].as_i64().unwrap_or(0), "star_count": r["star_count"].as_i64().unwrap_or(0),
            "is_official": r["is_official"].as_bool().unwrap_or(false),
        }))
    }).collect();
    Ok(results)
}

fn urlencoding(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => result.push(byte as char),
            b' ' => result.push_str("%20"),
            _ => { result.push_str(&format!("%{:02X}", byte)); }
        }
    }
    result
}

fn fetch_tags(repo: &str, page: u32) -> Result<serde_json::Value> {
    let tags_url = if repo.contains('/') {
        format!("https://hub.docker.com/v2/repositories/{}/tags/?page={}&page_size=20", repo, page)
    } else {
        format!("https://hub.docker.com/v2/repositories/library/{}/tags/?page={}&page_size=20", repo, page)
    };
    let resp: ureq::Response = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(5)).timeout_read(std::time::Duration::from_secs(5))
        .build().get(&tags_url).set("User-Agent", "kdct/0.1").set("Accept", "application/json").call()
        .map_err(|e| anyhow::anyhow!("Docker Hub request failed: {}", e))?;
    let body = resp.into_string().map_err(|e| anyhow::anyhow!("Failed to read response: {}", e))?;
    let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("Invalid response: {}", e))?;
    let tags: Vec<String> = parsed["results"].as_array().unwrap_or(&vec![]).iter()
        .filter_map(|t| t["name"].as_str().map(|s| s.to_string())).collect();
    let has_next = parsed["next"].is_string();
    Ok(json!({ "tags": tags, "has_next": has_next }))
}
