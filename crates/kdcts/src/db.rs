use anyhow::{Context, Result};
use rusqlite::{Connection as SqlConnection, params};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::info;

pub struct Database {
    conn: Mutex<SqlConnection>,
    path: String,
}

#[derive(Debug, Clone)]
pub struct ImageNode {
    pub id: i64,
    pub name: String,
    pub source: String,
    pub source_type: String,
    pub status: String,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct BridgePort {
    pub id: i64,
    pub bridge_id: i64,
    pub container_port: i64,
    pub mode: String,
    pub route_path: Option<String>,
    pub protocols: Option<String>,
    pub pool_port: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ClientNode {
    pub id: i64,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub docker_version: String,
    pub port_range_start: i64,
    pub port_range_end: i64,
    pub cpu_cores: i64,
    pub memory_mb: i64,
    pub status: String,
    /// Stable per-machine identifier assigned by the server on the client's
    /// first connect and persisted client-side.
    pub node_uuid: String,
    pub last_seen: i64,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = SqlConnection::open(path).context("Failed to open database")?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .context("Failed to set PRAGMA")?;
        let db = Database {
            conn: Mutex::new(conn),
            path: path.to_string_lossy().to_string(),
        };
        db.migrate()?;
        info!("Database opened at {}", path.display());
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        // Each migration step takes the conn lock independently. The
        // mutex is std::sync::Mutex (non-reentrant), so holding it across
        // multiple steps — including the closure-based `column_exists`
        // check below — would deadlock on legacy DBs that trigger the
        // ALTER TABLE branch.
        {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS image_nodes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                source TEXT NOT NULL,
                source_type TEXT NOT NULL DEFAULT 'docker_hub',
                status TEXT NOT NULL DEFAULT 'loaded',
                created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
            );

            CREATE TABLE IF NOT EXISTS bridges (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                status TEXT NOT NULL DEFAULT 'draft',
                created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
            );

            CREATE TABLE IF NOT EXISTS bridge_ports (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                bridge_id INTEGER NOT NULL,
                container_port INTEGER NOT NULL,
                mode TEXT NOT NULL DEFAULT 'route',
                route_path TEXT,
                protocols TEXT,
                pool_port INTEGER,
                UNIQUE(bridge_id, container_port),
                FOREIGN KEY (bridge_id) REFERENCES bridges(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS bridge_envs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                bridge_id INTEGER NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                FOREIGN KEY (bridge_id) REFERENCES bridges(id) ON DELETE CASCADE,
                UNIQUE(bridge_id, key)
            );

            CREATE TABLE IF NOT EXISTS connections (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                bridge_id INTEGER,
                image_id INTEGER,
                node_id INTEGER,
                status TEXT NOT NULL DEFAULT 'pending',
                container_name TEXT,
                created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                FOREIGN KEY (bridge_id) REFERENCES bridges(id) ON DELETE SET NULL,
                FOREIGN KEY (image_id) REFERENCES image_nodes(id) ON DELETE SET NULL,
                FOREIGN KEY (node_id) REFERENCES client_nodes(id) ON DELETE SET NULL
            );

            CREATE TABLE IF NOT EXISTS server_config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS client_nodes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                hostname TEXT NOT NULL,
                os TEXT NOT NULL DEFAULT '',
                arch TEXT NOT NULL DEFAULT '',
                docker_version TEXT NOT NULL DEFAULT '',
                port_range_start INTEGER NOT NULL DEFAULT 3000,
                port_range_end INTEGER NOT NULL DEFAULT 3999,
                cpu_cores INTEGER NOT NULL DEFAULT 0,
                memory_mb INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL DEFAULT 'offline',
                auth_digest TEXT,
                node_uuid TEXT UNIQUE,
                last_seen INTEGER NOT NULL DEFAULT (strftime('%s','now'))
            );
            ",
        )
        .context("Failed to run migrations")?;
        }

        // Add node_uuid column to pre-existing databases. We keep auth_digest
        // alongside (it's the SHA-256 of the client's auth token, used as the
        // server-side binding key that gates uuid claims). Both columns are
        // written on every connect going forward; existing rows are backfilled
        // below so legacy clients keep their identity (and their referencing
        // connections keep working) on first reconnect.
        let column_exists = |name: &str| -> bool {
            let conn = self.conn.lock().unwrap();
            let mut stmt = match conn.prepare("PRAGMA table_info(client_nodes)") {
                Ok(s) => s,
                Err(_) => return false,
            };
            let rows = stmt.query_map([], |row| row.get::<_, String>(1));
            match rows {
                Ok(rs) => rs.filter_map(|r| r.ok()).any(|n| n == name),
                Err(_) => false,
            }
        };
        if !column_exists("node_uuid") {
            let conn = self.conn.lock().unwrap();
            // SQLite's ALTER TABLE ADD COLUMN can't carry a UNIQUE
            // constraint on an existing table, so add the column plain and
            // then enforce uniqueness with a partial index (lets multiple
            // NULLs coexist until the backfill below replaces them).
            conn.execute("ALTER TABLE client_nodes ADD COLUMN node_uuid TEXT", [])
                .context("Failed to add node_uuid column")?;
            conn.execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_client_nodes_node_uuid \
                 ON client_nodes(node_uuid) WHERE node_uuid IS NOT NULL",
                [],
            )
            .context("Failed to create unique index on node_uuid")?;
        }
        // Backfill: any pre-existing row without a uuid gets one now. The
        // server uses auth_digest → node_uuid as the binding, so when the
        // legacy client reconnects (with no persisted node_id) the server
        // will hand back this exact uuid and the client's old connection
        // rows continue to point at a live node.
        {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT id FROM client_nodes WHERE node_uuid IS NULL AND auth_digest IS NOT NULL",
            )?;
            let legacy_ids: Vec<i64> = stmt
                .query_map([], |row| row.get::<_, i64>(0))?
                .filter_map(|r| r.ok())
                .collect();
            drop(stmt);
            for id in legacy_ids {
                let new_uuid = uuid::Uuid::new_v4().to_string();
                conn.execute(
                    "UPDATE client_nodes SET node_uuid=?1 WHERE id=?2",
                    params![new_uuid, id],
                )?;
            }
        }
        // Any row that still has no uuid (legacy row with no auth_digest
        // either — shouldn't happen, defensive) — null out the connections
        // that point at it so the panel surfaces them as unassigned instead
        // of failing silently at deploy time.
        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "UPDATE connections SET node_id = NULL, status = 'pending' \
                 WHERE node_id IN (SELECT id FROM client_nodes WHERE node_uuid IS NULL)",
                [],
            )
            .ok();
            conn.execute(
                "DELETE FROM client_nodes WHERE node_uuid IS NULL",
                [],
            )
            .ok();
        }
        Ok(())
    }

    // ── Settings (key/value) ──────────────────────────────────

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT value FROM server_config WHERE key=?1")?;
        let mut rows = stmt.query_map(params![key], |row| row.get::<_, String>(0))?;
        Ok(rows.next().transpose()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO server_config (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ── Image ─────────────────────────────────────────────────

    pub fn insert_image(&self, name: &str, source: &str, source_type: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO image_nodes (name, source, source_type, status) VALUES (?1, ?2, ?3, 'loaded')",
            params![name, source, source_type],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn get_image_by_name(&self, name: &str) -> Result<Option<ImageNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, source, source_type, status, created_at FROM image_nodes WHERE name=?1",
        )?;
        let mut rows = stmt.query_map(params![name], |row| {
            Ok(ImageNode {
                id: row.get(0)?,
                name: row.get(1)?,
                source: row.get(2)?,
                source_type: row.get(3)?,
                status: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        Ok(rows.next().transpose()?)
    }

    pub fn get_image_by_id(&self, id: i64) -> Result<Option<ImageNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, source, source_type, status, created_at FROM image_nodes WHERE id=?1",
        )?;
        let mut rows = stmt.query_map(params![id], |row| {
            Ok(ImageNode {
                id: row.get(0)?,
                name: row.get(1)?,
                source: row.get(2)?,
                source_type: row.get(3)?,
                status: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        Ok(rows.next().transpose()?)
    }

    pub fn list_images(&self) -> Result<Vec<ImageNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, source, source_type, status, created_at FROM image_nodes ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ImageNode {
                id: row.get(0)?,
                name: row.get(1)?,
                source: row.get(2)?,
                source_type: row.get(3)?,
                status: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    // ── Bridge (independent template) ─────────────────────────

    pub fn insert_bridge(&self, name: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO bridges (name, status) VALUES (?1, 'draft')",
            params![name],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn list_bridges(&self) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, status, created_at FROM bridges ORDER BY id DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, i64>(0)?,
                "name": row.get::<_, String>(1)?,
                "status": row.get::<_, String>(2)?,
                "created_at": row.get::<_, i64>(3)?,
            }))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn get_bridge_by_id(&self, bridge_id: i64) -> Result<Option<serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, status, created_at FROM bridges WHERE id=?1",
        )?;
        let mut rows = stmt.query_map(params![bridge_id], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, i64>(0)?,
                "name": row.get::<_, String>(1)?,
                "status": row.get::<_, String>(2)?,
                "created_at": row.get::<_, i64>(3)?,
            }))
        })?;
        match rows.next() {
            Some(Ok(v)) => Ok(Some(v)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    pub fn delete_bridge(&self, bridge_id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM bridges WHERE id=?1", params![bridge_id])?;
        Ok(())
    }

    // ── Bridge ports ──────────────────────────────────────────

    pub fn insert_bridge_port(&self, bridge_id: i64, container_port: i64, mode: &str, route_path: Option<&str>, protocols: Option<&str>, pool_port: Option<i64>) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO bridge_ports (bridge_id, container_port, mode, route_path, protocols, pool_port) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![bridge_id, container_port, mode, route_path, protocols, pool_port],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn get_bridge_ports(&self, bridge_id: i64) -> Result<Vec<BridgePort>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, bridge_id, container_port, mode, route_path, protocols, pool_port FROM bridge_ports WHERE bridge_id=?1 ORDER BY container_port",
        )?;
        let rows = stmt.query_map(params![bridge_id], |row| {
            Ok(BridgePort {
                id: row.get(0)?,
                bridge_id: row.get(1)?,
                container_port: row.get(2)?,
                mode: row.get(3)?,
                route_path: row.get(4)?,
                protocols: row.get(5)?,
                pool_port: row.get(6)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn delete_bridge_port(&self, bridge_id: i64, container_port: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM bridge_ports WHERE bridge_id=?1 AND container_port=?2",
            params![bridge_id, container_port],
        )?;
        Ok(())
    }

    pub fn get_bridge_port_pool_port(&self, bridge_id: i64, container_port: i64) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT pool_port FROM bridge_ports WHERE bridge_id=?1 AND container_port=?2",
        )?;
        Ok(stmt.query_row(params![bridge_id, container_port], |row| row.get(0)).ok())
    }

    pub fn get_bridge_pool_ports(&self, bridge_id: i64) -> Result<Vec<i64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT pool_port FROM bridge_ports WHERE bridge_id=?1 AND pool_port IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![bridge_id], |row| row.get::<_, i64>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    // ── Bridge envs ───────────────────────────────────────────

    pub fn set_bridge_envs(&self, bridge_id: i64, envs: &[(String, String)]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM bridge_envs WHERE bridge_id=?1", params![bridge_id])?;
        for (k, v) in envs {
            conn.execute(
                "INSERT INTO bridge_envs (bridge_id, key, value) VALUES (?1, ?2, ?3)",
                params![bridge_id, k, v],
            )?;
        }
        Ok(())
    }

    pub fn get_bridge_envs(&self, bridge_id: i64) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT key, value FROM bridge_envs WHERE bridge_id=?1 ORDER BY key",
        )?;
        let rows = stmt.query_map(params![bridge_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    // ── Connections ───────────────────────────────────────────

    pub fn insert_connection(&self, name: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO connections (name, status) VALUES (?1, 'pending')",
            params![name],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Update a connection's slot fields.
    ///
    /// Each argument is tri-state:
    /// - `None`         → leave the column unchanged
    /// - `Some(None)`   → set the column to NULL
    /// - `Some(Some(x))`→ set the column to `x`
    pub fn update_connection(&self, connection_id: i64, bridge_id: Option<Option<i64>>, image_id: Option<Option<i64>>, node_id: Option<Option<i64>>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let mut current_bridge: Option<i64> = None;
        let mut current_image: Option<i64> = None;
        let mut current_node: Option<i64> = None;
        {
            let mut stmt = conn.prepare("SELECT bridge_id, image_id, node_id FROM connections WHERE id=?1")?;
            let mut rows = stmt.query_map(params![connection_id], |row| {
                Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?, row.get::<_, Option<i64>>(2)?))
            })?;
            if let Some(Ok((b, i, n))) = rows.next() {
                current_bridge = b;
                current_image = i;
                current_node = n;
            }
        }
        let final_bridge = bridge_id.unwrap_or(current_bridge);
        let final_image = image_id.unwrap_or(current_image);
        let final_node = node_id.unwrap_or(current_node);
        conn.execute(
            "UPDATE connections SET bridge_id=?1, image_id=?2, node_id=?3 WHERE id=?4",
            params![final_bridge, final_image, final_node, connection_id],
        )?;
        Ok(())
    }

    pub fn update_connection_node(&self, connection_id: i64, node_id: Option<i64>, status: &str, container_name: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE connections SET node_id=?1, status=?2, container_name=?3 WHERE id=?4",
            params![node_id, status, container_name, connection_id],
        )?;
        Ok(())
    }

    pub fn list_connections(&self) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT c.id, c.name, c.bridge_id, c.image_id, c.node_id, c.status, c.container_name, c.created_at, \
             b.name as bridge_name, i.name as image_name, n.hostname as node_hostname \
             FROM connections c \
             LEFT JOIN bridges b ON c.bridge_id = b.id \
             LEFT JOIN image_nodes i ON c.image_id = i.id \
             LEFT JOIN client_nodes n ON c.node_id = n.id \
             ORDER BY c.created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, i64>(0)?,
                "name": row.get::<_, String>(1)?,
                "bridge_id": row.get::<_, Option<i64>>(2)?,
                "image_id": row.get::<_, Option<i64>>(3)?,
                "node_id": row.get::<_, Option<i64>>(4)?,
                "status": row.get::<_, String>(5)?,
                "container_name": row.get::<_, Option<String>>(6)?,
                "created_at": row.get::<_, i64>(7)?,
                "bridge_name": row.get::<_, Option<String>>(8)?,
                "image_name": row.get::<_, Option<String>>(9)?,
                "node_hostname": row.get::<_, Option<String>>(10)?,
            }))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn get_connection(&self, id: i64) -> Result<Option<serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT c.id, c.name, c.bridge_id, c.image_id, c.node_id, c.status, c.container_name, c.created_at, \
             b.name as bridge_name, i.name as image_name, n.hostname as node_hostname, n.status as node_status \
             FROM connections c \
             LEFT JOIN bridges b ON c.bridge_id = b.id \
             LEFT JOIN image_nodes i ON c.image_id = i.id \
             LEFT JOIN client_nodes n ON c.node_id = n.id \
             WHERE c.id=?1",
        )?;
        let mut rows = stmt.query_map(params![id], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, i64>(0)?,
                "name": row.get::<_, String>(1)?,
                "bridge_id": row.get::<_, Option<i64>>(2)?,
                "image_id": row.get::<_, Option<i64>>(3)?,
                "node_id": row.get::<_, Option<i64>>(4)?,
                "status": row.get::<_, String>(5)?,
                "container_name": row.get::<_, Option<String>>(6)?,
                "created_at": row.get::<_, i64>(7)?,
                "bridge_name": row.get::<_, Option<String>>(8)?,
                "image_name": row.get::<_, Option<String>>(9)?,
                "node_hostname": row.get::<_, Option<String>>(10)?,
                "node_status": row.get::<_, Option<String>>(11)?,
            }))
        })?;
        match rows.next() {
            Some(Ok(v)) => Ok(Some(v)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    pub fn delete_connection(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM connections WHERE id=?1", params![id])?;
        Ok(())
    }

    /// Get all connections that are ready to deploy (all three set + node online)
    pub fn get_connectable_connections(&self) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT c.id FROM connections c \
             JOIN client_nodes n ON c.node_id = n.id \
             WHERE c.bridge_id IS NOT NULL AND c.image_id IS NOT NULL AND c.node_id IS NOT NULL \
             AND n.status = 'online' AND c.status = 'pending'",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(serde_json::json!({ "id": row.get::<_, i64>(0)? }))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn get_connection_ids_for_node(&self, node_id: i64) -> Result<Vec<i64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id FROM connections WHERE node_id=?1 AND status='deployed'",
        )?;
        let rows = stmt.query_map(params![node_id], |row| row.get::<_, i64>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    // ── Nodes ─────────────────────────────────────────────────

    pub fn upsert_node(
        &self,
        node_uuid: &str,
        service_digest: &str,
        hostname: &str,
        os: &str,
        arch: &str,
        docker_version: &str,
        port_range_start: i64,
        port_range_end: i64,
        cpu_cores: i64,
        memory_mb: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO client_nodes (hostname, os, arch, docker_version, port_range_start, port_range_end, cpu_cores, memory_mb, status, node_uuid, auth_digest, last_seen) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'online', ?9, ?10, strftime('%s','now')) \
             ON CONFLICT(node_uuid) DO UPDATE SET \
             hostname=excluded.hostname, os=excluded.os, arch=excluded.arch, \
             docker_version=excluded.docker_version, \
             port_range_start=excluded.port_range_start, port_range_end=excluded.port_range_end, \
             cpu_cores=excluded.cpu_cores, memory_mb=excluded.memory_mb, \
             auth_digest=excluded.auth_digest, \
             status='online', last_seen=strftime('%s','now')",
            params![hostname, os, arch, docker_version, port_range_start, port_range_end, cpu_cores, memory_mb, node_uuid, service_digest],
        )?;
        Ok(())
    }

    /// Returns `(node_uuid → service_digest_hex)` for every node currently
    /// in the table. Seeds the tunnel server's binding map at startup so
    /// the spoof check survives kdcts restarts. One digest can map to
    /// multiple uuids — that's how two machines sharing one auth token
    /// keep distinct identities.
    pub fn load_bindings(&self) -> Result<std::collections::HashMap<String, String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT node_uuid, auth_digest FROM client_nodes \
             WHERE auth_digest IS NOT NULL AND node_uuid IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn list_nodes(&self) -> Result<Vec<ClientNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, hostname, os, arch, docker_version, port_range_start, port_range_end, \
             cpu_cores, memory_mb, status, COALESCE(node_uuid, ''), last_seen \
             FROM client_nodes WHERE node_uuid IS NOT NULL ORDER BY last_seen DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ClientNode {
                id: row.get(0)?,
                hostname: row.get(1)?,
                os: row.get(2)?,
                arch: row.get(3)?,
                docker_version: row.get(4)?,
                port_range_start: row.get(5)?,
                port_range_end: row.get(6)?,
                cpu_cores: row.get(7)?,
                memory_mb: row.get(8)?,
                status: row.get(9)?,
                node_uuid: row.get(10)?,
                last_seen: row.get(11)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn set_node_offline(&self, node_uuid: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE client_nodes SET status='offline' WHERE node_uuid=?1",
            params![node_uuid],
        )?;
        Ok(())
    }

    pub fn mark_all_offline(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE client_nodes SET status='offline'", [])?;
        Ok(())
    }

    pub fn get_node_by_id(&self, id: i64) -> Result<Option<ClientNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, hostname, os, arch, docker_version, port_range_start, port_range_end, \
             cpu_cores, memory_mb, status, COALESCE(node_uuid, ''), last_seen \
             FROM client_nodes WHERE id=?1",
        )?;
        let mut rows = stmt.query_map(params![id], |row| {
            Ok(ClientNode {
                id: row.get(0)?,
                hostname: row.get(1)?,
                os: row.get(2)?,
                arch: row.get(3)?,
                docker_version: row.get(4)?,
                port_range_start: row.get(5)?,
                port_range_end: row.get(6)?,
                cpu_cores: row.get(7)?,
                memory_mb: row.get(8)?,
                status: row.get(9)?,
                node_uuid: row.get(10)?,
                last_seen: row.get(11)?,
            })
        })?;
        Ok(rows.next().transpose()?)
    }

    pub fn get_node_by_uuid(&self, node_uuid: &str) -> Result<Option<ClientNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, hostname, os, arch, docker_version, port_range_start, port_range_end, \
             cpu_cores, memory_mb, status, COALESCE(node_uuid, ''), last_seen \
             FROM client_nodes WHERE node_uuid=?1",
        )?;
        let mut rows = stmt.query_map(params![node_uuid], |row| {
            Ok(ClientNode {
                id: row.get(0)?,
                hostname: row.get(1)?,
                os: row.get(2)?,
                arch: row.get(3)?,
                docker_version: row.get(4)?,
                port_range_start: row.get(5)?,
                port_range_end: row.get(6)?,
                cpu_cores: row.get(7)?,
                memory_mb: row.get(8)?,
                status: row.get(9)?,
                node_uuid: row.get(10)?,
                last_seen: row.get(11)?,
            })
        })?;
        Ok(rows.next().transpose()?)
    }
}
