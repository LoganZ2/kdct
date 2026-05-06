# KDCT — Tunnel + Pipeline Tool (based on rathole)

## Context

Build a frp-like tool: a **cloud server** with built-in reverse proxy that exposes local services to the public internet through NAT traversal, plus **remote pipeline execution** (cloud server can send CI commands to local clients). Server is CLI-only (no TUI).

Architecture:
- **kdct-server** runs on a cloud VPS with a public IP — accepts connections from clients, acts as reverse proxy for their services, and can trigger remote pipelines
- **kdct-client** runs on local machines behind NAT — maintains a persistent encrypted tunnel to the server, registers port mappings (e.g., `localhost:3000` → `cloud:8080`), and executes CI pipelines on demand

We use **rathole** as the tunnel foundation — it already has TCP/UDP forwarding, Noise/TLS encryption, WebSocket transport, and a clean Transport trait. We extend it.

## Why rathole

rathole (`crates/rathole/`) is a pure-Rust frp alternative:

| Component | File | What it does |
|-----------|------|-------------|
| `Transport` trait | `src/transport/mod.rs:52` | Abstraction over TCP/TLS/Noise/WS |
| `Client<T>` | `src/client.rs:84` | Client state machine (connects, handles data channels) |
| `Server<T>` | `src/server.rs:96` | Server state machine (accepts, manages port forwarding) |
| Protocol | `src/protocol.rs` | Binary bincode protocol (Hello/Auth/ControlChannelCmd/DataChannelCmd) |
| Config | `src/config.rs:234` | TOML config with services, transport, tokens |

**Key insight**: rathole's `Transport` trait means we can tunnel ANY stream — TCP, UDP, and later HTTP — through the same encrypted channel. No web framework lock-in. Extending it is just adding new protocol commands on top.

## Integration Strategy

**Fork and extend** — bring rathole into our workspace as `crates/rathole/`, make internal types public, add our features on top.

```
/Users/zhuangkaiyi/kdct/
├── Cargo.toml                          # Workspace root
├── crates/
│   ├── rathole/                        # Forked rathole (extended)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs                  # NOW PUB: all modules
│   │       ├── transport/              # TCP, TLS, Noise, WebSocket (unchanged)
│   │       ├── client.rs              # Client<T> (extended with pipeline dispatch)
│   │       ├── server.rs              # Server<T> (extended with status events)
│   │       ├── protocol.rs            # Extended: PipelineCmd, StatusReport (length-prefixed)
│   │       └── config.rs              # Extended: pipeline config section
│   ├── kdct-server/                    # Server binary — CLI only
│   │   └── src/
│   │       ├── main.rs                 # Parse args, init rathole server, CLI commands
│   │       └── commands.rs            # CLI: list clients, send pipeline, etc.
│   └── kdct-client/                    # Client binary
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

### 1. Make types public (visibility changes only) DONE

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
    PipelineOutput { id: PipelineId, step: String, stdout: Vec<u8>, stderr: Vec<u8>, exit_code: Option<i32> },
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
- Streams stdout/stderr back via `PipelineOutput` control channel messages
- Reports step results (exit code)

## What We Build New

### kdct-server — Server binary (CLI only)

- Starts rathole server with the provided config
- CLI commands via subcommands:
  - `kdct-server start --config server.toml` — start the server
  - `kdct-server list` — list connected clients
  - `kdct-server pipeline send --client <name> --file pipeline.yaml` — send pipeline to a client
  - `kdct-server pipeline status --client <name> --id <pipeline_id>` — check pipeline status
- Reverse proxy is handled by rathole Server<T> natively — it already binds ports, accepts visitor connections, and forwards through data channels to clients

### kdct-client — Client binary

- Starts rathole client with the provided config
- Pipeline executor hooks into the client's control channel
- On startup: reports hostname, OS, arch via `ReportStatus`
- Sits in the background, maintains tunnel, waits for pipelines

### kdct-panel — Tauri app (client-side GUI)

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
```yaml
id: "p1"
steps:
  - name: "install"
    command: "npm install"
    cwd: "/project"
    timeout_secs: 120
  - name: "build"
    command: "npm run build"
    timeout_secs: 300
  - name: "serve"
    command: "npm start"
    timeout_secs: 0
```

Client executes sequentially via `sh -c` (Unix) / `cmd /C` (Windows), streams stdout/stderr back, stops on first failure. Timeout 0 means no timeout.

## Data Flow

```
┌──────────────────────────────────────────────────────┐
│            kdct-server (cloud VPS, public IP)        │
│                                                      │
│  ┌──────────┐   ┌──────────┐   ┌──────────────────┐ │
│  │ CLI      │──│ rathole  │◄──│ Transport        │ │
│  │ commands │   │ Server   │   │ (Noise/TLS/TCP)  │ │
│  └──────────┘   │          │   └──────┬───────────┘ │
│                 │ - port   │          │              │
│                 │   fwd    │   ┌──────┴───────────┐ │
│                 │ - client │   │ TCP Listeners    │ │
│                 │   reg    │   │ (:8080, :9090)   │ │
│                 │ - pipe   │   └──────────────────┘ │
│                 │   ctl    │                         │
│                 └──────────┘                         │
└──────────────────────────────────────────────────────┘
                         │
        Control Channel + Data Channels (Noise encrypted)
                         │
┌──────────────────────────────────────────────────────┐
│            kdct-client (local machine, behind NAT)    │
│                                                      │
│  ┌──────────┐   ┌──────────┐   ┌──────────────────┐ │
│  │ Pipeline │◄──│ rathole  │◄──│ Transport        │ │
│  │ Executor │   │ Client   │   │ (Noise/TLS/TCP)  │ │
│  │          │   │          │   └──────────────────┘ │
│  │ npm inst │   │ - data   │                         │
│  │ npm run  │   │   ch fwd │    Forwarding from:     │
│  │ npm start│   │ - status │    localhost:3000       │
│  └──────────┘   │   report │    localhost:8080       │
│                 └──────────┘                         │
└──────────────────────────────────────────────────────┘
```

## Implementation Phases

### Phase 1 — Integrate rathole (visibility + build) DONE
- [x] Copy rathole into `crates/rathole/`
- [x] Verify it builds with `cargo build`
- [x] Make key types public (Transport, Client, Server, ControlChannelHandle)
- [x] Add to workspace

### Phase 2 — Extend protocol + config ✅ DONE
- [x] Upgrade protocol from fixed-size to length-prefixed encoding
- [x] Add `ControlChannelCmd::RunPipeline/CancelPipeline/PipelineOutput/ReportStatus`
- [x] Add `DataChannelCmd::StartForwardHttp`
- [x] Add `ServiceType::Http`
- [x] Wire up encoding/decoding for new message types

### Phase 3 — Pipeline executor (client side)
- [ ] Implement `pipeline.rs` in kdct-client
- [ ] Subprocess spawning with output streaming via control channel
- [ ] Wire into rathole client's control channel message loop
- [ ] Handle `RunPipeline`, `CancelPipeline` messages

### Phase 4 — Server CLI
- [ ] CLI arg parsing with clap: `start`, `list`, `pipeline send`, `pipeline status`
- [ ] Client registry: track connected clients, their services, and pipeline state
- [ ] Pipeline dispatch: send YAML to a client via control channel, receive output

### Phase 5 — Tauri client panel
- [ ] Config editor form (server address, auth token, port mappings)
- [ ] Start/stop client daemon
- [ ] Connection status indicator

### Phase 6 — HTTP reverse proxy (post-MVP if needed)
- [ ] `StartForwardHttp` data channel command
- [ ] Host/path-based routing on server side
- [ ] HTTP header forwarding

## Verification

1. `cargo build --workspace` — all crates compile
2. Start server: `kdct-server start --config server.toml`
3. Start client: `kdct-client start --config client.toml`
4. Server shows client in `kdct-server list`
5. `curl cloud-vps:8080` → forwarded to client's `localhost:3000`
6. `kdct-server pipeline send --client my-machine --file pipeline.yaml` → client executes → output streams back
