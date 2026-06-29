//! Persistent state storage via SQLite.
//!
//! Stores: users, metric history, environment variables, daemon configuration.
#![allow(dead_code)] // MVP: some methods used in Phase 2

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use crate::server::v1::AppMetric;

/// Persistent store backed by SQLite.
pub struct Store {
    conn: Mutex<rusqlite::Connection>,
}

impl Store {
    /// Open (or create) the database at `path`.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = rusqlite::Connection::open(path)?;
        // Create tables
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS users (
                username TEXT PRIMARY KEY,
                password_hash TEXT NOT NULL,
                role TEXT NOT NULL DEFAULT 'user'
            );
            CREATE TABLE IF NOT EXISTS metrics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                app_name TEXT NOT NULL,
                cpu_percent REAL NOT NULL DEFAULT 0,
                ram_bytes INTEGER NOT NULL DEFAULT 0,
                net_rx_bytes INTEGER NOT NULL DEFAULT 0,
                net_tx_bytes INTEGER NOT NULL DEFAULT 0,
                timestamp_unix INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS apps (
                name TEXT PRIMARY KEY,
                domain TEXT,
                port INTEGER,
                env_json TEXT NOT NULL DEFAULT '{}'
            );
            CREATE TABLE IF NOT EXISTS nodes (
                name TEXT PRIMARY KEY,
                addr TEXT NOT NULL,
                labels_json TEXT NOT NULL DEFAULT '{}'
            );",
        )?;
        tracing::info!("Database tables initialized");
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    // ── User management ───────────────────────────────────────────

    /// Create a new user.
    pub fn create_user(
        &self,
        username: &str,
        password_hash: &str,
        role: &str,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO users (username, password_hash, role) VALUES (?1, ?2, ?3)",
            rusqlite::params![username, password_hash, role],
        )?;
        Ok(())
    }

    /// Get a user by username.
    pub fn get_user(&self, username: &str) -> anyhow::Result<Option<UserRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT username, password_hash, role FROM users WHERE username = ?1",
        )?;
        let result = stmt
            .query_row(rusqlite::params![username], |row| {
                Ok(UserRecord {
                    username: row.get(0)?,
                    password_hash: row.get(1)?,
                    role: row.get(2)?,
                })
            })
            .optional()?;
        Ok(result)
    }

    /// List all users.
    pub fn list_users(&self) -> anyhow::Result<Vec<UserRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT username, password_hash, role FROM users ORDER BY username",
        )?;
        let records = stmt
            .query_map([], |row| {
                Ok(UserRecord {
                    username: row.get(0)?,
                    password_hash: row.get(1)?,
                    role: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(records)
    }

    /// Delete a user by username.
    pub fn delete_user(&self, username: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let affected = conn.execute(
            "DELETE FROM users WHERE username = ?1",
            rusqlite::params![username],
        )?;
        if affected == 0 {
            anyhow::bail!("User '{}' not found", username);
        }
        Ok(())
    }

    // ── Metrics ───────────────────────────────────────────────────

    /// Insert a metric snapshot into the database.
    pub fn insert_metric(&self, metric: &AppMetric) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO metrics (app_name, cpu_percent, ram_bytes, net_rx_bytes, net_tx_bytes, timestamp_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                metric.app_name,
                metric.cpu_percent,
                metric.ram_bytes as i64,
                metric.net_rx_bytes as i64,
                metric.net_tx_bytes as i64,
                metric.timestamp_unix as i64,
            ],
        )?;
        Ok(())
    }

    /// Get recent metrics for an app (last `limit` entries).
    pub fn get_recent_metrics(&self, app_name: &str, limit: u32) -> anyhow::Result<Vec<AppMetric>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT app_name, cpu_percent, ram_bytes, net_rx_bytes, net_tx_bytes, timestamp_unix
             FROM metrics WHERE app_name = ?1 ORDER BY timestamp_unix DESC LIMIT ?2",
        )?;
        let metrics = stmt
            .query_map(rusqlite::params![app_name, limit], |row| {
                Ok(AppMetric {
                    app_name: row.get(0)?,
                    cpu_percent: row.get(1)?,
                    ram_bytes: row.get::<_, i64>(2)? as u64,
                    net_rx_bytes: row.get::<_, i64>(3)? as u64,
                    net_tx_bytes: row.get::<_, i64>(4)? as u64,
                    timestamp_unix: row.get::<_, i64>(5)? as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(metrics)
    }

    /// Get a configuration value by key.
    pub fn get_config(&self, key: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT value FROM config WHERE key = ?1")?;
        let result = stmt
            .query_row(rusqlite::params![key], |row| row.get(0))
            .optional()?;
        Ok(result)
    }

    /// Set a configuration value.
    pub fn set_config(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO config (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    /// Insert or update an app record.
    pub fn upsert_app(
        &self,
        name: &str,
        domain: Option<&str>,
        port: Option<u32>,
        env: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let env_json = serde_json::to_string(env)?;
        conn.execute(
            "INSERT INTO apps (name, domain, port, env_json) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(name) DO UPDATE SET domain = ?2, port = ?3, env_json = ?4",
            rusqlite::params![name, domain, port, env_json],
        )?;
        Ok(())
    }

    /// Get an app record by name.
    pub fn get_app(&self, name: &str) -> anyhow::Result<Option<AppRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, domain, port, env_json FROM apps WHERE name = ?1",
        )?;
        let result = stmt
            .query_row(rusqlite::params![name], |row| {
                Ok(AppRecord {
                    name: row.get(0)?,
                    domain: row.get(1)?,
                    port: row.get::<_, Option<i64>>(2)?.map(|p| p as u32),
                    env_json: row.get(3)?,
                })
            })
            .optional()?;
        Ok(result)
    }

    /// List all app records.
    pub fn list_apps(&self) -> anyhow::Result<Vec<AppRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, domain, port, env_json FROM apps ORDER BY name",
        )?;
        let records = stmt
            .query_map([], |row| {
                Ok(AppRecord {
                    name: row.get(0)?,
                    domain: row.get(1)?,
                    port: row.get::<_, Option<i64>>(2)?.map(|p| p as u32),
                    env_json: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(records)
    }

    /// Get environment variables for an app as a HashMap.
    pub fn get_app_env(&self, app_name: &str) -> anyhow::Result<HashMap<String, String>> {
        match self.get_app(app_name)? {
            Some(record) => {
                let env: HashMap<String, String> = serde_json::from_str(&record.env_json)?;
                Ok(env)
            }
            None => Ok(HashMap::new()),
        }
    }

    /// Set an environment variable for an app.
    pub fn set_app_env(&self, app_name: &str, key: &str, value: &str) -> anyhow::Result<()> {
        let mut env = self.get_app_env(app_name)?;
        env.insert(key.to_string(), value.to_string());
        let env_json = serde_json::to_string(&env)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO apps (name, domain, port, env_json) VALUES (?1, NULL, NULL, ?2)
             ON CONFLICT(name) DO UPDATE SET env_json = ?2",
            rusqlite::params![app_name, env_json],
        )?;
        Ok(())
    }

    /// Unset (remove) an environment variable for an app.
    pub fn unset_app_env(&self, app_name: &str, key: &str) -> anyhow::Result<()> {
        let mut env = self.get_app_env(app_name)?;
        env.remove(key);
        let env_json = serde_json::to_string(&env)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO apps (name, domain, port, env_json) VALUES (?1, NULL, NULL, ?2)
             ON CONFLICT(name) DO UPDATE SET env_json = ?2",
            rusqlite::params![app_name, env_json],
        )?;
        Ok(())
    }

    // ── Cluster node management ─────────────────────────────────────

    /// Insert or update a cluster node.
    pub fn upsert_node(
        &self,
        name: &str,
        addr: &str,
        labels: &std::collections::HashMap<String, String>,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let labels_json = serde_json::to_string(labels)?;
        conn.execute(
            "INSERT INTO nodes (name, addr, labels_json) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET addr = ?2, labels_json = ?3",
            rusqlite::params![name, addr, labels_json],
        )?;
        Ok(())
    }

    /// Get a node by name.
    pub fn get_node(&self, name: &str) -> anyhow::Result<Option<NodeRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, addr, labels_json FROM nodes WHERE name = ?1",
        )?;
        let result = stmt
            .query_row(rusqlite::params![name], |row| {
                Ok(NodeRecord {
                    name: row.get(0)?,
                    addr: row.get(1)?,
                    labels_json: row.get(2)?,
                })
            })
            .optional()?;
        Ok(result)
    }

    /// List all cluster nodes.
    pub fn list_nodes(&self) -> anyhow::Result<Vec<NodeRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, addr, labels_json FROM nodes ORDER BY name",
        )?;
        let records = stmt
            .query_map([], |row| {
                Ok(NodeRecord {
                    name: row.get(0)?,
                    addr: row.get(1)?,
                    labels_json: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(records)
    }

    /// Delete a node by name.
    pub fn delete_node(&self, name: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let affected = conn.execute(
            "DELETE FROM nodes WHERE name = ?1",
            rusqlite::params![name],
        )?;
        if affected == 0 {
            anyhow::bail!("Node '{}' not found", name);
        }
        Ok(())
    }
}

/// A row from the `users` table.
#[derive(Debug, Clone)]
pub struct UserRecord {
    pub username: String,
    pub password_hash: String,
    pub role: String,
}

/// A row from the `apps` table.
#[derive(Debug, Clone)]
pub struct AppRecord {
    pub name: String,
    pub domain: Option<String>,
    pub port: Option<u32>,
    pub env_json: String,
}

/// A row from the `nodes` table.
#[derive(Debug, Clone)]
pub struct NodeRecord {
    pub name: String,
    pub addr: String,
    pub labels_json: String,
}

/// Helper trait for rusqlite optional row fetching.
trait OptionalExt<T> {
    fn optional(self) -> rusqlite::Result<Option<T>>;
}

impl<T> OptionalExt<T> for rusqlite::Result<T> {
    fn optional(self) -> rusqlite::Result<Option<T>> {
        match self {
            Ok(val) => Ok(Some(val)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
