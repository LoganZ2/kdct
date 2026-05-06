//! Simple web dashboard for kdct-server.
//!
//! Serves an embedded HTML page with vanilla JS. No build step, no npm.
//! API routes wrap the existing registry and pipeline functionality.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    response::Html,
    routing::{get, post},
};
use rathole::protocol::{ControlChannelCmd, PipelineStep};
use rathole::registry::ClientRegistry;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

// ── Shared state ──────────────────────────────────────────────

pub struct WebState {
    pub registry: ClientRegistry,
    pub log_dir: PathBuf,
}

// ── API response types ────────────────────────────────────────

#[derive(Serialize)]
struct ClientInfo {
    name: String,
    hostname: String,
    os: String,
    arch: String,
    ports: Vec<String>,
}

#[derive(Deserialize)]
struct PipelineRequest {
    client: String,
    steps: Vec<PipelineStep>,
}

#[derive(Serialize)]
struct ApiResult<T: Serialize> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// ── Routes ────────────────────────────────────────────────────

pub async fn run_web(
    port: u16,
    registry: ClientRegistry,
    log_dir: PathBuf,
) -> anyhow::Result<()> {
    let state = Arc::new(WebState {
        registry,
        log_dir,
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/clients", get(list_clients))
        .route("/api/pipeline", post(send_pipeline))
        .route("/api/logs/:client", get(get_client_logs))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("Web UI listening on http://{}", addr);

    axum::serve(listener, app).await?;
    Ok(())
}

// ── Handlers ──────────────────────────────────────────────────

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn list_clients(State(state): State<Arc<WebState>>) -> Json<ApiResult<Vec<ClientInfo>>> {
    let guard = state.registry.read().await;
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
    Json(ApiResult {
        ok: true,
        data: Some(clients),
        error: None,
    })
}

async fn send_pipeline(
    State(state): State<Arc<WebState>>,
    Json(req): Json<PipelineRequest>,
) -> Json<ApiResult<String>> {
    let guard = state.registry.read().await;
    let entry = match guard.values().find(|e| e.service_name == req.client) {
        Some(e) => e.clone(),
        None => {
            return Json(ApiResult {
                ok: false,
                data: None,
                error: Some(format!("Client not found: {}", req.client)),
            });
        }
    };
    drop(guard);

    let pipeline_id = format!("{:x}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros());

    let _ = entry
        .pipeline_tx
        .send(ControlChannelCmd::RunPipeline {
            id: pipeline_id.clone(),
            steps: req.steps,
        })
        .await;

    Json(ApiResult {
        ok: true,
        data: Some(pipeline_id),
        error: None,
    })
}

async fn get_client_logs(
    State(state): State<Arc<WebState>>,
    Path(client): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<ApiResult<String>> {
    let max_lines: usize = params
        .get("lines")
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let log_path = state.log_dir.join(format!("{}.log", client));
    let content = match tokio::fs::read_to_string(&log_path).await {
        Ok(c) => c,
        Err(_) => {
            return Json(ApiResult {
                ok: true,
                data: Some(String::new()),
                error: None,
            });
        }
    };
    if max_lines == 0 {
        return Json(ApiResult { ok: true, data: Some(content), error: None });
    }
    let all_lines: Vec<&str> = content.lines().collect();
    let start = if all_lines.len() > max_lines { all_lines.len() - max_lines } else { 0 };
    let tail: String = all_lines[start..].join("\n");
    Json(ApiResult { ok: true, data: Some(tail), error: None })
}

// ── Embedded HTML dashboard ───────────────────────────────────

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>KDCT Dashboard</title>
<style>
  * { box-sizing:border-box; margin:0; padding:0; }
  body { font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif; background:#0f172a; color:#e2e8f0; min-height:100vh; }
  .container { max-width:960px; margin:0 auto; padding:24px 16px; }
  h1 { font-size:20px; font-weight:600; margin-bottom:24px; color:#f1f5f9; }
  h2 { font-size:14px; font-weight:600; color:#94a3b8; text-transform:uppercase; letter-spacing:.05em; margin:24px 0 12px; }
  .card { background:#1e293b; border-radius:8px; padding:16px; margin-bottom:16px; }
  table { width:100%; border-collapse:collapse; }
  th { text-align:left; font-size:12px; color:#94a3b8; padding:8px 12px; border-bottom:1px solid #334155; }
  td { padding:10px 12px; font-size:13px; border-bottom:1px solid #1e293b; }
  tr.client-row { cursor:pointer; }
  tr.client-row:hover td { background:#33415540; }
  tr.client-row.active td { background:#3b82f633; }
  .status-dot { display:inline-block; width:8px; height:8px; border-radius:50%; background:#22c55e; margin-right:6px; }
  .empty { text-align:center; color:#64748b; padding:32px; font-size:14px; }
  .btn { background:#3b82f6; color:#fff; border:none; padding:8px 14px; border-radius:6px; font-size:13px; cursor:pointer; }
  .btn:hover { background:#2563eb; }
  .btn:disabled { opacity:.5; cursor:not-allowed; }
  .btn.active { background:#22c55e; }
  select, textarea { background:#0f172a; color:#e2e8f0; border:1px solid #334155; border-radius:6px; padding:8px 12px; font-size:13px; }
  select { width:auto; }
  textarea { font-family:monospace; resize:vertical; min-height:100px; width:100%; }
  .form-row { margin-bottom:12px; }
  .form-row label { display:block; font-size:12px; color:#94a3b8; margin-bottom:4px; }
  .output { background:#0f172a; border-radius:6px; padding:12px; font-family:monospace; font-size:12px; max-height:400px; overflow-y:auto; white-space:pre-wrap; }
  .badge { display:inline-block; padding:2px 8px; border-radius:10px; font-size:11px; background:#334155; }
  .flex { display:flex; gap:12px; align-items:center; flex-wrap:wrap; }
  .grow { flex:1; }
  .hidden { display:none; }
  .log-header { justify-content:space-between; margin-bottom:8px; }
  .muted { font-size:12px; color:#94a3b8; }
</style>
</head>
<body>
<div class="container">
  <h1>KDCT Dashboard</h1>

  <h2>Connected Clients <span class="muted">— click a client to view logs</span></h2>
  <div class="card">
    <table><thead><tr><th>Name</th><th>Hostname</th><th>OS</th><th>Arch</th><th>Ports</th></tr></thead>
    <tbody id="client-table"><tr><td class="empty" colspan="5">Loading…</td></tr></tbody></table>
  </div>

  <!-- Per-client log viewer -->
  <div id="log-viewer" class="card hidden">
    <div class="flex log-header">
      <span style="font-weight:600;" id="log-client-name"></span>
      <div class="flex">
        <span class="muted">Lines:</span>
        <button class="btn lines-btn" data-lines="100">100</button>
        <button class="btn lines-btn" data-lines="200">200</button>
        <button class="btn lines-btn" data-lines="500">500</button>
        <button class="btn lines-btn" data-lines="0">All</button>
        <button class="btn" onclick="refreshLogs()" style="background:#475569;">Refresh</button>
        <button class="btn" onclick="closeLogs()" style="background:#ef4444;">Close</button>
      </div>
    </div>
    <div class="output" id="log-output">—</div>
    <div class="flex" style="margin-top:6px;justify-content:space-between;">
      <span class="muted" id="log-info"></span>
      <span class="muted">Auto-refresh: <span id="log-timer-status">off</span></span>
    </div>
  </div>

  <h2>Send Pipeline</h2>
  <div class="card">
    <div class="form-row">
      <label>Target Client</label>
      <select id="pipeline-client"></select>
    </div>
    <div class="form-row">
      <label>Pipeline Steps (JSON)</label>
      <textarea id="pipeline-steps">[
  {"name":"hello","command":"echo 'Hello from KDCT'","timeout_secs":10},
  {"name":"pwd","command":"pwd","timeout_secs":0}
]</textarea>
    </div>
    <button class="btn" id="pipeline-send" onclick="sendPipeline()">Send Pipeline</button>
    <span id="pipeline-status" style="font-size:13px;margin-left:12px;"></span>
  </div>

</div>

<script>
let timer;
let selectedClient = null;
let logLines = 100;
let logRefreshInterval = null;
let currentLogData = '';

function $(id) { return document.getElementById(id); }

// ── Client table ────────────────────────────────────

function renderClients(clients) {
  let sel = $('pipeline-client');
  sel.innerHTML = '';
  if (clients.length === 0) {
    $('client-table').innerHTML = '<tr><td class="empty" colspan="5">No connected clients</td></tr>';
    if (selectedClient) closeLogs();
    return;
  }
  let html = '';
  clients.forEach(function(c) {
    let active = (selectedClient === c.name) ? ' active' : '';
    html += '<tr class="client-row' + active + '" onclick="selectClient(\'' + c.name.replace(/'/g,"\\'") + '\')">';
    html += '<td><span class="status-dot"></span>' + c.name + '</td>';
    html += '<td>' + c.hostname + '</td><td>' + c.os + '</td><td>' + c.arch + '</td>';
    html += '<td>' + (c.ports||[]).join(', ') + '</td></tr>';
    let opt = document.createElement('option');
    opt.value = c.name;
    opt.textContent = c.name;
    sel.appendChild(opt);
  });
  $('client-table').innerHTML = html;
}

// ── Log viewer ──────────────────────────────────────

function selectClient(name) {
  selectedClient = name;
  logLines = 100;
  // Update row highlights
  document.querySelectorAll('tr.client-row').forEach(function(r) {
    r.classList.remove('active');
    if (r.cells[0].textContent.trim() === name) r.classList.add('active');
  });
  // Update buttons
  document.querySelectorAll('.lines-btn').forEach(function(b) {
    b.classList.toggle('active', parseInt(b.dataset.lines) === logLines);
  });
  // Show viewer
  $('log-viewer').classList.remove('hidden');
  $('log-client-name').textContent = 'Logs: ' + name;
  fetchLogs();
  // Auto-refresh logs every 3s
  if (logRefreshInterval) clearInterval(logRefreshInterval);
  logRefreshInterval = setInterval(fetchLogs, 3000);
  $('log-timer-status').textContent = 'on (3s)';
}

function closeLogs() {
  selectedClient = null;
  $('log-viewer').classList.add('hidden');
  document.querySelectorAll('tr.client-row').forEach(function(r) { r.classList.remove('active'); });
  if (logRefreshInterval) { clearInterval(logRefreshInterval); logRefreshInterval = null; }
  $('log-timer-status').textContent = 'off';
}

function refreshLogs() { if (selectedClient) fetchLogs(); }

async function fetchLogs() {
  if (!selectedClient) return;
  try {
    let r = await fetch('/api/logs/' + encodeURIComponent(selectedClient) + '?lines=' + logLines);
    let j = await r.json();
    if (j.ok && j.data !== undefined) {
      let newData = j.data;
      $('log-output').textContent = newData || '(empty)';
      $('log-output').scrollTop = $('log-output').scrollHeight;
      let count = newData ? newData.split('\n').filter(function(l){return l.trim()}).length : 0;
      $('log-info').textContent = count + ' lines shown (of ' + (newData?newData.split('\n').length:0) + ' total)';
      currentLogData = newData;
    }
  } catch(e) { $('log-info').textContent = 'Error loading logs'; }
}

// Line count buttons
document.addEventListener('click', function(e) {
  if (e.target.classList.contains('lines-btn')) {
    logLines = parseInt(e.target.dataset.lines);
    document.querySelectorAll('.lines-btn').forEach(function(b) {
      b.classList.toggle('active', parseInt(b.dataset.lines) === logLines);
    });
    fetchLogs();
  }
});

// ── Polling ─────────────────────────────────────────

async function refresh() {
  try {
    let r = await fetch('/api/clients');
    let j = await r.json();
    if (j.ok && j.data) renderClients(j.data);
  } catch(e) {}
}

// ── Pipeline ────────────────────────────────────────

async function sendPipeline() {
  let client = $('pipeline-client').value;
  if (!client) { alert('No client selected'); return; }
  let stepsText = $('pipeline-steps').value;
  let steps;
  try { steps = JSON.parse(stepsText); } catch(e) { alert('Invalid JSON: ' + e.message); return; }

  let btn = $('pipeline-send');
  btn.disabled = true;
  $('pipeline-status').textContent = 'Sending...';

  try {
    let r = await fetch('/api/pipeline', {
      method:'POST',
      headers:{'Content-Type':'application/json'},
      body: JSON.stringify({client:client, steps:steps})
    });
    let j = await r.json();
    if (j.ok) {
      $('pipeline-status').textContent = 'Pipeline ' + j.data + ' dispatched';
    } else {
      $('pipeline-status').textContent = 'Error: ' + j.error;
    }
  } catch(e) {
    $('pipeline-status').textContent = 'Error: ' + e.message;
  }
  btn.disabled = false;
}

refresh();
timer = setInterval(refresh, 1500);
</script>
</body>
</html>"#;
