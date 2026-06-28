//! Container health checker daemon.
//!
//! Periodically inspects Bosun-managed containers and auto-restarts
//! any that have exited unexpectedly, with rate-limiting to prevent
//! restart storms.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::docker::DockerClient;

/// Restart counts shared between the health checker and gRPC service.
pub type RestartCounts = Arc<Mutex<HashMap<String, u32>>>;

/// Periodically checks Bosun-managed containers and auto-restarts
/// unhealthy ones with rate-limiting.
pub struct HealthChecker {
    docker: Arc<Mutex<DockerClient>>,
    interval_secs: u64,
    /// Shared restart-count map (exposed for gRPC to read).
    pub restart_counts: RestartCounts,
}

impl HealthChecker {
    /// Create a new health checker that polls every `interval_secs`.
    pub fn new(docker: Arc<Mutex<DockerClient>>, interval_secs: u64) -> Self {
        Self {
            docker,
            interval_secs,
            restart_counts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Start the health checker daemon (spawns a background task).
    pub fn start(&self) {
        let docker = self.docker.clone();
        let counts = self.restart_counts.clone();
        let interval_secs = self.interval_secs;

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;

                let client = docker.lock().await;
                let apps = match client.list_bosun_apps().await {
                    Ok(apps) => apps,
                    Err(e) => {
                        tracing::warn!("Health checker: failed to list apps: {e}");
                        continue;
                    }
                };
                drop(client);

                for app in &apps {
                    // Only check running or stopped apps
                    let docker = docker.lock().await;
                    match docker.inspect_container(&app.name).await {
                        Ok(info) => {
                            let running = info
                                .state
                                .as_ref()
                                .and_then(|s| s.running)
                                .unwrap_or(false);

                            if !running {
                                let mut counts = counts.lock().await;
                                let count = counts.entry(app.name.clone()).or_insert(0);

                                // Rate limit: max 3 restarts per app
                                if *count < 3 {
                                    tracing::warn!(
                                        "Health checker: restarting container '{}' (attempt {})",
                                        app.name,
                                        *count + 1
                                    );
                                    *count += 1;
                                    drop(counts);

                                    // Restart the container
                                    if let Err(e) = docker.restart_container(&app.name).await {
                                        tracing::error!(
                                            "Health checker: failed to restart '{}': {e}",
                                            app.name
                                        );
                                    }
                                } else {
                                    tracing::error!(
                                        "Health checker: container '{}' has been restarted {} times — giving up",
                                        app.name,
                                        *count
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::debug!(
                                "Health checker: inspect failed for '{}': {e}",
                                app.name
                            );
                        }
                    }
                }
            }
        });
    }
}
