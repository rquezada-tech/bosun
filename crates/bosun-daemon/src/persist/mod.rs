//! Persistent state storage via SQLite.
//!
//! Stores: metric history, environment variables, daemon configuration.

use std::path::Path;

/// Persistent store backed by SQLite.
pub struct Store {
    conn: std::sync::Mutex<rusqlite::Connection>,
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
            "CREATE TABLE IF NOT EXISTS metrics (
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
            );",
        )?;
        Ok(Self {
            conn: std::sync::Mutex::new(conn),
        })
    }
}
