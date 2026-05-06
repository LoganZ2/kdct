# KDCT — Tunnel + Pipeline Tool (based on rathole)

## Context

Build a frp-like tool with pipeline execution. Since we'll eventually support TCP/UDP, we use **rathole** as the tunnel foundation — it already has TCP/UDP forwarding, Noise/TLS encryption, WebSocket transport, and a clean Transport trait. We extend it with pipeline execution, server TUI panel, and client Tauri panel.

## Why rathole

rathole (`crates/rathole/`) is a pure-Rust frp alternative with exactly the architecture we need:

| Component | File | What it does |
|-----------|------|-------------|
| `Transport` trait | `src/transport/mod.rs:52` | Abstraction over TCP/TLS/Noise/WS |
| `Client<T>` | `src/client.rs:84` | Client state machine (connects, handles data channels) |
| `Server<T>` | `src/server.rs:96` | Server state machine (accepts, manages port forwarding) |
| Protocol | `src/protocol.rs` | Binary bincode protocol (Hello/Auth/ControlChannelCmd/DataChannelCmd) |
| Config | `src/config.rs:234` | TOML config with services, transport, tokens |

**Key insight**: rathole's `Transport` trait means we can tunnel ANY stream — TCP, UDP, and later HTTP — through the same encrypted channel. No web framework lock-in.

**Problem**: rathole's internals are `pub(crate)`. We need to make them `pub` to extend.

## Integration Strategy

**Fork and extend** — bring rathole into our workspace as `crates/rathole/`, make internal types public, add our features on top.

```
/Users/zhuangkaiyi/kdct/
├── Cargo.toml                          # Workspace root
├── crates/
│   ├── rathole/                        # Forked rathole (extended)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs                  # NOW PUB: Transport, Client, Server, etc.
│   │       ├── transport/              # TCP, TLS, Noise, WebSocket (unchanged)
│   │       ├── client.rs              # Client<T> (extended with pipeline dispatch)
│   │       ├── server.rs              # Server<T> (extended with status events)
│   │       ├── protocol.rs            # Extended: PipelineCmd, StatusReport
│   │       └── config.rs              # Extended: pipeline config section
│   ├── kdct-server/                    # Server binary (thin layer over rathole)
│   │   └── src/
│   │       ├── main.rs                 # Parse args, init TUI + rathole server
│   │       └── panel.rs               # ratatui TUI: clients, mappings, pipelines
│   └── kdct-client/                    # Client binary (thin layer over rathole)
│       └── src/
│           ├── main.rs                 # Parse args, init rathole client
│           └── pipeline.rs            # Pipeline executor (subprocess manager)
└── apps/
    └── kdct-panel/                     # Tauri v2 app (client config UI)
        ├── src/                        # Svelte frontend
        └── src-tauri/
            ├── Cargo.toml
            └── src/lib.rs              # Tauri commands
```

## What We Change in rathole

### 1. Make types public (visibility changes only) ✅ DONE

- `src/lib.rs`: `pub mod transport; pub mod protocol; pub mod client; pub mod server; pub mod config; pub mod helper; pub mod constants;`
- `src/transport/mod.rs`: `pub(crate) use TlsTransport` → `pub use TlsTransport`
- `src/client.rs`: `Client<T>`, `ControlChannelHandle` → `pub`
- `src/server.rs`: `Server<T>` → `pub` (ControlChannelHandle already `pub`)

### 2. Extend protocol (new message types)

**Problem**: Protocol uses fixed-size reads (`read_control_cmd` reads `PACKET_LEN.c_cmd` bytes). Adding sized variants like `RunPipeline { id, steps }` breaks this.

**Solution**: Change to length-prefixed encoding (read 2-byte length, then payload).

```rust
// Updated control channel commands (length-prefixed)
pub enum ControlChannelCmd {
    CreateDataChannel,
    HeartBeat,
    RunPipeline { id: PipelineId, steps: Vec<PipelineStep> },
    CancelPipeline { id: PipelineId },
    ReportStatus { hostname: String, os: String, arch: String },
}

// New data channel commands
pub enum DataChannelCmd {
    StartForwardTcp,
    StartForwardUdp,
    StartForwardHttp { path_prefix: Option<String>, host: Option<String> },
}
```

### 3. Extend Config (new service types + pipeline config)

```rust
pub enum ServiceType {
    Tcp,
    Udp,
    Http,  // NEW
}
```

### 4. Pipeline execution on client

New module: pipeline execution engine that:
- Receives `RunPipeline` via control channel
- Spawns subprocesses with `tokio::process::Command`
- Streams stdout/stderr back via control channel
- Reports step results

## What We Build New

### kdct-server — Server binary

- Parses CLI args (config file path)
- Initializes rathole server with config
- Runs ratatui TUI in a separate thread
- TUI receives events from the server (client connect/disconnect, pipeline output, status)

### kdct-client — Client binary

- Parses CLI args (config file path)
- Initializes rathole client with config
- Pipeline executor hooks into the client's control channel

### kdct-panel — Tauri app

- Simple config UI for editing client config.toml
- Start/stop the client daemon
- Show connection status

## Pipeline Design

```yaml
# In client config.toml
[pipeline]
enabled = true
max_concurrent = 1
```

Server sends pipeline via control channel:
```json
{
  "type": "run_pipeline",
  "id": "p1",
  "steps": [
    {"name": "install", "command": "npm install", "cwd": "/project", "timeout_secs": 120},
    {"name": "build", "command": "npm run build", "timeout_secs": 300},
    {"name": "serve", "command": "npm start", "timeout_secs": 0}
  ]
}
```

Client executes sequentially via `sh -c` / `cmd /C`, streams output, stops on failure.

## Data Flow

```
┌──────────────────────────────────────────────────────┐
│                   kdct-server                        │
│                                                      │
│  ┌──────────┐   ┌──────────┐   ┌──────────────────┐ │
│  │ TUI Panel│◄──│ rathole  │◄──│ Transport        │ │
│  │ (ratatui)│   │ Server   │   │ (Noise/TLS/TCP)  │ │
│  └──────────┘   │          │   └──────┬───────────┘ │
│                 │ - port   │          │              │
│                 │   fwd    │   ┌──────┴───────────┐ │
│                 │ - client │   │ TCP Listeners    │ │
│                 │   reg    │   │ (:port1, :port2) │ │
│                 └──────────┘   └──────────────────┘ │
└──────────────────────────────────────────────────────┘
                         │
        Control Channel + Data Channels (Noise encrypted)
                         │
┌──────────────────────────────────────────────────────┐
│                   kdct-client                        │
│                                                      │
│  ┌──────────┐   ┌──────────┐   ┌──────────────────┐ │
│  │ Pipeline │◄──│ rathole  │◄──│ Transport        │ │
│  │ Executor │   │ Client   │   │ (Noise/TLS/TCP)  │ │
│  └──────────┘   │          │   └──────────────────┘ │
│                 │ - local  │                         │
│                 │   connect│    Forwarding to:       │
│                 │ - data   │    localhost:3000       │
│                 │   ch fwd │    localhost:8080       │
│                 └──────────┘                         │
└──────────────────────────────────────────────────────┘
```

## Implementation Phases

### Phase 1 — Integrate rathole (visibility + build) ✅ DONE
- [x] Copy rathole into `crates/rathole/`
- [x] Verify it builds with `cargo build`
- [x] Make key types public (Transport, Client, Server, ControlChannelHandle)
- [x] Add to workspace

### Phase 2 — Extend protocol + config
- [ ] Upgrade protocol from fixed-size to length-prefixed encoding
- [ ] Add `ControlChannelCmd::RunPipeline/CancelPipeline/ReportStatus`
- [ ] Add `DataChannelCmd::StartForwardHttp`
- [ ] Add `ServiceType::Http`
- [ ] Wire up encoding/decoding for new message types

### Phase 3 — Pipeline executor
- [ ] Implement `pipeline.rs` in kdct-client
- [ ] Subprocess spawning with output streaming
- [ ] Wire into rathole client's control channel handler
- [ ] Handle `RunPipeline` and `CancelPipeline` messages

### Phase 4 — Server TUI panel
- [ ] ratatui TUI with client list, port mapping table, pipeline output viewer
- [ ] Event bus: server events → TUI (via mpsc channel)
- [ ] Command input: select client → type/load pipeline YAML → send

### Phase 5 — Tauri client panel
- [ ] Svelte form: server address, auth token, port mappings table
- [ ] Start/stop client daemon
- [ ] Save to config.toml

### Phase 6 — HTTP forwarding (optional, can be post-MVP)
- [ ] `StartForwardHttp` data channel command
- [ ] Host/path-based routing on server side
- [ ] HTTP header forwarding

## Verification

1. `cargo build --workspace` — all crates compile
2. Start server with test config → TUI shows up
3. Start client → appears in TUI client list
4. `curl server:2334` → forwarded to client's local service (TCP mode)
5. Send pipeline from TUI → client executes → output streams back
6. Open Tauri panel → edit config → save → client reloads
