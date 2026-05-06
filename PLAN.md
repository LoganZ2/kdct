# KDCT — Docker Container Tunnel (基于 rathole + Pingora)

## 概述

KDCT 是一个 NAT 穿透 + Docker 容器反向代理工具。服务端运行在 VPS 上，通过 Pingora 做 HTTP 反向代理，通过 rathole 隧道将流量转发到 NAT 后客户端的 Docker 容器中。

核心抽象：
- **ClientNode**: 有 Docker 的客户端机器，上报资源状态，执行 Docker 指令
- **ImageNode**: Docker image 的元数据 + 路由配置，绑定到一个 ClientNode 上部署

## 架构

```
┌─ kdct-server (VPS, CLI) ──────────────────────────────────────────┐
│                                                                    │
│  CLI (纯命令行)               Pingora 反向代理 (:80/:443, rustls)   │
│  ├─ image load                │  ProxyHttp::upstream_peer()       │
│  ├─ image config              │  → 查 RouteTable                   │
│  ├─ image deploy              │  → HttpPeer("127.0.0.1:<port>")   │
│  ├─ image stop                                                 │
│  ├─ image list                 RouteTable (Arc<RwLock>)           │
│  ├─ node list                  (path) → localhost:<port>          │
│  └─ node show                                                    │
│                                                                    │
│  SQLite (rusqlite)                                                │
│  image_nodes, client_nodes, routes, deployments, port_allocations │
│                                                                    │
│  Rathole Server (改造后)                                          │
│  ├── PortPool (预绑端口)                                          │
│  ├── NodeRegistry (原 ClientRegistry)                             │
│  └── ControlChannel (Docker 协议)                                 │
└────────────────────────────────────────────────────────────────────┘
        │  TCP 长连接 (心跳 + Docker 指令)
        ▼
┌─ kdct-client (NAT 后, 需 Docker) ─────────────────────────────────┐
│  Rathole Client (改造后)                                          │
│  ├── docker pull / build / run / stop (本地 shell 执行)            │
│  ├── 定期上报容器状态                                             │
│  └── 数据通道 (rathole TCP 隧道)                                  │
│                                                                    │
│  Docker daemon                                                    │
│  └── container:80 ← rathole 隧道 → 服务端:9000 → Pingora          │
└────────────────────────────────────────────────────────────────────┘
```

## 关键设计决策

### 1. Domain 是全局服务端配置

服务端启动时必须配置 `--domain example.com`，不提供则拒绝启动。所有 image 的路由都挂在同一个 domain 下，只区分 path。

```
kdct-server start --config server.toml --domain myapp.example.com
```

路由匹配: `myapp.example.com/` → nginx 容器, `myapp.example.com/api` → api 容器

### 2. Docker 检查 (双端)

服务端和客户端启动时都检查 `docker` 命令是否可用，不可用则直接报错退出。

### 3. TLS 使用 rustls

Pingora 的 TLS 通过 `pingora-rustls` feature 启用，纯 Rust 实现，无需系统库。

### 4. 服务端有 Docker (仅用于 inspect)

服务端需要 Docker 来 pull image → inspect EXPOSE → 清理缓存。Docker 守护进程必须在服务端环境可用。

## 数据流

```
1. Client 连接 → ReportNodeStatus { docker_ver, port_range, cpu, mem, containers }
2. 用户: kdct-server image load nginx:latest
   → 服务端 docker pull → docker inspect → 提取 EXPOSE 端口 → docker rmi 清理
3. 交互: 为每个 EXPOSE 端口填写 path (domain 已全局配置)
   → Port 80/tcp → path: /
4. 用户: kdct-server image deploy my-nginx --to client1
   → 检查 client1 在线 + 空闲端口数 >= 1
   → 检查 path 冲突 (domain:/ 未被占用)
   → 分配 client 端口 3000, server 端口 9000
   → 下发 DockerRun { image, port_map: [(3000, 80)] }
   → 建立 rathole 数据通道: server:9000 ↔ client:3000 ↔ container:80
   → RouteTable 写入: / → 127.0.0.1:9000
5. 外部请求 myapp.example.com/
   → Pingora upstream_peer → RouteTable 匹配 → 127.0.0.1:9000
   → rathole TCP 隧道 → client:3000 → container:80
```

## 项目结构

```
kdct/
├── Cargo.toml
├── PLAN.md
├── crates/
│   ├── rathole/              # Forked rathole (大幅改造)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── protocol.rs   # 新 Docker 协议, 删 pipeline
│   │       ├── client.rs     # Docker 指令处理
│   │       ├── server.rs     # Node 管理 + Docker 指令下发
│   │       ├── transport/    # TCP, TLS, Noise, WebSocket
│   │       ├── config.rs     # 更新配置模型
│   │       ├── port_pool.rs  # 保留 (端口分配仍需要)
│   │       ├── registry.rs   # 改造成 NodeRegistry
│   │       └── ...
│   ├── kdct-server/          # 服务端 (CLI + Pingora)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs       # clap CLI
│   │       ├── proxy.rs      # Pingora 反向代理 + RouteTable
│   │       ├── image.rs      # ImageNode 管理
│   │       ├── node.rs       # ClientNode 管理
│   │       ├── db.rs         # SQLite 持久化
│   │       ├── deploy.rs     # 部署调度 (端口/路由冲突检查)
│   │       └── interactive.rs # 交互式路由配置
│   └── kdct-client/          # 客户端 (CLI + Docker)
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs       # 连接 + 协议处理
│           └── docker.rs     # Docker 命令执行
└── apps/
    └── kdct-panel/           # 系统托盘 (保留并优化)
        ├── Cargo.toml
        └── src/
            ├── main.rs       # 优化后的托盘 UI
            └── editor.rs     # 优化后的配置编辑器
```

## 协议改造 (protocol.rs)

### 删除

- `RunPipeline`, `CancelPipeline`, `PipelineOutput`, `PipelineStep`
- `StartForwardHttp` (Pingora 取代)

### 保留

- `CreateDataChannel`, `HeartBeat`, `PortsAssigned`
- `StartForwardTcp`, `StartForwardUdp`

### 修改

`ReportStatus` → `ReportNodeStatus`:

```rust
ReportNodeStatus {
    hostname: String,
    os: String,
    arch: String,
    docker_version: String,
    port_range_start: u16,
    port_range_end: u16,
    cpu_cores: u32,
    memory_mb: u64,
    running_containers: Vec<ContainerInfo>,
}

ContainerInfo {
    container_name: String,
    image: String,
    ports: Vec<u16>,
    status: String,  // running, stopped, etc.
}
```

### 新增 Docker 指令

```rust
// Server → Client
DockerPull { image: String }
DockerBuild { git_url: String, branch: String, image_tag: String }
DockerRun {
    image_tag: String,
    container_name: String,
    port_map: Vec<(u16, u16)>,  // (host_port, container_port)
    env: HashMap<String, String>,
}
DockerStop { container_name: String }

// Client → Server
DockerPullProgress { image: String, status: String }  // pulling / complete / error
ContainerStarted { container_name: String, ports: Vec<u16> }
ContainerStopped { container_name: String }
ContainerError { container_name: String, error: String }
```

## SQLite Schema

```sql
-- 全局配置
server_config (key TEXT PRIMARY KEY, value TEXT)
  -- domain, port_range_start, port_range_end

-- Docker image 元数据
image_nodes (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    source TEXT NOT NULL,          -- 如 nginx:latest 或 git url
    source_type TEXT NOT NULL,     -- "docker_hub" or "git"
    status TEXT NOT NULL DEFAULT 'loaded',  -- loaded, configured, deployed, error
    created_at INTEGER NOT NULL
)

-- 从 EXPOSE 提取的端口
image_ports (
    id INTEGER PRIMARY KEY,
    image_node_id INTEGER NOT NULL,
    port INTEGER NOT NULL,
    protocol TEXT NOT NULL DEFAULT 'tcp',  -- tcp or udp
    FOREIGN KEY (image_node_id) REFERENCES image_nodes(id)
)

-- 每个 EXPOSE 端口的路由配置
image_routes (
    id INTEGER PRIMARY KEY,
    image_port_id INTEGER NOT NULL UNIQUE,
    path TEXT NOT NULL,            -- 如 /, /api, /ws
    FOREIGN KEY (image_port_id) REFERENCES image_ports(id)
)

-- 客户端节点
client_nodes (
    id INTEGER PRIMARY KEY,
    hostname TEXT NOT NULL,
    os TEXT,
    arch TEXT,
    docker_version TEXT,
    port_range_start INTEGER NOT NULL,
    port_range_end INTEGER NOT NULL,
    cpu_cores INTEGER,
    memory_mb INTEGER,
    status TEXT NOT NULL DEFAULT 'offline',  -- online, offline
    auth_digest TEXT UNIQUE,
    last_seen INTEGER
)

-- 部署关系 (一对一: 一个 image 只能部署到一个 node)
deployments (
    id INTEGER PRIMARY KEY,
    image_node_id INTEGER NOT NULL UNIQUE,
    client_node_id INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'running',  -- running, stopped, error
    deployed_at INTEGER NOT NULL,
    FOREIGN KEY (image_node_id) REFERENCES image_nodes(id),
    FOREIGN KEY (client_node_id) REFERENCES client_nodes(id)
)

-- 端口分配记录
port_allocations (
    id INTEGER PRIMARY KEY,
    deployment_id INTEGER NOT NULL,
    image_port_id INTEGER NOT NULL,
    client_port INTEGER NOT NULL,   -- 客户端 docker 端口
    server_port INTEGER NOT NULL,   -- 服务端 rathole 端口
    FOREIGN KEY (deployment_id) REFERENCES deployments(id),
    FOREIGN KEY (image_port_id) REFERENCES image_ports(id)
)
```

## CLI 命令

```
kdct-server start --config server.toml --domain example.com
  启动服务端 (域名必填, Docker 必检)

kdct-server image load <source>
  source 可以是 docker hub image (nginx:latest) 或 git url
  → 服务端 pull image → inspect EXPOSE → 清理缓存 → 存储元数据

kdct-server image config <name>
  交互式配置每个 EXPOSE 端口的 path 路由

kdct-server image deploy <name> --to <node>
  部署到指定客户端节点 (检查端口/路由冲突)

kdct-server image stop <name>
  停止并清理容器, 回收端口

kdct-server image list
  列出所有 image 及状态

kdct-server node list
  列出所有客户端节点及资源状态

kdct-server node show <name>
  显示节点详情 (运行中的容器、端口使用等)
```

## 交互式路由配置流程

```
$ kdct-server image config my-nginx

  Domain: example.com (全局配置)
  Image: nginx:latest
  Exposed ports: 1

  ── Port 80/tcp ──
  Description: HTTP server
  Path: /

  ✓ Configuration saved. Route: example.com/ → my-nginx:80
```

使用 `dialoguer` crate 做交互式输入，对 path 做正确性校验：
- 必须以 `/` 开头
- 不能重复 (检查已有路由)

## 部署调度逻辑

`kdct-server image deploy <name> --to <node>`:

1. 检查目标 ClientNode 在线
2. 统计该 image 需要的端口数 (image_ports count)
3. 检查 ClientNode 的空闲端口数 >= 需要的端口数
4. 检查所有 image_routes 的 path 不与已有路由冲突
5. 分配 client 端口 (从 node 的 port_range) 和 server 端口 (从 PortPool)
6. 发送 DockerRun 指令到客户端
7. 等待 ContainerStarted 确认
8. 建立 rathole 数据通道
9. 更新 RouteTable (内存) + SQLite

## Pingora 嵌入方式

```rust
// proxy.rs
use pingora::prelude::*;
use pingora_proxy::{ProxyHttp, Session};
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct KdctProxy {
    pub route_table: Arc<RwLock<RouteTable>>,
    pub domain: String,
}

#[async_trait]
impl ProxyHttp for KdctProxy {
    type CTX = ();
    fn new_ctx(&self) -> () { () }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut (),
    ) -> Result<Box<HttpPeer>> {
        let header = session.req_header();
        let host = header.uri.host().unwrap_or("");
        let path = header.uri.path();

        // 只处理本域名
        if host != self.domain {
            return Err(/* 404 */);
        }

        let table = self.route_table.read().await;
        let target = table.resolve(path)?;

        let peer = Box::new(HttpPeer::new(
            ("127.0.0.1", target.port),
            false,  // 本地 rathole 隧道无需 TLS
            self.domain.clone(),
        ));
        Ok(peer)
    }
}
```

```toml
# Cargo.toml for kdct-server
pingora = { version = "0.8", default-features = false, features = ["proxy", "rustls"] }
```

## 客户端掉线 / 服务端重启处理

**客户端掉线:**
- 心跳超时 / TCP 连接断开 → 服务端标记 ClientNode offline
- 关联的所有 deployment → stopped 状态
- RouteTable 移除该节点相关路由

**服务端重启:**
1. SQLite 加载所有持久化数据
2. 客户端重连 (已有指数退避重试)
3. 客户端上报 ReportNodeStatus (含 running_containers)
4. 服务端对比 SQLite 记录，确认哪些容器仍在运行
5. 重新分配 server 端口 (PortPool 刷新后可能不同)
6. 重建 RouteTable

## 需删除的代码

| 文件 | 说明 |
|------|------|
| `crates/kdct-server/src/web.rs` | HTML 控制台 + axum |
| `crates/kdct-server/src/admin.rs` | TCP admin API |
| `crates/rathole/src/pipeline.rs` | Pipeline 执行器 |
| `tests/pipeline.json` | 测试文件 |
| 协议中的 Pipeline 相关 | RunPipeline, CancelPipeline, PipelineOutput, StartForwardHttp |
| 配置中的 web_port | ServerConfig |
| axum, tower-http, serde_yaml 依赖 | kdct-server Cargo.toml |

## 需新增的代码

| 文件 | 依赖 |
|------|------|
| `proxy.rs` | pingora (proxy + rustls) |
| `image.rs` | - |
| `node.rs` | - |
| `db.rs` | rusqlite |
| `deploy.rs` | - |
| `interactive.rs` | dialoguer |
| `docker.rs` (client) | - |

## kdct-panel 优化

保留系统托盘应用，优化方向：
- 现代化 UI 风格 (不再用原生 egui 默认皮肤)
- 专门的 GUI 工具包考虑用 egui v0.31 的自定义 theme
- 增加 Docker 容器状态展示
- 连接状态实时更新
- 配置编辑器改为内嵌表单 (而非打开外部编辑器)

## 实现阶段

| Phase | 内容 |
|-------|------|
| **1** | 删除旧代码 (web.rs, admin.rs, pipeline.rs, 协议清理) |
| **2** | 改造 protocol.rs: 新增 Docker 协议, 删 pipeline 协议 |
| **3** | 改造 client: docker.rs (Docker 命令执行) + 新协议处理 |
| **4** | 改造 server: NodeRegistry 改造 + PortPool 适配 |
| **5** | 新增 db.rs (SQLite schema + CRUD) |
| **6** | 新增 proxy.rs (Pingora + RouteTable) |
| **7** | 新增 image.rs + deploy.rs + interactive.rs |
| **8** | 新增 CLI (main.rs 重写) |
| **9** | 断连恢复 / 状态同步 |
| **10** | 优化 kdct-panel UI |
