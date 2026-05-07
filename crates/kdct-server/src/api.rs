use anyhow::Result;
use serde_json::json;
use std::collections::HashMap;
use std::io::Cursor;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::RwLock;
use tracing::{error, info};

use crate::db::Database;
use crate::deploy;
use crate::deployment_tracker::DeploymentTracker;
use crate::image;
use crate::proxy::RouteTable;
use tunnel::port_pool::PortPool;
use tunnel::registry::ClientRegistry;

const API_PORT: u16 = 9933;

type Resp = tiny_http::Response<Cursor<Vec<u8>>>;

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
    docker_results: Arc<RwLock<HashMap<String, Result<Vec<u16>, String>>>>,
) -> Result<()> {
    let listener = TcpListener::bind(format!("127.0.0.1:{}", API_PORT))?;
    let server = tiny_http::Server::from_listener(listener, None)
        .map_err(|e| anyhow::anyhow!("{}", e))?;
    let panel = panel_dir();
    info!("Panel API listening on http://127.0.0.1:{}", API_PORT);
    info!("Serving panel from: {}", panel.display());

    tokio::task::spawn_blocking(move || {
        loop {
            let mut request = match server.recv() {
                Ok(r) => r,
                Err(e) => {
                    error!("API recv error: {}", e);
                    continue;
                }
            };

            let path = request.url().to_string();
            let method = request.method();

            let path = path.split('?').next().unwrap_or(&path).to_string();

            let handle = Handle::current();

            let response = match (method, path.as_str()) {
                (&tiny_http::Method::Get, "/api/nodes") => {
                    match db.list_nodes() {
                        Ok(nodes) => {
                            let list: Vec<serde_json::Value> = nodes.iter().map(|n| json!({
                                "id": n.id,
                                "hostname": n.hostname,
                                "os": n.os,
                                "arch": n.arch,
                                "docker_version": n.docker_version,
                                "port_range_start": n.port_range_start,
                                "port_range_end": n.port_range_end,
                                "cpu_cores": n.cpu_cores,
                                "memory_mb": n.memory_mb,
                                "status": n.status,
                                "last_seen": n.last_seen,
                            })).collect();
                            respond_json(&json!(list))
                        }
                        Err(e) => error_json(&format!("{}", e), 500),
                    }
                }

                (&tiny_http::Method::Get, "/api/images") => {
                    match db.list_images() {
                        Ok(images) => {
                            let list: Vec<serde_json::Value> = images.iter().map(|i| json!({
                                "id": i.id,
                                "name": i.name,
                                "source": i.source,
                                "source_type": i.source_type,
                                "status": i.status,
                                "created_at": i.created_at,
                            })).collect();
                            respond_json(&json!(list))
                        }
                        Err(e) => error_json(&format!("{}", e), 500),
                    }
                }

                (&tiny_http::Method::Get, p) if p.starts_with("/api/images/") => {
                    let name = &p["/api/images/".len()..];
                    let name = percent_decode(name);
                    let img = match db.get_image_by_name(&name) {
                        Ok(Some(i)) => i,
                        Ok(None) => return error_json("Image not found", 404),
                        Err(e) => return error_json(&format!("{}", e), 500),
                    };
                    let ports = db.get_image_routes(img.id).unwrap_or_default();
                    let envs = db.get_image_envs(img.id).unwrap_or_default();
                    let port_list: Vec<serde_json::Value> = ports.iter().map(|(p, route)| json!({
                        "id": p.id,
                        "image_node_id": p.image_node_id,
                        "port": p.port,
                        "protocol": p.protocol,
                        "route_path": route,
                    })).collect();
                    let env_list: Vec<serde_json::Value> = envs.iter().map(|(k, v)| json!({
                        "key": k,
                        "value": v,
                    })).collect();
                    respond_json(&json!({
                        "id": img.id,
                        "name": img.name,
                        "source": img.source,
                        "source_type": img.source_type,
                        "status": img.status,
                        "created_at": img.created_at,
                        "ports": port_list,
                        "envs": env_list,
                    }))
                }

                (&tiny_http::Method::Get, "/api/deployments") => {
                    let guard = handle.block_on(registry.read());
                    let mut list = Vec::new();
                    for (_digest, entry) in guard.iter() {
                        for c in &entry.running_containers {
                            list.push(json!({
                                "container_name": c.container_name,
                                "image": c.image,
                                "hostname": entry.hostname,
                                "ports": c.ports,
                                "status": c.status,
                            }));
                        }
                    }
                    respond_json(&json!(list))
                }

                (&tiny_http::Method::Get, "/api/overview") => {
                    let nodes = db.list_nodes().unwrap_or_default();
                    let images = db.list_images().unwrap_or_default();
                    let online = nodes.iter().filter(|n| n.status == "online").count();
                    let configured = images.iter().filter(|i| i.status == "configured").count();

                    let guard = handle.block_on(registry.read());
                    let container_count: usize = guard.iter().map(|(_, e)| e.running_containers.len()).sum();
                    drop(guard);

                    respond_json(&json!({
                        "node_count": nodes.len(),
                        "online_count": online,
                        "image_count": images.len(),
                        "configured_count": configured,
                        "deployment_count": 0,
                        "container_count": container_count,
                    }))
                }

                (&tiny_http::Method::Get, "/api/ping") => {
                    respond_json(&json!({"ok": true}))
                }

                (&tiny_http::Method::Get, p) if p.starts_with("/api/search") => {
                    let query = p.split('?').nth(1).and_then(|qs| {
                        qs.split('&').find_map(|pair| {
                            let mut kv = pair.splitn(2, '=');
                            if kv.next()? == "q" { kv.next().map(|s| s.to_string()) } else { None }
                        })
                    }).unwrap_or_default();
                    if query.is_empty() {
                        respond_json(&json!([]))
                    } else {
                        match search_docker_hub(&query) {
                            Ok(results) => respond_json(&json!(results)),
                            Err(e) => error_json(&format!("{:#}", e), 500),
                        }
                    }
                }

                (&tiny_http::Method::Post, "/api/image/load") => {
                    let mut body = String::new();
                    if request.as_reader().read_to_string(&mut body).is_err() {
                        return error_json("Failed to read body", 400);
                    }
                    let parsed: serde_json::Value = match serde_json::from_str(&body) {
                        Ok(v) => v,
                        Err(e) => return error_json(&format!("Invalid JSON: {}", e), 400),
                    };
                    let source = parsed["source"].as_str().unwrap_or("");
                    if source.is_empty() {
                        return error_json("Missing 'source'", 400);
                    }

                    let db2 = db.clone();
                    let source = source.to_string();
                    match handle.block_on(async move { image::load_image(db2.as_ref(), &source).await }) {
                        Ok(name) => tiny_http::Response::from_string(format!("Image {} loaded successfully", name)),
                        Err(e) => error_json(&format!("{:#}", e), 500),
                    }
                }

                (&tiny_http::Method::Post, "/api/deploy") => {
                    let mut body = String::new();
                    if request.as_reader().read_to_string(&mut body).is_err() {
                        return error_json("Failed to read body", 400);
                    }
                    let parsed: serde_json::Value = match serde_json::from_str(&body) {
                        Ok(v) => v,
                        Err(e) => return error_json(&format!("Invalid JSON: {}", e), 400),
                    };
                    let image = parsed["image"].as_str().unwrap_or("");
                    let node_id = parsed["node_id"].as_i64().unwrap_or(0);
                    if image.is_empty() || node_id == 0 {
                        return error_json("Missing 'image' or 'node_id'", 400);
                    }

                    let db2 = db.clone();
                    let registry2 = registry.clone();
                    let rt2 = route_table.clone();
                    let pool2 = pool.clone();
                    let tracker2 = tracker.clone();
                    let dr2 = docker_results.clone();
                    let image = image.to_string();

                    let result = handle.block_on(async move {
                        deploy::deploy_image(
                            db2.as_ref(),
                            &registry2,
                            &rt2,
                            &pool2,
                            &tracker2,
                            &dr2,
                            &image,
                            node_id,
                        ).await
                    });

                    match result {
                        Ok(msg) => tiny_http::Response::from_string(msg),
                        Err(e) => error_json(&format!("{:#}", e), 500),
                    }
                }

                (&tiny_http::Method::Post, "/api/stop") => {
                    let mut body = String::new();
                    if request.as_reader().read_to_string(&mut body).is_err() {
                        return error_json("Failed to read body", 400);
                    }
                    let parsed: serde_json::Value = match serde_json::from_str(&body) {
                        Ok(v) => v,
                        Err(e) => return error_json(&format!("Invalid JSON: {}", e), 400),
                    };
                    let image = parsed["image"].as_str().unwrap_or("");
                    let node_id = parsed["node_id"].as_i64().unwrap_or(0);
                    if image.is_empty() || node_id == 0 {
                        return error_json("Missing 'image' or 'node_id'", 400);
                    }

                    let db2 = db.clone();
                    let registry2 = registry.clone();
                    let rt2 = route_table.clone();
                    let pool2 = pool.clone();
                    let tracker2 = tracker.clone();
                    let dr2 = docker_results.clone();
                    let image = image.to_string();

                    let result = handle.block_on(async move {
                        deploy::stop_image(
                            db2.as_ref(),
                            &registry2,
                            &rt2,
                            &pool2,
                            &tracker2,
                            &dr2,
                            &image,
                            node_id,
                        ).await
                    });

                    match result {
                        Ok(msg) => tiny_http::Response::from_string(msg),
                        Err(e) => error_json(&format!("{:#}", e), 500),
                    }
                }

                _ => {
                    serve_static(&panel, &path, handle.clone(), &registry)
                }
            };

            if let Err(e) = request.respond(response) {
                error!("Failed to send response: {}", e);
            }
        }
    });

    Ok(())
}

fn respond_json(data: &serde_json::Value) -> Resp {
    let body = data.to_string();
    tiny_http::Response::from_string(body)
        .with_header(tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap())
}

fn error_json(msg: &str, code: u16) -> Resp {
    let body = json!({"error": msg}).to_string();
    let code = tiny_http::StatusCode(match code {
        400 => 400,
        404 => 404,
        _ => 500,
    });
    tiny_http::Response::from_string(body)
        .with_status_code(code)
        .with_header(tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap())
}

fn serve_static(panel_dir: &PathBuf, path: &str, _handle: Handle, _registry: &ClientRegistry) -> Resp {
    let path = path.trim_start_matches('/');

    if path.is_empty() {
        return serve_file_or_fallback(panel_dir, "index.html");
    }

    let file_path = panel_dir.join(path);
    if file_path.exists() && file_path.is_file() {
        return serve_file(&file_path, path);
    }

    let html_path = panel_dir.join(format!("{}.html", path));
    if html_path.exists() {
        return serve_file(&html_path, &format!("{}.html", path));
    }

    let index_path = panel_dir.join(path).join("index.html");
    if index_path.exists() {
        return serve_file(&index_path, "index.html");
    }

    serve_file_or_fallback(panel_dir, "index.html")
}

fn serve_file_or_fallback(panel_dir: &PathBuf, name: &str) -> Resp {
    let file_path = panel_dir.join(name);
    match std::fs::read(&file_path) {
        Ok(data) => {
            tiny_http::Response::from_data(data)
                .with_header(tiny_http::Header::from_bytes("Content-Type", "text/html").unwrap())
        }
        Err(_) => tiny_http::Response::from_string("Not Found")
            .with_status_code(tiny_http::StatusCode(404)),
    }
}

fn serve_file(file_path: &PathBuf, name: &str) -> Resp {
    match std::fs::read(file_path) {
        Ok(data) => {
            let ct = content_type(name);
            tiny_http::Response::from_data(data)
                .with_header(tiny_http::Header::from_bytes("Content-Type", ct).unwrap())
        }
        Err(_) => tiny_http::Response::from_string("Not Found")
            .with_status_code(tiny_http::StatusCode(404)),
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
            if let Ok(hex) = u8::from_str_radix(hex_s, 16) {
                result.push(hex as char);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

fn search_docker_hub(query: &str) -> Result<Vec<serde_json::Value>> {
    let url = format!(
        "https://hub.docker.com/v2/search/repositories/?query={}&page_size=15",
        urlencoding(query)
    );
    let resp: ureq::Response = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(5))
        .timeout_read(std::time::Duration::from_secs(5))
        .build()
        .get(&url)
        .set("User-Agent", "kdct/0.1")
        .set("Accept", "application/json")
        .call()
        .map_err(|e| anyhow::anyhow!("Docker Hub request failed: {}", e))?;

    let body = resp
        .into_string()
        .map_err(|e| anyhow::anyhow!("Failed to read response: {}", e))?;

    let parsed: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("Invalid response: {}", e))?;

    let results_list = parsed["results"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let results: Vec<serde_json::Value> = results_list
        .iter()
        .filter_map(|r| {
            let name = r["repo_name"].as_str()?;
            if name.contains('/') { return None; }
            Some(json!({
                "name": name,
                "description": r["short_description"].as_str().unwrap_or(""),
                "pull_count": r["pull_count"].as_i64().unwrap_or(0),
                "star_count": r["star_count"].as_i64().unwrap_or(0),
                "is_official": r["is_official"].as_bool().unwrap_or(false),
            }))
        })
        .collect();

    Ok(results)
}

fn urlencoding(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            b' ' => result.push_str("%20"),
            _ => {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    result
}
