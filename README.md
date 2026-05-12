<p align="center">
  <img src="https://img.shields.io/badge/rust-1.85+-orange.svg" alt="Rust 1.85+">
  <img src="https://img.shields.io/badge/proxy-pingora-blue.svg" alt="Pingora">
  <img src="https://img.shields.io/badge/ui-svelte_5-ff3e00.svg" alt="Svelte 5">
  <img src="https://img.shields.io/badge/license-Apache--2.0-red.svg" alt="Apache 2.0">
</p>

# KDCT

**Run Docker containers behind NAT, serve them on a public domain.**

`ngrok` and `fly.io` rolled into one self-hosted binary pair. Drop `kdcts` on a $5 VPS, run `kdctc` on whatever box you want to expose, and use the web panel to deploy containers under any path on your domain.

```
                                                ┌── home / lab / laptop ──┐
  https://app.example.com  ─┐                   │  docker run nginx:80    │
  https://app.example.com/api ─┐                │  docker run api:3000    │
                              │                 │                         │
                              ▼                 │  kdctc                  │
                          ┌──────┐  TCP tunnel  │   ↑ NAT-pierced         │
                          │ kdcts│ ◄──────────► │                         │
                          └──────┘              └─────────────────────────┘
                          VPS, public IP
```

## Features

- **One panel, all of it.** Pull an image, define a port (route via path or direct TCP/UDP), pick a node. No YAML, no kubectl.
- **Many services on one domain.** `/`, `/api`, `/grafana`, … each routed to a different container, possibly on a different machine.
- **Docker Hub or Git.** Point at `nginx:latest`, or at a repo with a `Dockerfile` — KDCT builds it on the client.
- **Built-in TLS.** Bring your own cert, flip the toggle, restart.
- **Tiny.** Two Rust binaries (`kdcts`, `kdctc`) and a static SvelteKit panel. SQLite for state.

## Quick start

### 1. Server — on a VPS with a public IP

```toml
# server.toml
[server]
bind_addr     = "0.0.0.0:2333"           # tunnel listener
default_token = "change-me"
port_pool     = "9000-9999"              # tunnel ports, pre-bound at start
domain        = "app.example.com"        # optional — leave unset for IP-only setups

http_port  = 80                          # used when TLS is OFF
https_port = 443                         # used when TLS is ON
api_port   = 9933                        # panel + REST API, 127.0.0.1 only

# tls_cert_path = "/etc/kdct/cert.pem"   # uncomment to enable TLS toggle
# tls_key_path  = "/etc/kdct/key.pem"

# admin_user     = "admin"                # optional basic auth for /admin
# admin_password = "s3cret"
```

```bash
./kdcts --config server.toml
```

Docker must be on the box. The panel lives at `https://app.example.com/admin/`.

### 2. Client — on the machine behind NAT

```toml
# client.toml
[client]
remote_addr   = "VPS_IP:2333"
default_token = "change-me"

[client.services.my-node]
type             = "tcp"
local_addr       = "127.0.0.1:3000"
port_range_start = 3000                  # required
port_range_end   = 3999                  # required
# image_cache_ttl_seconds = 300          # default 5 min
```

```bash
./kdctc --config client.toml
```

### 3. Deploy in the panel

Open `https://app.example.com/admin/`:

1. **Load image** — `nginx:latest`, or a Git URL pointing at a repo with a `Dockerfile`.
2. **Create a bridge** — add a port (container `80` → route `/` or direct `:9000`), set protocols (tcp/udp).
3. **Create a connection** — pick the image, the bridge, and an online node.

KDCT pulls or builds, runs the container on the client, wires the path into the route table, and serves traffic. Visit `https://app.example.com/`.

## Concepts

KDCT splits a deployment into three reusable pieces, joined by a connection:

| Piece          | What it is                                                                 |
| -------------- | -------------------------------------------------------------------------- |
| **Image**      | A Docker Hub tag (`nginx:latest`) or a Git URL                             |
| **Bridge**     | Port + env template: container port → route path or direct TCP/UDP, env vars |
| **Node**       | A connected `kdctc` client (hostname, OS, Docker version, port range, …)   |
| **Connection** | `image × bridge × node` — once all three are picked, KDCT auto-deploys     |

## How it works

1. On startup `kdcts` pre-binds every port in `port_pool`. If anything is in use the server fails fast.
2. When you add a port to a bridge, one pool port is **reserved up front** and stored with the bridge — same port across deploys, releases on delete.
3. Once a connection has all three slots filled and its node is online, the server sends one `ImageStart` over the control channel. The client decides whether to skip the pull, `docker pull`, or `git clone` + `docker build`, then `docker run`s.
4. Pingora's `upstream_peer` does longest-prefix matching on the route table and forwards through the tunnel to the client.
5. On disconnect the routes drop but the bridge keeps its pool ports; reconnect and the auto-check loop redeploys. The client keeps containers and images warm for `image_cache_ttl_seconds`.

## Domain

`domain` is optional.

- **Set:** the proxy enforces it on the `Host` header — anything else gets a 404, same as nginx with `server_name`. Required for TLS.
- **Unset:** the proxy accepts any `Host`, so you can hit `kdcts` directly by public IP — same behavior as nginx without a `server_name`. The panel lives at `http://<your-ip>/admin/`. TLS is locked off in this mode.

## TLS

Two ways to get a cert:

**1. Auto (Let's Encrypt).** Add `[server.acme]`:

```toml
[server.acme]
enabled = true
email   = "you@example.com"
# staging = true               # use LE staging while testing
# state_dir = "kdct-state/acme/your.domain"
```

`kdcts` issues a cert via HTTP-01 on startup, persists it under `state_dir`, flips the TLS toggle on, and renews automatically when fewer than 30 days remain (Pingora picks the new cert up on next restart). `domain` is required, and `http_port` must be reachable from the public internet for the challenge.

**2. Manual.** Set `tls_cert_path` and `tls_key_path` in `[server]`. Flip the TLS toggle in the panel. Restart `kdcts`.

Either way, when TLS is on `kdcts` binds HTTPS on `https_port`, the panel TLS toggle becomes available, and `http_port` is left for `acme` renewals (and a future HTTP→HTTPS redirector). Pingora itself only ever speaks one of the two.

## Reserved paths

- `/admin` and `/admin/*` belong to the panel. Bridges can't claim them.
- `/` is fair game.

## Build from source

```bash
git clone https://github.com/LoganZ2/kdct.git
cd kdct

# Pingora needs cmake.
#   apt install cmake build-essential   # Debian/Ubuntu
#   brew install cmake                  # macOS

# Panel
cd apps/kdct-panel && npm install && npm run build && cd ../..

# Binaries
cargo build --release --workspace

./target/release/kdcts --config server.toml
./target/release/kdctc --config client.toml
```

At runtime `kdcts` looks for the panel in this order:

1. `$KDCT_PANEL_DIR` (env var, if set and the path exists)
2. `<directory of kdcts>/panel/` (default — release tarballs ship this layout)
3. `apps/kdct-panel/build/` relative to the crate at compile time (for `cargo run` during development)

## Releases

Pre-built tarballs for `linux-x86_64`, `linux-aarch64`, and `macos-universal` are published on the [GitHub Releases page](https://github.com/LoganZ2/kdct/releases). Each tarball includes both binaries, the panel build, sample configs, and a `docs/` directory with `LICENSE`, `NOTICE`, `README.md`, and `THIRD_PARTY_LICENSES.html`.

Windows is not supported — Pingora upstream doesn't target it.

## Layout

```
kdct/
├── crates/
│   ├── tunnel/    TCP tunnel + control protocol (rathole-derived)
│   ├── kdcts/     server: API, proxy, deploy, SQLite
│   └── kdctc/     client: Docker driver, image cache
└── apps/
    └── kdct-panel/   Svelte 5 + SvelteKit static
```

## Caveats

- **Panel has optional basic auth.** Set `admin_user` and `admin_password` in `server.toml` to protect `/admin/` with HTTP Basic Authentication. Not required, but recommended for production.
- **Hostname collision.** Nodes are keyed by SHA-256 auth digest, but the registry also indexes by hostname. Two clients sharing a hostname currently collide.
- **No HTTP→HTTPS redirect.** TLS-on means HTTP is gone; if you want both, terminate elsewhere.

## License

Apache-2.0. Built on [rathole](https://github.com/rapiz1/rathole) and [Pingora](https://github.com/cloudflare/pingora).
