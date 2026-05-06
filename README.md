<p align="center">
  <img src="https://img.shields.io/badge/rust-1.75+-orange.svg" alt="Rust 1.75+">
  <img src="https://img.shields.io/badge/tls-rustls-green.svg" alt="TLS: rustls">
  <img src="https://img.shields.io/badge/proxy-pingora-blue.svg" alt="Proxy: Pingora">
  <img src="https://img.shields.io/badge/license-Apache--2.0-red.svg" alt="Apache 2.0">
</p>

# KDCT — Docker Container Tunnel

Deploy Docker containers behind NAT to the public internet through a cheap VPS. Think `ngrok` + `fly.io`, but you own the infrastructure.

Under the hood: an encrypted TCP tunnel (rathole fork) + HTTP reverse proxy (Pingora) + dynamic Docker orchestration.

---

## Architecture

```
  Internet                     VPS ($5/mo)                     Your Machine (NAT)
  ─────────                    ──────────                      ──────────────────

  https://app.example.com  →  kdcts (server)  ←── tunnel ──→  kdctc (client)
                              │                                │
                              Pingora proxy                    docker run
                              /  → 127.0.0.1:9000              nginx:80
                              /api → 127.0.0.1:9001            api:3000
```

---

## Quick Start

### 1. Server (VPS)

```bash
# server.toml
[server]
bind_addr = "0.0.0.0:2333"
default_token = "your-secret"
port_pool = "9000-9999"
domain = "myapp.example.com"
http_port = 80

[server.transport]
type = "tcp"
```

```bash
./kdcts start
```

### 2. Client (your machine behind NAT)

```bash
# client.toml
[client]
remote_addr = "VPS_IP:2333"
default_token = "your-secret"

[client.transport]
type = "tcp"
```

```bash
./kdctc --config client.toml
```

### 3. Deploy

```bash
# Load image (auto-triggers interactive route config)
./kdcts image load nginx:latest
  → Port 80/tcp → Route path: /

# Check nodes
./kdcts node list
  ID   HOSTNAME             STATUS   DOCKER
  1    my-laptop.local      online   Docker version 29.x

# Deploy
./kdcts image deploy nginx:latest --to 1

# Visit
curl https://myapp.example.com/
```

---

## CLI Reference

```
kdcts start                          Start server daemon

kdcts image load <source>            Load image, auto-trigger route config
kdcts image config <name> [-p HOST:CONTAINER] [-e KEY=VALUE]
                                      Add port mapping / env vars, 
                                      auto-trigger route for new ports
kdcts image deploy <name> --to <id>  Deploy to node
kdcts image stop <name>              Stop and release
kdcts image list                     List images
kdcts image show <name>              Image details

kdcts node list                      List nodes
kdcts node show <id>                 Node details

kdctc --config client.toml            Connect to server
```

---

## Config Reference

### server.toml

```toml
[server]
bind_addr = "0.0.0.0:2333"       # Rathole listen address
default_token = "your-secret"     # Auth token
port_pool = "9000-9999"           # Tunnel port range
domain = "example.com"            # REQUIRED
http_port = 80                    # Reverse proxy port (default 80)
https_port = 0                    # 0 = disabled

[server.transport]
type = "tcp"
```

### client.toml

```toml
[client]
remote_addr = "VPS_IP:2333"
default_token = "your-secret"

[client.transport]
type = "tcp"
```

---

## How It Works

1. Client connects → reports hostname, Docker version, port range, CPU, memory → stored in SQLite
2. `image load` → server pulls image, inspects EXPOSE, asks for route path per port interactively
3. `image deploy` → checks node online, ports available, no route conflict → sends DockerRun via tunnel
4. Client `docker run` the container → server assigns tunnel ports → RouteTable updated
5. External request → Pingora resolves path → tunnels to container through NAT
6. Server restart → SQLite rebuilds RouteTable, clients reconnect with backoff

---

## Build From Source

```bash
git clone https://github.com/LoganZ2/kdct.git
cd kdct

# macOS: brew install cmake
cargo build --release --workspace

./target/release/kdcts --help
./target/release/kdctc --help
```

---

## Why Not X

| Alternative | Why KDCT instead |
|-------------|-----------------|
| ngrok / Cloudflare Tunnel | Metered, you don't own the infra |
| frp / rathole alone | Static config per port, no Docker orchestration |
| fly.io / Railway | Per-container pricing, no local-first workflow |
| k3s / Kubernetes | Massive overhead for simple deploys |

---

## License

Apache 2.0. Built on [rathole](https://github.com/rapiz1/rathole) and [Pingora](https://github.com/cloudflare/pingora).
