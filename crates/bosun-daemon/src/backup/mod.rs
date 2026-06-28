//! Backup and restore system for Bosun apps.
//!
//! Creates tar.gz snapshots of app volume data + metadata (env vars, config),
//! and can restore them — recreating containers as needed.
//!
//! Backups are stored at: /var/lib/bosun/backups/{app}/{timestamp}.tar.gz
//! with an accompanying metadata.json file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::server::v1::BackupInfo;

/// Metadata stored alongside each backup tarball.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackupMetadata {
    pub app_name: String,
    pub timestamp_unix: u64,
    /// Docker image used by the container at backup time.
    pub image: String,
    /// Volume mounts: host_path -> container_path.
    pub volumes: HashMap<String, String>,
    /// Environment variables (KEY=VALUE pairs).
    pub env: Vec<String>,
    /// Port mapping (host port).
    pub port: Option<u16>,
    /// Domain if configured.
    pub domain: Option<String>,
    /// Docker labels at backup time.
    pub labels: HashMap<String, String>,
}

/// Service that handles backup creation, listing, and restoration.
pub struct BackupService {
    /// Root directory for backups: {data_dir}/backups
    backups_dir: PathBuf,
    /// Shared Docker client for container operations.
    docker: Arc<tokio::sync::Mutex<crate::docker::DockerClient>>,
}

impl BackupService {
    /// Create a new BackupService.
    ///
    /// `data_dir` is the Bosun data directory (e.g. `/var/lib/bosun`).
    /// Backups are stored under `{data_dir}/backups/`.
    pub fn new(
        data_dir: &Path,
        docker: Arc<tokio::sync::Mutex<crate::docker::DockerClient>>,
    ) -> Self {
        let backups_dir = data_dir.join("backups");
        Self {
            backups_dir,
            docker,
        }
    }

    // ── Public API ─────────────────────────────────────────────────

    /// Create a backup of an app's volumes and configuration.
    ///
    /// 1. Inspects the container for volumes, env, image, labels, and port.
    /// 2. Creates a tar.gz of volume host paths.
    /// 3. Saves metadata JSON alongside the backup.
    /// 4. Returns BackupInfo.
    pub async fn create_backup(&self, app_name: &str) -> anyhow::Result<BackupInfo> {
        let docker = self.docker.lock().await;
        let inspect = docker
            .inspect_container(app_name)
            .await
            .map_err(|e| anyhow::anyhow!("Container '{}' not found: {}", app_name, e))?;

        let config = inspect
            .config
            .ok_or_else(|| anyhow::anyhow!("Container '{}' has no config", app_name))?;

        let host_config = inspect
            .host_config
            .ok_or_else(|| anyhow::anyhow!("Container '{}' has no host config", app_name))?;

        let image = config
            .image
            .clone()
            .unwrap_or_else(|| format!("{}:latest", app_name));

        let labels = config.labels.clone().unwrap_or_default();

        // Extract volume bind mounts from HostConfig
        let binds = host_config.binds.clone().unwrap_or_default();
        let mut volumes: HashMap<String, String> = HashMap::new();
        for bind in &binds {
            // Format: "host_path:container_path:mode"
            let parts: Vec<&str> = bind.splitn(2, ':').collect();
            if parts.len() == 2 {
                let host_path = parts[0].to_string();
                let container_path = parts[1]
                    .split(':')
                    .next()
                    .unwrap_or("")
                    .to_string();
                volumes.insert(host_path, container_path);
            }
        }

        let env: Vec<String> = config.env.clone().unwrap_or_default();

        // Extract port from labels or port bindings
        let port: Option<u16> = labels
            .get("bosun.port")
            .and_then(|p| p.parse().ok());

        let domain: Option<String> = labels.get("bosun.domain").cloned();

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Create app backup directory
        let app_backup_dir = self.backups_dir.join(app_name);
        std::fs::create_dir_all(&app_backup_dir)?;

        let backup_id = format!("{}-{}", app_name, timestamp);
        let tarball_name = format!("{}.tar.gz", backup_id);
        let tarball_path = app_backup_dir.join(&tarball_name);
        let metadata_path = app_backup_dir.join(format!("{}.metadata.json", backup_id));

        // Build metadata
        let metadata = BackupMetadata {
            app_name: app_name.to_string(),
            timestamp_unix: timestamp,
            image: image.clone(),
            volumes: volumes.clone(),
            env: env.clone(),
            port,
            domain: domain.clone(),
            labels: labels.clone(),
        };

        // Create tar.gz of volume host paths
        let size_bytes = create_backup_tarball(&tarball_path, &volumes)?;

        // Write metadata JSON
        let metadata_json = serde_json::to_string_pretty(&metadata)?;
        std::fs::write(&metadata_path, metadata_json)?;

        tracing::info!(
            "Backup created: {} ({} bytes, {} volumes)",
            backup_id,
            size_bytes,
            volumes.len()
        );

        Ok(BackupInfo {
            id: backup_id,
            app_name: app_name.to_string(),
            timestamp_unix: timestamp,
            size_bytes,
        })
    }

    /// List all backups, optionally filtered by app name.
    pub fn list_backups(&self, app_name: Option<&str>) -> anyhow::Result<Vec<BackupInfo>> {
        let mut backups: Vec<BackupInfo> = Vec::new();

        if !self.backups_dir.exists() {
            return Ok(backups);
        }

        let dirs: Vec<PathBuf> = if let Some(app) = app_name {
            let app_dir = self.backups_dir.join(app);
            if app_dir.exists() {
                vec![app_dir]
            } else {
                return Ok(backups);
            }
        } else {
            std::fs::read_dir(&self.backups_dir)?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        };

        for dir in &dirs {
            let entries = std::fs::read_dir(dir)?;
            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                if path.extension().map_or(false, |ext| ext == "gz") {
                    // Parse backup ID from filename: {app}-{timestamp}.tar.gz
                    let stem = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("");
                    // stem is "{app}-{timestamp}" for the tar.gz, but we know the app from directory
                    let app = dir
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown");

                    let size_bytes = path.metadata().map(|m| m.len()).unwrap_or(0);

                    // Try to read timestamp from metadata first (most reliable)
                    let metadata_file = dir.join(format!("{}.metadata.json", stem));
                    let timestamp = if metadata_file.exists() {
                        std::fs::read_to_string(&metadata_file)
                            .ok()
                            .and_then(|s| serde_json::from_str::<BackupMetadata>(&s).ok())
                            .map(|m| m.timestamp_unix)
                            .unwrap_or(0)
                    } else {
                        // Fallback: parse from filename
                        stem.rsplit('-')
                            .next()
                            .and_then(|ts| ts.parse().ok())
                            .unwrap_or(0)
                    };

                    backups.push(BackupInfo {
                        id: stem.to_string(),
                        app_name: app.to_string(),
                        timestamp_unix: timestamp,
                        size_bytes,
                    });
                }
            }
        }

        // Sort by timestamp descending (newest first)
        backups.sort_by(|a, b| b.timestamp_unix.cmp(&a.timestamp_unix));
        Ok(backups)
    }

    /// Restore a backup by its ID.
    ///
    /// 1. Reads metadata JSON.
    /// 2. If container exists, stops and removes it.
    /// 3. Extracts tar.gz to volume paths.
    /// 4. Recreates container with the same config.
    /// 5. Starts the container.
    pub async fn restore_backup(&self, backup_id: &str) -> anyhow::Result<(String, String)> {
        // Parse backup_id: "{app}-{timestamp}"
        let (app_name, _timestamp) = parse_backup_id(backup_id)?;

        let app_backup_dir = self.backups_dir.join(&app_name);
        let tarball_path = app_backup_dir.join(format!("{}.tar.gz", backup_id));
        let metadata_path = app_backup_dir.join(format!("{}.metadata.json", backup_id));

        if !tarball_path.exists() {
            anyhow::bail!("Backup tarball not found: {}", tarball_path.display());
        }
        if !metadata_path.exists() {
            anyhow::bail!("Backup metadata not found: {}", metadata_path.display());
        }

        let metadata_json = std::fs::read_to_string(&metadata_path)?;
        let metadata: BackupMetadata = serde_json::from_str(&metadata_json)?;

        tracing::info!(
            "Restoring backup '{}' for app '{}' (image={}, {} volumes)",
            backup_id,
            app_name,
            metadata.image,
            metadata.volumes.len()
        );

        // 1. Stop and remove existing container if it exists
        let docker = self.docker.lock().await;
        match docker.inspect_container(&app_name).await {
            Ok(_) => {
                tracing::info!("Stopping existing container '{}'...", app_name);
                let _ = docker.stop_container(&app_name).await;
                tracing::info!("Removing existing container '{}'...", app_name);
                docker
                    .force_remove_container(&app_name)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to remove existing container '{}': {}", app_name, e))?;
            }
            Err(_) => {
                tracing::info!("No existing container '{}' to replace", app_name);
            }
        };

        // 2. Extract tar.gz to volume host paths
        tracing::info!("Extracting backup to volume paths...");
        extract_backup_tarball(&tarball_path, &metadata.volumes)?;

        // 3. Recreate container with same config
        let port = metadata.port.unwrap_or(8080);
        let image = metadata.image.clone();
        let domain = metadata.domain.as_deref();
        let env_vars: HashMap<String, String> = metadata
            .env
            .iter()
            .filter_map(|e| {
                let parts: Vec<&str> = e.splitn(2, '=').collect();
                if parts.len() == 2 {
                    Some((parts[0].to_string(), parts[1].to_string()))
                } else {
                    None
                }
            })
            .collect();

        tracing::info!(
            "Recreating container '{}' from image '{}' (port={})...",
            app_name,
            image,
            port
        );

        // Check if it's a template-based deployment
        let template_name = metadata.labels.get("bosun.template").cloned();
        if let Some(ref template) = template_name {
            tracing::info!("Restoring template-based app '{}' (template={})", app_name, template);
        }

        // Recreate using the standard deploy flow
        docker
            .restore_container(
                &app_name,
                &image,
                port,
                domain,
                &env_vars,
                &metadata.volumes,
                &metadata.labels,
            )
            .await?;

        tracing::info!(
            "Backup '{}' restored successfully for app '{}'",
            backup_id,
            app_name
        );

        Ok((app_name, "restored".to_string()))
    }
}

// ── Helpers ────────────────────────────────────────────────────────

/// Create a tar.gz archive of the given volume host paths.
/// Returns the size of the created file in bytes.
fn create_backup_tarball(
    tarball_path: &Path,
    volumes: &HashMap<String, String>,
) -> anyhow::Result<u64> {
    let file = std::fs::File::create(tarball_path)?;
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut tar = tar::Builder::new(encoder);

    for (host_path, container_path) in volumes {
        let host = Path::new(host_path);
        if host.exists() {
            // Strip leading slash for tar entry paths
            let entry_name = container_path.trim_start_matches('/');
            tar.append_dir_all(entry_name, host)?;
        } else {
            tracing::warn!(
                "Volume host path '{}' does not exist — skipping in backup",
                host_path
            );
        }
    }

    let encoder = tar.into_inner()?;
    let file = encoder.finish()?;
    let size = file.metadata()?.len();
    Ok(size)
}

/// Extract a tar.gz archive to the specified volume host paths.
/// Files are extracted relative to each host path.
fn extract_backup_tarball(
    tarball_path: &Path,
    volumes: &HashMap<String, String>,
) -> anyhow::Result<()> {
    let file = std::fs::File::open(tarball_path)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);

    // For each volume, extract files matching the container path
    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?.to_path_buf();
        let entry_str = entry_path.to_string_lossy().to_string();

        // Find which volume this entry belongs to
        for (host_path, container_path) in volumes {
            let container_clean = container_path.trim_start_matches('/');
            if entry_str.starts_with(container_clean) {
                let relative = entry_str.strip_prefix(container_clean).unwrap_or(&entry_str);
                let relative = relative.trim_start_matches('/');
                let dest = Path::new(host_path).join(relative);

                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }

                if entry.header().entry_type().is_dir() {
                    std::fs::create_dir_all(&dest)?;
                } else {
                    entry.unpack(&dest)?;
                }
                break;
            }
        }
    }

    Ok(())
}

/// Parse a backup ID into (app_name, timestamp).
/// Backup IDs are formatted as "{app_name}-{timestamp}".
fn parse_backup_id(backup_id: &str) -> anyhow::Result<(String, u64)> {
    // Find the last '-' which separates app name from timestamp
    let last_dash = backup_id
        .rfind('-')
        .ok_or_else(|| anyhow::anyhow!("Invalid backup ID format: '{}'", backup_id))?;

    let app_name = backup_id[..last_dash].to_string();
    let timestamp: u64 = backup_id[last_dash + 1..]
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid timestamp in backup ID: '{}'", backup_id))?;

    Ok((app_name, timestamp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_backup_id() {
        let (app, ts) = parse_backup_id("myapp-1719619200").unwrap();
        assert_eq!(app, "myapp");
        assert_eq!(ts, 1719619200);

        let (app, ts) = parse_backup_id("redis-cache-1719619200").unwrap();
        assert_eq!(app, "redis-cache");
        assert_eq!(ts, 1719619200);

        assert!(parse_backup_id("notimestamp").is_err());
    }
}
