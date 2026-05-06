use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::info;

pub struct Database {
    conn: Mutex<Connection>,
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
pub struct ImagePort {
    pub id: i64,
    pub image_node_id: i64,
    pub port: i64,
    pub protocol: String,
}

#[derive(Debug, Clone)]
pub struct ImageRoute {
    pub id: i64,
    pub image_port_id: i64,
    pub path: String,
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
    pub auth_digest: Option<String>,
    pub last_seen: i64,
}

#[derive(Debug, Clone)]
pub struct Deployment {
    pub id: i64,
    pub image_node_id: i64,
    pub client_node_id: i64,
    pub status: String,
    pub deployed_at: i64,
}

#[derive(Debug, Clone)]
pub struct PortAllocation {
    pub id: i64,
    pub deployment_id: i64,
    pub image_port_id: i64,
    pub client_port: i64,
    pub server_port: i64,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).context("Failed to open database")?;
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

    pub fn clone_for_connection(&self) -> Result<Self> {
        Database::open(&PathBuf::from(&self.path))
    }

    fn migrate(&self) -> Result<()> {
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

            CREATE TABLE IF NOT EXISTS image_ports (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                image_node_id INTEGER NOT NULL,
                port INTEGER NOT NULL,
                protocol TEXT NOT NULL DEFAULT 'tcp',
                FOREIGN KEY (image_node_id) REFERENCES image_nodes(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS image_routes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                image_port_id INTEGER NOT NULL UNIQUE,
                path TEXT NOT NULL,
                FOREIGN KEY (image_port_id) REFERENCES image_ports(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS image_envs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                image_node_id INTEGER NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                FOREIGN KEY (image_node_id) REFERENCES image_nodes(id) ON DELETE CASCADE,
                UNIQUE(image_node_id, key)
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
                auth_digest TEXT UNIQUE,
                last_seen INTEGER NOT NULL DEFAULT (strftime('%s','now'))
            );

            CREATE TABLE IF NOT EXISTS deployments (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                image_node_id INTEGER NOT NULL UNIQUE,
                client_node_id INTEGER NOT NULL,
                status TEXT NOT NULL DEFAULT 'running',
                deployed_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                FOREIGN KEY (image_node_id) REFERENCES image_nodes(id),
                FOREIGN KEY (client_node_id) REFERENCES client_nodes(id)
            );

            CREATE TABLE IF NOT EXISTS port_allocations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                deployment_id INTEGER NOT NULL,
                image_port_id INTEGER NOT NULL,
                client_port INTEGER NOT NULL,
                server_port INTEGER NOT NULL,
                FOREIGN KEY (deployment_id) REFERENCES deployments(id) ON DELETE CASCADE,
                FOREIGN KEY (image_port_id) REFERENCES image_ports(id)
            );
            ",
        )
        .context("Failed to run migrations")?;
        Ok(())
    }

    // ── Image operations ──────────────────────────────────────

    pub fn insert_image(&self, name: &str, source: &str, source_type: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO image_nodes (name, source, source_type, status) VALUES (?1, ?2, ?3, 'loaded')",
            params![name, source, source_type],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn update_image_status(&self, id: i64, status: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE image_nodes SET status=?1 WHERE id=?2", params![status, id])?;
        Ok(())
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

    pub fn insert_image_port(&self, image_node_id: i64, port: i64, protocol: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO image_ports (image_node_id, port, protocol) VALUES (?1, ?2, ?3)",
            params![image_node_id, port, protocol],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn get_image_ports(&self, image_node_id: i64) -> Result<Vec<ImagePort>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, image_node_id, port, protocol FROM image_ports WHERE image_node_id=?1 ORDER BY port",
        )?;
        let rows = stmt.query_map(params![image_node_id], |row| {
            Ok(ImagePort {
                id: row.get(0)?,
                image_node_id: row.get(1)?,
                port: row.get(2)?,
                protocol: row.get(3)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn set_image_route(&self, image_port_id: i64, path: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO image_routes (image_port_id, path) VALUES (?1, ?2)",
            params![image_port_id, path],
        )?;
        Ok(())
    }

    pub fn get_image_routes(&self, image_node_id: i64) -> Result<Vec<(ImagePort, Option<String>)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT p.id, p.image_node_id, p.port, p.protocol, r.path \
             FROM image_ports p LEFT JOIN image_routes r ON p.id = r.image_port_id \
             WHERE p.image_node_id=?1 ORDER BY p.port",
        )?;
        let rows = stmt.query_map(params![image_node_id], |row| {
            Ok((
                ImagePort {
                    id: row.get(0)?,
                    image_node_id: row.get(1)?,
                    port: row.get(2)?,
                    protocol: row.get(3)?,
                },
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn set_image_envs(&self, image_node_id: i64, envs: &[(String, String)]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM image_envs WHERE image_node_id=?1", params![image_node_id])?;
        for (k, v) in envs {
            conn.execute(
                "INSERT INTO image_envs (image_node_id, key, value) VALUES (?1, ?2, ?3)",
                params![image_node_id, k, v],
            )?;
        }
        Ok(())
    }

    pub fn get_image_envs(&self, image_node_id: i64) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT key, value FROM image_envs WHERE image_node_id=?1 ORDER BY key",
        )?;
        let rows = stmt.query_map(params![image_node_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    // ── Node operations ───────────────────────────────────────

    pub fn upsert_node(
        &self,
        auth_digest: &str,
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
            "INSERT INTO client_nodes (hostname, os, arch, docker_version, port_range_start, port_range_end, cpu_cores, memory_mb, status, auth_digest, last_seen) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'online', ?9, strftime('%s','now')) \
             ON CONFLICT(auth_digest) DO UPDATE SET \
             hostname=excluded.hostname, os=excluded.os, arch=excluded.arch, \
             docker_version=excluded.docker_version, \
             port_range_start=excluded.port_range_start, port_range_end=excluded.port_range_end, \
             cpu_cores=excluded.cpu_cores, memory_mb=excluded.memory_mb, \
             status='online', last_seen=strftime('%s','now')",
            params![hostname, os, arch, docker_version, port_range_start, port_range_end, cpu_cores, memory_mb, auth_digest],
        )?;
        Ok(())
    }

    pub fn set_node_offline(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE client_nodes SET status='offline' WHERE id=?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn list_nodes(&self) -> Result<Vec<ClientNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, hostname, os, arch, docker_version, port_range_start, port_range_end, \
             cpu_cores, memory_mb, status, auth_digest, last_seen FROM client_nodes ORDER BY last_seen DESC",
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
                auth_digest: row.get(10)?,
                last_seen: row.get(11)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    // ── Deployment operations ─────────────────────────────────

    pub fn insert_deployment(&self, image_node_id: i64, client_node_id: i64) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO deployments (image_node_id, client_node_id, status) VALUES (?1, ?2, 'running')",
            params![image_node_id, client_node_id],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn set_deployment_stopped(&self, image_node_id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE deployments SET status='stopped' WHERE image_node_id=?1",
            params![image_node_id],
        )?;
        Ok(())
    }

    pub fn get_deployment_by_image(&self, image_node_id: i64) -> Result<Option<Deployment>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, image_node_id, client_node_id, status, deployed_at FROM deployments WHERE image_node_id=?1",
        )?;
        let mut rows = stmt.query_map(params![image_node_id], |row| {
            Ok(Deployment {
                id: row.get(0)?,
                image_node_id: row.get(1)?,
                client_node_id: row.get(2)?,
                status: row.get(3)?,
                deployed_at: row.get(4)?,
            })
        })?;
        Ok(rows.next().transpose()?)
    }

    pub fn insert_port_allocation(
        &self,
        deployment_id: i64,
        image_port_id: i64,
        client_port: i64,
        server_port: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO port_allocations (deployment_id, image_port_id, client_port, server_port) VALUES (?1, ?2, ?3, ?4)",
            params![deployment_id, image_port_id, client_port, server_port],
        )?;
        Ok(())
    }

    pub fn get_port_allocations(&self, deployment_id: i64) -> Result<Vec<PortAllocation>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, deployment_id, image_port_id, client_port, server_port FROM port_allocations WHERE deployment_id=?1",
        )?;
        let rows = stmt.query_map(params![deployment_id], |row| {
            Ok(PortAllocation {
                id: row.get(0)?,
                deployment_id: row.get(1)?,
                image_port_id: row.get(2)?,
                client_port: row.get(3)?,
                server_port: row.get(4)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Get all port allocations with active deployments (for rebuilding RouteTable on restart)
    pub fn get_active_routes(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT r.path, pa.server_port \
             FROM image_routes r \
             JOIN image_ports p ON r.image_port_id = p.id \
             JOIN port_allocations pa ON pa.image_port_id = p.id \
             JOIN deployments d ON d.id = pa.deployment_id \
             WHERE d.status = 'running'",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }
}
