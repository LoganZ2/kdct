<p align="center">
  <img src="https://img.shields.io/badge/rust-1.75+-orange.svg" alt="Rust 1.75+">
  <img src="https://img.shields.io/badge/proxy-pingora-blue.svg" alt="Proxy: Pingora">
  <img src="https://img.shields.io/badge/ui-svelte_5-ff3e00.svg" alt="UI: Svelte 5">
  <img src="https://img.shields.io/badge/license-Apache--2.0-red.svg" alt="Apache 2.0">
</p>

# KDCT — Docker Container Tunnel

Deploy Docker containers behind NAT to the public internet through a cheap VPS. Think `ngrok` + `fly.io`, but you own the infrastructure.

Under the hood: a custom TCP tunnel (rathole-derived) + HTTP reverse proxy (Pingora) + dynamic Docker orchestration, all driven by a Svelte web panel.

---

## Architecture

```
  Internet                    VPS  (kdcts)                       Your Machine  (kdctc, NAT)
  ─────────                   ──────────────                     ────────────────────────────

  https://app.example.com  →  Pingora :80 OR :443  ──┐           ┌── docker run ──┐
                              (HTTP xor HTTPS,       │           │  nginx:80       │
                               toggled in panel)     │           │  api:3000       │
                              RouteTable             │           │                 │
                              /admin   → 127.0.0.1:api_port      │  reports node   │
                              /api/*   → bridge ports ←── encrypted TCP ──┤  status, runs   │
                                                     │           │  Docker cmds    │
                              tunnel server          │           └─────────────────┘
                              (port pool 9000-9999)  │
                                                     │
                              tiny_http :api_port (default 9933, 127.0.0.1)
                              REST API + Svelte panel  ←─ also exposed at /admin
                              SQLite (kdct.db, server_config)
```

The web panel (`apps/kdct-panel`) is the primary UI: load images, configure port→path bridges, wire up connections, watch nodes, toggle TLS — no CLI subcommands beyond `kdcts` / `kdctc` themselves. The panel is reachable two ways:

- **Public**: `http(s)://<domain>/admin/` — forwarded by Pingora to the internal API port.
- **Local**: `http://127.0.0.1:<api_port>/admin/` — bound to loopback only.

The `/admin` path is reserved — bridges cannot use it as a route path. The bare `/` on the public domain is free for bridges to claim.

---

## Concepts

KDCT splits a deployment into three editable pieces, joined by a **connection**:

| Piece          | What it is                                                                  |
|----------------|-----------------------------------------------------------------------------|
| **Image**      | A Docker Hub image (`nginx:latest`) or a Git repo with a `Dockerfile`       |
| **Bridge**     | Reusable port + env template — for each container port, route path / protocols / env vars |
| **Node**       | A connected `kdctc` client — hostname, OS, Docker version, port range, CPU/mem |
| **Connection** | `image × bridge × node` — once all three are set, KDCT auto-deploys         |

A connection is "deployable" when it has a bridge, an image, and an online node assigned. The server polls every 5s and brings any pending-but-ready connection up.

---

## Quick Start

### 1. Server (VPS)

```toml
# server.toml
[server]
bind_addr = "0.0.0.0:2333"
default_token = "your-secret"
port_pool = "9000-9999"
domain = "myapp.example.com"

# Pingora binds exactly one of these at a time (HTTP xor HTTPS), picked by
# the TLS toggle in the admin panel.
http_port = 80
https_port = 443

# Internal panel/API port. Bound to 127.0.0.1 only. Pingora forwards
# <domain>/admin/* here.
api_port = 9933

# Required to enable TLS in the panel toggle. Comment out for HTTP-only.
# tls_cert_path = "/etc/kdct/cert.pem"
# tls_key_path  = "/etc/kdct/key.pem"

[server.transport]
type = "tcp"
```

```bash
./kdcts --config server.toml
```

The server binds three listeners:
- `:2333` — tunnel control + data channels (clients connect here)
- `:80` **or** `:443` — Pingora reverse proxy (public traffic, HTTP xor HTTPS)
- `127.0.0.1:9933` — REST API + Svelte panel (also reachable as `<domain>/admin`)

Docker must be installed on the server (used for the panel; the daemon is checked at startup).

### 2. Client (your machine behind NAT)

```toml
# client.toml
[client]
remote_addr = "VPS_IP:2333"
default_token = "your-secret"

[client.services.my-node]
type = "tcp"
local_addr = "127.0.0.1:3000"
port_range_start = 3000           # required, no default
port_range_end = 3999             # required, no default
# image_cache_ttl_seconds = 300   # optional, default 5 min

[client.transport]
type = "tcp"
```

```bash
./kdctc --config client.toml
```

The client reports hostname, OS, arch, Docker version, port range, CPU cores, memory. Docker must be installed. On disconnect, the client keeps containers and pulled images around for `image_cache_ttl_seconds` so a quick reconnect can reuse them — only after the TTL elapses are the containers stopped and the images pruned.

### 3. Deploy via the panel

Open `http://<domain>/admin/` (public) or `http://VPS_IP:9933` (SSH-tunnel for safety) and:

1. **Load an image** — `nginx:latest`, or a Git URL pointing at a repo with a `Dockerfile`
2. **Create a bridge** — add ports (e.g. `80` → route mode, path `/`) and any env vars. `/admin` is reserved.
3. **Create a connection** — pick the image, bridge, and target node

KDCT pulls/builds, picks a tunnel port from the pool, runs the container, wires the path into the route table, and serves `http(s)://myapp.example.com/`.

### 4. Toggling TLS

In the panel's **⚙ Settings** modal, flip the **TLS / HTTPS** switch. The setting is persisted in SQLite and applied on the next `kdcts` restart — the modal shows a *restart required* banner until the live mode matches. The toggle is disabled if `tls_cert_path` / `tls_key_path` are not set in `server.toml` or point to non-existent files.

---

## CLI Reference

KDCT is panel-driven. The two binaries are daemons:

```
kdcts --config server.toml      Start the server (tunnel + proxy + panel API)
kdctc --config client.toml      Connect a node to the server
```

All deploy/stop/configure operations happen through the web panel (or directly against the REST API on `127.0.0.1:9933`).

---

## REST API (selected endpoints)

The panel talks to the API on `127.0.0.1:<api_port>` directly, or via the public proxy at `<domain>/admin/api/...` (the API server transparently strips the `/admin` prefix).

```
GET    /api/overview              counts, online nodes, container count, pool free
GET    /api/nodes                 list connected client nodes
GET    /api/images                loaded images
POST   /api/image/load            { source, name? }  →  job_id
GET    /api/image/load/progress?job=<id>

GET    /api/bridges               list port/env templates
POST   /api/bridges               { name }
GET    /api/bridges/{id}          bridge with ports + envs
DELETE /api/bridges/{id}
POST   /api/bridges/{id}/port     { container_port, mode, route_path?, protocols? }
                                  route_path under /admin is rejected
POST   /api/bridges/{id}/env      { envs: [{ key, value }, ...] }

GET    /api/connections           list connections (joined with bridge/image/node names)
POST   /api/connections           { name, bridge_id?, image_id?, node_id? }  → auto-deploys
PATCH  /api/connections/{id}      change bridge/image/node (null clears) → auto-deploys
DELETE /api/connections/{id}      stops + removes
POST   /api/auto-check            kick the auto-deploy loop manually

GET    /api/settings              { tls_enabled, live_tls_enabled, tls_configurable,
                                    restart_required, http_port, https_port, api_port }
POST   /api/settings              { tls_enabled: bool }  persists; restart to apply

GET    /api/search?q=<query>      Docker Hub repo search
GET    /api/tags?repo=<repo>      Docker Hub tags for a repo
```

`PATCH /api/connections/{id}` uses tri-state slots: omit a field to leave it, send `null` to clear it, send a number to set it.

---

## Config Reference

### server.toml

```toml
[server]
bind_addr = "0.0.0.0:2333"        # tunnel listen address
default_token = "your-secret"     # SHA-256 token + nonce auth
port_pool = "9000-9999"           # tunnel port range (server-side endpoints)
domain = "example.com"            # REQUIRED — Pingora rejects other hosts with 404

# Pingora binds exactly one of these at a time, picked by the panel TLS toggle.
http_port = 80                    # used when TLS is OFF
https_port = 443                  # used when TLS is ON

api_port = 9933                   # internal panel + REST API (127.0.0.1 only)

# Required if you intend to enable TLS in the panel toggle.
tls_cert_path = "/etc/kdct/cert.pem"
tls_key_path  = "/etc/kdct/key.pem"

[server.transport]
type = "tcp"
```

### client.toml

```toml
[client]
remote_addr = "VPS_IP:2333"
default_token = "your-secret"

[client.services.<node-name>]
type = "tcp"
local_addr = "127.0.0.1:3000"     # legacy field, unused for Docker flow
port_range_start = 3000           # client-side host ports for `docker run -p`
port_range_end = 3999

[client.transport]
type = "tcp"
```

---

## How It Works

1. Server starts → port pool is **pre-bound** (every port in `port_pool` is `bind()`ed up front; startup fails fast if any is taken)
2. Client connects → reports `ReportNodeStatus { hostname, os, arch, docker_version, port_range, cpu, mem, running_containers }` → upserted into SQLite (`client_nodes`)
3. User loads an image via the panel — server records it (Git sources are shallow-cloned to verify a `Dockerfile` exists; the actual `docker build` runs on the client at deploy time)
4. User creates a bridge and adds ports — for **each** port, the server immediately reserves one `pool_port` from the port pool and stores it in `bridge_ports.pool_port`. Deleting a port (or the whole bridge) releases the reservation back to the pool.
5. User creates a connection (image + bridge + node). Server verifies the node is online, the bridge has pre-allocated pool ports, and no route paths conflict
6. Server sends a single `ImageStart { image_tag, source, source_type, container_name, port_map, env }` over the control channel. The **client** decides what to do: if the image is already present locally it skips the pull/build; otherwise it `docker pull`s (Docker Hub source) or `git clone` + `docker build`s (Git source), then `docker run`s. On success the client replies with `ContainerStarted { ports }`.
7. Server registers each `route` port in `RouteTable` (`/api` → `127.0.0.1:9001`) and spawns an accept loop on the pre-allocated pool port
8. External request → Pingora `upstream_peer` resolves longest-prefix match → forwards to `127.0.0.1:<pool_port>` → tunnel data channel → client `127.0.0.1:<client_port>` → container

`ImageStop` is the symmetric teardown — the server sends one command, the client `docker stop`s + `docker rm`s and replies with `ContainerStopped`.

When a node disconnects the server marks its connections `pending`, removes the routes, and tears down the accept loops. The bridge's pool ports are **kept reserved** (they belong to the bridge config, not the deployment), so when the node reconnects the auto-check loop redeploys the same connection with the same ports. On the client side, containers and pulled images are kept for `image_cache_ttl_seconds` so a quick reconnect skips the pull/build entirely.

On server restart, all nodes are marked offline; clients reconnect with backoff and re-register, and the auto-check loop redeploys ready connections.

---

## Build From Source

```bash
git clone https://github.com/LoganZ2/kdct.git
cd kdct

# Pingora needs cmake. On Debian/Ubuntu:
#   sudo apt install cmake build-essential
# On macOS:
#   brew install cmake

# Build the panel (Svelte 5 / SvelteKit static)
cd apps/kdct-panel && npm install && npm run build && cd ../..

# Build the binaries
cargo build --release --workspace

./target/release/kdcts --help
./target/release/kdctc --help
```

The `kdcts` binary expects the panel build output at `../../apps/kdct-panel/build` relative to its crate (resolved at compile time via `CARGO_MANIFEST_DIR`).

---

## Workspace Layout

```
kdct/
├── crates/
│   ├── tunnel/               TCP tunnel + Docker control protocol
│   │   └── src/{client,server,protocol,registry,port_pool,transport}.rs
│   ├── kdcts/                server binary (kdcts)
│   │   └── src/{main,api,db,deploy,deployment_tracker,image,proxy}.rs
│   └── kdctc/                client binary (kdctc)
│       └── src/{main,docker}.rs
└── apps/
    └── kdct-panel/           Svelte 5 + Vite + SvelteKit static panel
```

---

## Status / Caveats

- **TLS**: built-in via `rustls`. User-provided cert + key paths only — no ACME / Let's Encrypt automation. Toggle with the panel; restart `kdcts` to apply.
- **HTTP ↔ HTTPS is exclusive**: the proxy binds either `http_port` or `https_port`, never both. There's no automatic HTTP→HTTPS redirect — front with Caddy/nginx if you want one.
- **`/admin` is reserved on the public domain**: bridges cannot use a route path of `/admin` or anything under `/admin/`. The bare `/` is fine — it's free for bridges.
- **SPA fallback is scoped to `/admin`**: unmatched paths under `/admin/*` fall through to the panel's `index.html`; unmatched paths anywhere else return a normal 404 (so a misconfigured bridge route doesn't accidentally serve the panel HTML).
- **Panel API binds to `127.0.0.1:<api_port>`**: also reachable via the proxy at `<domain>/admin/`. It has no built-in auth, so if you don't want anyone with the domain hitting it, front the proxy with basic auth or a VPN.
- **Image cache on the client**: containers and pulled images are kept for `image_cache_ttl_seconds` (default 5 min) after a disconnect to make quick reconnects cheap. Tune it down for memory-constrained nodes.
- **Per-client identity**: nodes are keyed by `auth_digest`, but `client_nodes.hostname` is also used as a registry key. Two clients sharing a hostname will currently collide.

---

## Why Not X

| Alternative                | Why KDCT instead                                                          |
|----------------------------|---------------------------------------------------------------------------|
| ngrok / Cloudflare Tunnel  | Metered, you don't own the infra                                          |
| frp / rathole alone        | Static config per port, no Docker orchestration                           |
| fly.io / Railway           | Per-container pricing, no local-first workflow                            |
| k3s / Kubernetes           | Massive overhead for simple deploys                                       |

---

## License

Apache 2.0. Built on [rathole](https://github.com/rapiz1/rathole) and [Pingora](https://github.com/cloudflare/pingora).
