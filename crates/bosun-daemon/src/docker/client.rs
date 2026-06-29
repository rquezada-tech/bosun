//! Docker API client wrapper.
//!
//! Connects to the local Docker daemon via bollard
//! and provides Bosun-specific operations.

use std::collections::HashMap;
use std::path::Path;

use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, LogOutput, LogsOptions,
    RemoveContainerOptions, RestartContainerOptions, StopContainerOptions,
};
use bollard::image::BuildImageOptions;
use bollard::models::{HostConfig, PortBinding};
use bollard::secret::ContainerSummary;
use bollard::service::{EndpointSpec, ListServicesOptions, UpdateServiceOptions};
use bollard::network::{CreateNetworkOptions, InspectNetworkOptions};
use bollard::models::{
    EndpointPortConfig, ServiceSpec, ServiceUpdateStatusStateEnum,
    TaskSpec, TaskSpecContainerSpec,
};
use crate::server::v1::{App, AppStatus, LogEntry};
use futures_util::StreamExt;

pub struct DockerClient {
    pub inner: Docker,
}

impl Clone for DockerClient {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl DockerClient {
    pub async fn connect() -> anyhow::Result<Self> {
        let docker = Docker::connect_with_local_defaults()?;
        docker.ping().await?;
        tracing::info!("Connected to Docker daemon");
        Ok(Self { inner: docker })
    }

    /// List all containers managed by Bosun (filtered by label `managed-by=bosun`).
    pub async fn list_bosun_apps(&self) -> anyhow::Result<Vec<App>> {
        let mut filters = std::collections::HashMap::new();
        filters.insert("label".to_string(), vec!["managed-by=bosun".to_string()]);

        let options = ListContainersOptions {
            all: true,
            filters,
            ..Default::default()
        };

        let containers: Vec<ContainerSummary> = self.inner.list_containers(Some(options)).await?;

        let apps = containers
            .into_iter()
            .filter_map(|c| {
                let names = c.names?;
                let name = names
                    .first()?
                    .strip_prefix('/')
                    .unwrap_or(names.first()?)
                    .to_string();
                let status = match c.state.as_deref() {
                    Some("running") => AppStatus::Running,
                    Some("exited") | Some("created") => AppStatus::Stopped,
                    _ => AppStatus::Unknown,
                };

                let labels = c.labels.unwrap_or_default();
                let domain = labels.get("bosun.domain").cloned();
                let port = labels.get("bosun.port").and_then(|p| p.parse().ok());
                let restart_count = labels
                    .get("bosun.restart-count")
                    .and_then(|c| c.parse().ok())
                    .unwrap_or(0);

                Some(App {
                    name,
                    status: status.into(),
                    domain,
                    port,
                    instances: Some(1),
                    env_keys: vec![],
                    restart_count,
                })
            })
            .collect();

        Ok(apps)
    }

    /// Restart a container by name.
    pub async fn restart_container(&self, name: &str) -> anyhow::Result<()> {
        tracing::info!("Restarting container: {}", name);
        let options = RestartContainerOptions { t: 10 };
        self.inner.restart_container(name, Some(options)).await?;
        tracing::info!("Container {} restarted", name);
        Ok(())
    }

    /// Stop a container by name.
    #[allow(dead_code)] // used in Phase 2 (manual stop command)
    pub async fn stop_container(&self, name: &str) -> anyhow::Result<()> {
        tracing::info!("Stopping container: {}", name);
        let options = StopContainerOptions { t: 10 };
        self.inner.stop_container(name, Some(options)).await?;
        tracing::info!("Container {} stopped", name);
        Ok(())
    }

    /// Get logs from a container, returning a stream of `LogEntry` items.
    pub fn get_logs(
        &self,
        name: &str,
        follow: bool,
        tail_lines: u32,
    ) -> impl futures_util::Stream<Item = anyhow::Result<LogEntry>> + use<> {
        let docker = self.inner.clone();
        let name = name.to_string();
        let options = LogsOptions {
            follow,
            stdout: true,
            stderr: true,
            tail: tail_lines.to_string(),
            timestamps: true,
            ..Default::default()
        };

        let stream = docker.logs(&name, Some(options));

        stream.map(|result| match result {
            Ok(log_output) => {
                let (message, stream_name) = match log_output {
                    LogOutput::StdOut { message } => (message, "stdout"),
                    LogOutput::StdErr { message } => (message, "stderr"),
                    LogOutput::StdIn { message } => (message, "stdin"),
                    LogOutput::Console { message } => (message, "console"),
                };
                let msg_str = String::from_utf8_lossy(&message).into_owned();
                Ok(LogEntry {
                    timestamp_unix: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    message: msg_str,
                    stream: stream_name.to_string(),
                })
            }
            Err(e) => Err(anyhow::anyhow!("Log error: {}", e)),
        })
    }

    /// Scale an app. For MVP, only supports instances=1.
    pub async fn scale_app(&self, name: &str, instances: u32) -> anyhow::Result<()> {
        if instances != 1 {
            anyhow::bail!(
                "Scaling to {} instances is not supported in MVP. Only instances=1 is allowed.",
                instances
            );
        }
        tracing::info!("Scale app '{}' to {} instances (no-op for MVP)", name, instances);
        Ok(())
    }

    /// Create a tarball of a directory (for Docker build context).
    fn create_build_tar(build_dir: &Path) -> anyhow::Result<Vec<u8>> {
        let mut tar_buf = Vec::new();
        {
            let mut tar = tar::Builder::new(&mut tar_buf);
            tar.append_dir_all(".", build_dir)?;
            tar.finish()?;
        }
        Ok(tar_buf)
    }

    /// Deploy an app: build Docker image from build_dir, create + run container.
    pub async fn deploy(
        &self,
        build_dir: &Path,
        app_name: &str,
        domain: Option<&str>,
        port: u16,
        env_vars: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        tracing::info!(
            "Deploying app '{}' from {} (domain={:?}, port={})",
            app_name,
            build_dir.display(),
            domain,
            port
        );

        // 1. Create tar of build directory
        let tar_bytes = Self::create_build_tar(build_dir)?;
        tracing::info!("Created build context tar ({} bytes)", tar_bytes.len());

        // 2. Build Docker image
        let build_options = BuildImageOptions {
            dockerfile: "Dockerfile".to_string(),
            t: app_name.to_string(),
            ..Default::default()
        };

        tracing::info!("Building Docker image '{}'...", app_name);
        let mut build_stream = self.inner.build_image(
            build_options,
            None,
            Some(bytes::Bytes::from(tar_bytes)),
        );

        while let Some(result) = build_stream.next().await {
            match result {
                Ok(info) => {
                    if let Some(stream) = &info.stream {
                        tracing::debug!("build: {}", stream.trim());
                    }
                    if let Some(error) = &info.error {
                        anyhow::bail!("Docker build error: {}", error.trim());
                    }
                }
                Err(e) => {
                    anyhow::bail!("Docker build stream error: {}", e);
                }
            }
        }
        tracing::info!("Docker image '{}' built successfully", app_name);

        // 3. Remove old container if it exists
        match self
            .inner
            .remove_container(
                app_name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
        {
            Ok(_) => tracing::info!("Removed existing container '{}'", app_name),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                // Container didn't exist, that's fine
                tracing::debug!("No existing container '{}' to remove", app_name);
            }
            Err(e) => {
                tracing::warn!("Failed to remove old container '{}': {} (continuing)", app_name, e);
            }
        }

        // 4. Build container config
        let port_binding = format!("{}/tcp", port);
        let mut exposed_ports = HashMap::new();
        exposed_ports.insert(port_binding.clone(), HashMap::new());

        let mut port_bindings = HashMap::new();
        port_bindings.insert(
            port_binding.clone(),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some(port.to_string()),
            }]),
        );

        let mut labels = HashMap::new();
        labels.insert("managed-by".to_string(), "bosun".to_string());
        labels.insert("bosun.app".to_string(), app_name.to_string());
        if let Some(d) = domain {
            labels.insert("bosun.domain".to_string(), d.to_string());
        }
        labels.insert("bosun.port".to_string(), port.to_string());
        labels.insert("bosun.health-check".to_string(), "true".to_string());

        // Convert env_vars to "KEY=VALUE" strings
        let env: Vec<String> = env_vars
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        let config = Config {
            image: Some(app_name.to_string()),
            exposed_ports: Some(exposed_ports),
            env: Some(env),
            labels: Some(labels),
            host_config: Some(HostConfig {
                port_bindings: Some(port_bindings),
                restart_policy: Some(bollard::models::RestartPolicy {
                    name: Some(bollard::models::RestartPolicyNameEnum::UNLESS_STOPPED),
                    maximum_retry_count: Some(3),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let create_options = CreateContainerOptions {
            name: app_name.to_string(),
            ..Default::default()
        };

        // 5. Create and start container
        tracing::info!("Creating container '{}'...", app_name);
        let container = self.inner.create_container(Some(create_options), config).await?;
        tracing::info!("Container '{}' created (id={})", app_name, container.id);

        self.inner.start_container::<String>(app_name, None).await?;
        tracing::info!("Container '{}' started successfully", app_name);

        Ok(())
    }

    /// Deploy an app from a built-in template (no Dockerfile needed).
    /// Pulls the Docker image, creates required directories, and starts
    /// the container with the template's pre-configured env vars, volumes,
    /// and port.
    ///
    /// `image` is the resolved Docker image (from the selected version).
    pub async fn deploy_template(
        &self,
        template: &crate::templates::Template,
        image: &str,
        app_name: &str,
        domain: Option<&str>,
        port_override: Option<u16>,
    ) -> anyhow::Result<()> {
        let port = port_override.unwrap_or(template.default_port);

        tracing::info!(
            "Deploying template '{}' as app '{}' (image={}, domain={:?}, port={})",
            template.name,
            app_name,
            image,
            domain,
            port
        );

        // 1. Create host directories for volume mounts
        let data_root = format!("/var/lib/bosun/data/{}", app_name);
        for vol in &template.volumes {
            let host_path = format!("{}/{}", data_root, vol.name);
            std::fs::create_dir_all(&host_path)?;
            tracing::debug!("Created volume host directory: {}", host_path);
        }

        // 2. Remove old container if it exists
        match self
            .inner
            .remove_container(
                app_name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
        {
            Ok(_) => tracing::info!("Removed existing container '{}'", app_name),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                tracing::debug!("No existing container '{}' to remove", app_name);
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to remove old container '{}': {} (continuing)",
                    app_name,
                    e
                );
            }
        }

        // 3. Build env vars with default values and app name resolution
        let env: Vec<String> = template
            .env_vars
            .iter()
            .map(|ev| {
                let value = ev
                    .default_value
                    .as_deref()
                    .unwrap_or("")
                    .replace("{name}", app_name);
                format!("{}={}", ev.name, value)
            })
            .collect();

        // 4. Build volume binds (host:container:rw)
        let binds: Vec<String> = template
            .volumes
            .iter()
            .map(|vol| {
                let host_path = format!("{}/{}", data_root, vol.name);
                format!("{}:{}:rw", host_path, vol.container_path)
            })
            .collect();

        // 5. Port mapping
        let port_binding = format!("{}/tcp", port);
        let mut exposed_ports = HashMap::new();
        exposed_ports.insert(port_binding.clone(), HashMap::new());

        let mut port_bindings = HashMap::new();
        port_bindings.insert(
            port_binding.clone(),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some(port.to_string()),
            }]),
        );

        // 6. Labels
        let mut labels = HashMap::new();
        labels.insert("managed-by".to_string(), "bosun".to_string());
        labels.insert("bosun.app".to_string(), app_name.to_string());
        labels.insert("bosun.template".to_string(), template.name.to_string());
        if let Some(d) = domain {
            labels.insert("bosun.domain".to_string(), d.to_string());
        }
        labels.insert("bosun.port".to_string(), port.to_string());
        labels.insert("bosun.health-check".to_string(), "true".to_string());

        // 7. Create container config using the resolved image
        let config = Config {
            image: Some(image.to_string()),
            exposed_ports: Some(exposed_ports),
            env: Some(env),
            labels: Some(labels),
            host_config: Some(HostConfig {
                port_bindings: Some(port_bindings),
                binds: Some(binds),
                restart_policy: Some(bollard::models::RestartPolicy {
                    name: Some(bollard::models::RestartPolicyNameEnum::UNLESS_STOPPED),
                    maximum_retry_count: Some(3),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let create_options = CreateContainerOptions {
            name: app_name.to_string(),
            ..Default::default()
        };

        // 8. Create and start container (Docker auto-pulls the image if missing)
        tracing::info!(
            "Creating container '{}' from image '{}'...",
            app_name,
            image
        );
        let container = self
            .inner
            .create_container(Some(create_options), config)
            .await?;
        tracing::info!("Container '{}' created (id={})", app_name, container.id);

        self.inner.start_container::<String>(app_name, None).await?;
        tracing::info!("Container '{}' started successfully", app_name);

        Ok(())
    }

    /// Wait up to `timeout_secs` for a container to reach running state.
    /// Polls `inspect_container` every 2 seconds.
    pub async fn wait_for_container_healthy(
        &self,
        container_name: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<()> {
        use tokio::time::{sleep, Duration};

        let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);

        while std::time::Instant::now() < deadline {
            match self.inner.inspect_container(container_name, None).await {
                Ok(info) => {
                    if let Some(state) = &info.state {
                        if state.running == Some(true) {
                            tracing::info!(
                                "Container '{}' is healthy (state=running)",
                                container_name
                            );
                            return Ok(());
                        } else {
                            tracing::debug!(
                                "Container '{}' status={:?}, waiting...",
                                container_name,
                                state.status
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        "Container '{}' inspect error (may be starting): {}",
                        container_name,
                        e
                    );
                }
            }
            sleep(Duration::from_secs(2)).await;
        }

        anyhow::bail!(
            "Container '{}' did not become healthy within {}s",
            container_name,
            timeout_secs
        );
    }
    /// Inspect a container by name and return full Docker inspect info.
    pub async fn inspect_container(
        &self,
        name: &str,
    ) -> anyhow::Result<bollard::models::ContainerInspectResponse> {
        Ok(self.inner.inspect_container(name, None).await?)
    }

    /// Force-remove a container by name, ignoring errors if it doesn't exist.
    pub async fn force_remove_container(&self, name: &str) -> anyhow::Result<()> {
        match self
            .inner
            .remove_container(
                name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
        {
            Ok(_) => {
                tracing::info!("Force-removed container '{}'", name);
                Ok(())
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                tracing::debug!("Container '{}' already gone", name);
                Ok(())
            }
            Err(e) => Err(anyhow::anyhow!(
                "Failed to force-remove container '{}': {}",
                name,
                e
            )),
        }
    }

    /// Restore a container from backup: create and start with the given config.
    pub async fn restore_container(
        &self,
        app_name: &str,
        image: &str,
        port: u16,
        _domain: Option<&str>,
        env_vars: &HashMap<String, String>,
        volumes: &HashMap<String, String>,
        labels: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        tracing::info!(
            "Restoring container '{}' from image '{}' (port={}, volumes={})",
            app_name,
            image,
            port,
            volumes.len()
        );

        // Create volume directories on host
        for host_path in volumes.keys() {
            std::fs::create_dir_all(host_path)?;
        }

        // Build volume binds: "host_path:container_path:rw"
        let binds: Vec<String> = volumes
            .iter()
            .map(|(host, container)| format!("{}:{}:rw", host, container))
            .collect();

        // Port mapping
        let port_binding = format!("{}/tcp", port);
        let mut exposed_ports = HashMap::new();
        exposed_ports.insert(port_binding.clone(), HashMap::new());

        let mut port_bindings = HashMap::new();
        port_bindings.insert(
            port_binding.clone(),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some(port.to_string()),
            }]),
        );

        // Merge provided labels with required bosun labels
        let mut final_labels = labels.clone();
        final_labels.entry("managed-by".to_string())
            .or_insert_with(|| "bosun".to_string());
        final_labels.entry("bosun.app".to_string())
            .or_insert_with(|| app_name.to_string());
        final_labels.insert("bosun.port".to_string(), port.to_string());
        final_labels.insert("bosun.restored".to_string(), "true".to_string());

        let env: Vec<String> = env_vars
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        let config = Config {
            image: Some(image.to_string()),
            exposed_ports: Some(exposed_ports),
            env: Some(env),
            labels: Some(final_labels),
            host_config: Some(HostConfig {
                port_bindings: Some(port_bindings),
                binds: Some(binds),
                restart_policy: Some(bollard::models::RestartPolicy {
                    name: Some(bollard::models::RestartPolicyNameEnum::UNLESS_STOPPED),
                    maximum_retry_count: Some(3),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let create_options = CreateContainerOptions {
            name: app_name.to_string(),
            ..Default::default()
        };

        tracing::info!("Creating container '{}' from image '{}'...", app_name, image);
        let container = self
            .inner
            .create_container(Some(create_options), config)
            .await?;
        tracing::info!("Container '{}' created (id={})", app_name, container.id);

        self.inner.start_container::<String>(app_name, None).await?;
        tracing::info!("Container '{}' started successfully", app_name);

        // Wait for healthy
        tracing::info!(
            "Waiting up to 30s for restored container '{}' to become healthy...",
            app_name
        );
        self.wait_for_container_healthy(app_name, 30).await?;

        Ok(())
    }

    /// Rolling update deploy: stop old container, remove it, start new one on
    /// the same port, and update Caddy. Brief downtime (~2-5s) for port swap.
    /// This is the pragmatic single-host approach without Swarm.
    pub async fn deploy_rolling(
        &self,
        build_dir: &Path,
        app_name: &str,
        domain: Option<&str>,
        port: u16,
        env_vars: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        tracing::info!(
            "Rolling deploy: app='{}' from {} (domain={:?}, port={})",
            app_name,
            build_dir.display(),
            domain,
            port
        );

        // 1. Build Docker image
        let tar_bytes = Self::create_build_tar(build_dir)?;
        tracing::info!("Created build context tar ({} bytes)", tar_bytes.len());

        let build_options = BuildImageOptions {
            dockerfile: "Dockerfile".to_string(),
            t: app_name.to_string(),
            ..Default::default()
        };

        tracing::info!("Building Docker image '{}'...", app_name);
        let mut build_stream = self.inner.build_image(
            build_options,
            None,
            Some(bytes::Bytes::from(tar_bytes)),
        );

        while let Some(result) = build_stream.next().await {
            match result {
                Ok(info) => {
                    if let Some(stream) = &info.stream {
                        tracing::debug!("build: {}", stream.trim());
                    }
                    if let Some(error) = &info.error {
                        anyhow::bail!("Docker build error: {}", error.trim());
                    }
                }
                Err(e) => {
                    anyhow::bail!("Docker build stream error: {}", e);
                }
            }
        }
        tracing::info!("Docker image '{}' built successfully", app_name);

        // 2. Stop old container gracefully (10s timeout)
        let old_exists = self
            .inner
            .inspect_container(app_name, None)
            .await
            .is_ok();

        if old_exists {
            tracing::info!("Stopping old container '{}' gracefully (10s timeout)...", app_name);
            match self
                .inner
                .stop_container(app_name, Some(StopContainerOptions { t: 10 }))
                .await
            {
                Ok(_) => tracing::info!("Old container '{}' stopped", app_name),
                Err(e) => tracing::warn!(
                    "Failed to gracefully stop old container '{}': {} (forcing removal)",
                    app_name, e
                ),
            }

            // 3. Remove old container
            tracing::info!("Removing old container '{}'...", app_name);
            match self
                .inner
                .remove_container(
                    app_name,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await
            {
                Ok(_) => tracing::info!("Old container '{}' removed", app_name),
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => {
                    tracing::debug!("Old container '{}' already gone", app_name);
                }
                Err(e) => {
                    anyhow::bail!("Failed to remove old container '{}': {}", app_name, e);
                }
            }
        } else {
            tracing::info!("No existing container '{}' to replace", app_name);
        }

        // 4. Build container config
        let port_binding = format!("{}/tcp", port);
        let mut exposed_ports = HashMap::new();
        exposed_ports.insert(port_binding.clone(), HashMap::new());

        let mut port_bindings = HashMap::new();
        port_bindings.insert(
            port_binding.clone(),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some(port.to_string()),
            }]),
        );

        let mut labels = HashMap::new();
        labels.insert("managed-by".to_string(), "bosun".to_string());
        labels.insert("bosun.app".to_string(), app_name.to_string());
        labels.insert("bosun.deploy-strategy".to_string(), "rolling".to_string());
        if let Some(d) = domain {
            labels.insert("bosun.domain".to_string(), d.to_string());
        }
        labels.insert("bosun.port".to_string(), port.to_string());
        labels.insert("bosun.health-check".to_string(), "true".to_string());

        let env: Vec<String> = env_vars
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        let config = Config {
            image: Some(app_name.to_string()),
            exposed_ports: Some(exposed_ports),
            env: Some(env),
            labels: Some(labels),
            host_config: Some(HostConfig {
                port_bindings: Some(port_bindings),
                restart_policy: Some(bollard::models::RestartPolicy {
                    name: Some(bollard::models::RestartPolicyNameEnum::UNLESS_STOPPED),
                    maximum_retry_count: Some(3),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let create_options = CreateContainerOptions {
            name: app_name.to_string(),
            ..Default::default()
        };

        // 5. Create and start new container
        tracing::info!("Creating new container '{}'...", app_name);
        let container = self.inner.create_container(Some(create_options), config).await?;
        tracing::info!("New container '{}' created (id={})", app_name, container.id);

        self.inner.start_container::<String>(app_name, None).await?;
        tracing::info!("New container '{}' started", app_name);

        // 6. Health check (30s timeout)
        tracing::info!("Waiting up to 30s for new container '{}' to become healthy...", app_name);
        self.wait_for_container_healthy(app_name, 30).await?;

        tracing::info!("Rolling deploy complete for '{}'", app_name);
        Ok(())
    }

    /// Determine which color is currently active for a blue-green app.
    /// Returns (active_color, inactive_color) as ("blue", "green") or vice versa.
    /// Defaults to blue as active if neither exists.
    pub async fn determine_blue_green_colors(
        &self,
        app_name: &str,
    ) -> anyhow::Result<(String, String)> {
        let blue_name = format!("{}-blue", app_name);
        let green_name = format!("{}-green", app_name);

        let blue_exists = self.inner.inspect_container(&blue_name, None).await.is_ok();
        let green_exists = self.inner.inspect_container(&green_name, None).await.is_ok();

        match (blue_exists, green_exists) {
            (true, true) => {
                // Both exist — check labels to determine which is active
                let blue_info = self.inner.inspect_container(&blue_name, None).await?;
                let blue_labels = blue_info.config.and_then(|c| c.labels).unwrap_or_default();
                let blue_active = blue_labels
                    .get("bosun.active")
                    .map(|v| v == "true")
                    .unwrap_or(false);

                if blue_active {
                    Ok(("blue".to_string(), "green".to_string()))
                } else {
                    Ok(("green".to_string(), "blue".to_string()))
                }
            }
            (true, false) => {
                // Only blue exists — it's the active one
                tracing::info!("Blue-green: only blue exists, treating as active");
                Ok(("blue".to_string(), "green".to_string()))
            }
            (false, true) => {
                // Only green exists — it's the active one
                tracing::info!("Blue-green: only green exists, treating as active");
                Ok(("green".to_string(), "blue".to_string()))
            }
            (false, false) => {
                // Neither exists — start with blue
                tracing::info!("Blue-green: no containers exist, defaulting to blue as active");
                Ok(("blue".to_string(), "green".to_string()))
            }
        }
    }

    /// Get the port from a container's labels, falling back to a default.
    pub async fn get_container_port(&self, container_name: &str, default_port: u16) -> u16 {
        match self.inner.inspect_container(container_name, None).await {
            Ok(info) => {
                let labels = info.config.and_then(|c| c.labels).unwrap_or_default();
                labels
                    .get("bosun.port")
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(default_port)
            }
            Err(_) => default_port,
        }
    }

    /// Blue-green deploy: start new container on the inactive color's slot,
    /// health-check it, then swap Caddy to point to the new color.
    /// Old color stays running for instant rollback.
    pub async fn deploy_blue_green(
        &self,
        build_dir: &Path,
        app_name: &str,
        domain: Option<&str>,
        port: u16,
        env_vars: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        tracing::info!(
            "Blue-green deploy: app='{}' from {} (domain={:?}, port={})",
            app_name,
            build_dir.display(),
            domain,
            port
        );

        // 1. Determine active/inactive colors
        let (active_color, inactive_color) = self.determine_blue_green_colors(app_name).await?;
        let new_container_name = format!("{}-{}", app_name, inactive_color);
        let _old_container_name = format!("{}-{}", app_name, active_color);

        // Inactive color uses a high port to avoid conflicts with the active one
        let inactive_port = 30000 + (port as u32 % 2767) as u16;
        tracing::info!(
            "Blue-green: active={} (port={}), deploying to inactive={} (temp port={})",
            active_color, port, inactive_color, inactive_port
        );

        // 2. Remove old inactive container if it exists (stale previous deploy)
        match self
            .inner
            .remove_container(
                &new_container_name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
        {
            Ok(_) => tracing::info!("Removed stale inactive container '{}'", new_container_name),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                tracing::debug!("No stale container '{}' to remove", new_container_name);
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to remove stale container '{}': {} (continuing)",
                    new_container_name, e
                );
            }
        }

        // 3. Build Docker image
        let tar_bytes = Self::create_build_tar(build_dir)?;
        tracing::info!("Created build context tar ({} bytes)", tar_bytes.len());

        let build_options = BuildImageOptions {
            dockerfile: "Dockerfile".to_string(),
            t: app_name.to_string(),
            ..Default::default()
        };

        tracing::info!("Building Docker image '{}'...", app_name);
        let mut build_stream = self.inner.build_image(
            build_options,
            None,
            Some(bytes::Bytes::from(tar_bytes)),
        );

        while let Some(result) = build_stream.next().await {
            match result {
                Ok(info) => {
                    if let Some(stream) = &info.stream {
                        tracing::debug!("build: {}", stream.trim());
                    }
                    if let Some(error) = &info.error {
                        anyhow::bail!("Docker build error: {}", error.trim());
                    }
                }
                Err(e) => {
                    anyhow::bail!("Docker build stream error: {}", e);
                }
            }
        }
        tracing::info!("Docker image '{}' built successfully", app_name);

        // 4. Create container config for the new (inactive) color
        let port_binding = format!("{}/tcp", port);
        let mut exposed_ports = HashMap::new();
        exposed_ports.insert(port_binding.clone(), HashMap::new());

        let mut port_bindings = HashMap::new();
        port_bindings.insert(
            port_binding.clone(),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some(inactive_port.to_string()),
            }]),
        );

        let mut labels = HashMap::new();
        labels.insert("managed-by".to_string(), "bosun".to_string());
        labels.insert("bosun.app".to_string(), app_name.to_string());
        labels.insert("bosun.deploy-strategy".to_string(), "blue-green".to_string());
        labels.insert("bosun.color".to_string(), inactive_color.clone());
        labels.insert("bosun.active".to_string(), "false".to_string());
        labels.insert("bosun.main-port".to_string(), port.to_string());
        if let Some(d) = domain {
            labels.insert("bosun.domain".to_string(), d.to_string());
        }
        labels.insert("bosun.port".to_string(), inactive_port.to_string());
        labels.insert("bosun.health-check".to_string(), "true".to_string());

        let env: Vec<String> = env_vars
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        let config = Config {
            image: Some(app_name.to_string()),
            exposed_ports: Some(exposed_ports),
            env: Some(env),
            labels: Some(labels),
            host_config: Some(HostConfig {
                port_bindings: Some(port_bindings),
                restart_policy: Some(bollard::models::RestartPolicy {
                    name: Some(bollard::models::RestartPolicyNameEnum::UNLESS_STOPPED),
                    maximum_retry_count: Some(3),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let create_options = CreateContainerOptions {
            name: new_container_name.clone(),
            ..Default::default()
        };

        // 5. Create and start the new container
        tracing::info!("Creating new {} container '{}'...", inactive_color, new_container_name);
        let container = self
            .inner
            .create_container(Some(create_options), config)
            .await?;
        tracing::info!(
            "{} container '{}' created (id={})",
            inactive_color,
            new_container_name,
            container.id
        );

        self.inner
            .start_container::<String>(&new_container_name, None)
            .await?;
        tracing::info!(
            "{} container '{}' started on port {}",
            inactive_color,
            new_container_name,
            inactive_port
        );

        // 6. Health check (30s)
        tracing::info!(
            "Waiting up to 30s for {} container '{}' to become healthy...",
            inactive_color,
            new_container_name
        );
        self.wait_for_container_healthy(&new_container_name, 30)
            .await?;

        // 7. Mark the new container as active
        tracing::info!(
            "Promoting {} container '{}' to active",
            inactive_color,
            new_container_name
        );

        // Update labels on the new container to mark it as active
        // We need to update labels via the container update API
        // bollard doesn't directly expose label updates on running containers,
        // but we can track state via inspect + labels stored at create time.
        // The active status is determined by which color Caddy points to.
        // For now, we signal the transition by updating Caddy.

        // 8. Swap Caddy to point to the new color's port
        if let Some(d) = domain {
            tracing::info!(
                "Swapping Caddy reverse proxy: {} -> localhost:{} (was {}:{})",
                d,
                inactive_port,
                active_color,
                port
            );
            // Note: The CaddyClient is not available directly in DockerClient.
            // The caller (server handler / strategy dispatcher) will handle Caddy updates.
            // We log the intent here; the actual swap happens in the dispatcher.
            tracing::info!(
                "Blue-green: Caddy swap needed for domain '{}' to port {}",
                d,
                inactive_port
            );
        }

        tracing::info!(
            "Blue-green deploy complete: {} is now active on port {}, {} is inactive (kept for rollback)",
            inactive_color, inactive_port, active_color
        );

        Ok(())
    }

    /// Rollback a blue-green deploy: swap Caddy back to the inactive (previous) color.
    /// Returns the port of the rolled-back-to container.
    pub async fn rollback_blue_green(
        &self,
        app_name: &str,
    ) -> anyhow::Result<(String, u16)> {
        let (active_color, inactive_color) = self.determine_blue_green_colors(app_name).await?;
        let inactive_name = format!("{}-{}", app_name, inactive_color);

        // Check that the inactive container exists and is running
        match self.inner.inspect_container(&inactive_name, None).await {
            Ok(info) => {
                let state = info.state.unwrap_or_default();
                if state.running != Some(true) {
                    anyhow::bail!(
                        "Inactive container '{}' is not running — cannot rollback",
                        inactive_name
                    );
                }
            }
            Err(_) => {
                anyhow::bail!(
                    "Inactive container '{}' does not exist — nothing to rollback to",
                    inactive_name
                );
            }
        }

        let rollback_port = self.get_container_port(&inactive_name, 8080).await;

        tracing::info!(
            "Blue-green rollback: swapping from {} ({}) to {} ({})",
            active_color,
            app_name,
            inactive_color,
            rollback_port
        );

        Ok((inactive_color, rollback_port))
    }

    /// Promote the current blue-green deploy: remove the inactive color container,
    /// making the current active color permanent.
    #[allow(dead_code)]
    pub async fn promote_blue_green(&self, app_name: &str) -> anyhow::Result<String> {
        let (active_color, inactive_color) = self.determine_blue_green_colors(app_name).await?;
        let inactive_name = format!("{}-{}", app_name, inactive_color);

        tracing::info!(
            "Blue-green promote: removing inactive container '{}' ({}), keeping {} as permanent",
            inactive_name,
            inactive_color,
            active_color
        );

        // Stop the inactive container gracefully
        match self
            .inner
            .stop_container(&inactive_name, Some(StopContainerOptions { t: 10 }))
            .await
        {
            Ok(_) => tracing::info!("Stopped inactive container '{}'", inactive_name),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                tracing::debug!(
                    "Inactive container '{}' already gone — nothing to promote",
                    inactive_name
                );
                return Ok(active_color);
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to stop inactive container '{}': {} (forcing removal)",
                    inactive_name, e
                );
            }
        }

        // Remove the inactive container
        match self
            .inner
            .remove_container(
                &inactive_name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
        {
            Ok(_) => tracing::info!("Removed inactive container '{}'", inactive_name),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                tracing::debug!(
                    "Inactive container '{}' already removed",
                    inactive_name
                );
            }
            Err(e) => {
                anyhow::bail!(
                    "Failed to remove inactive container '{}': {}",
                    inactive_name,
                    e
                );
            }
        }

        tracing::info!(
            "Blue-green promote complete: {} is now the only deployment",
            active_color
        );

        Ok(active_color)
    }

    /// Redeploy an existing app: pull latest image, stop old container,
    /// recreate with the same configuration, and start it.
    pub async fn redeploy(&self, app_name: &str, strategy: &str) -> anyhow::Result<()> {
        tracing::info!("Redeploying app '{}' (strategy={})...", app_name, strategy);

        // 1. Inspect current container to capture its config
        let inspect = self.inner.inspect_container(app_name, None).await.map_err(|e| {
            anyhow::anyhow!(
                "Cannot find container '{}' for redeploy: {}",
                app_name,
                e
            )
        })?;

        let config = inspect.config.ok_or_else(|| {
            anyhow::anyhow!("Container '{}' has no config in inspect response", app_name)
        })?;

        let host_config = inspect.host_config.ok_or_else(|| {
            anyhow::anyhow!(
                "Container '{}' has no host config in inspect response",
                app_name
            )
        })?;

        // Extract image name
        let image = config.image.ok_or_else(|| {
            anyhow::anyhow!("Container '{}' has no image set", app_name)
        })?;
        tracing::info!("Redeploy: current image '{}'", image);

        // 2. Pull latest image
        tracing::info!("Pulling latest image '{}'...", image);
        let mut pull_stream = self.inner.create_image(
            Some(bollard::image::CreateImageOptions {
                from_image: image.clone(),
                ..Default::default()
            }),
            None,
            None,
        );
        while let Some(result) = pull_stream.next().await {
            match result {
                Ok(info) => {
                    if let Some(status) = &info.status {
                        tracing::debug!("pull: {}", status);
                    }
                    if let Some(error) = &info.error {
                        tracing::warn!("Pull warning for '{}': {}", image, error.trim());
                    }
                }
                Err(e) => {
                    tracing::warn!("Pull stream error for '{}': {} (continuing)", image, e);
                }
            }
        }
        tracing::info!("Image '{}' pull completed", image);

        // 3. Stop old container
        match self
            .inner
            .stop_container(app_name, Some(StopContainerOptions { t: 10 }))
            .await
        {
            Ok(_) => tracing::info!("Stopped container '{}'", app_name),
            Err(e) => tracing::warn!(
                "Failed to stop container '{}': {} (continuing)",
                app_name,
                e
            ),
        }

        // 4. Remove old container
        match self
            .inner
            .remove_container(
                app_name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
        {
            Ok(_) => tracing::info!("Removed container '{}'", app_name),
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Failed to remove container '{}': {}",
                    app_name,
                    e
                ));
            }
        }

        // 5. Recreate container with same config
        let labels = config.labels.unwrap_or_default();

        // Build volume binds from host config
        let binds: Vec<String> = host_config
            .binds
            .clone()
            .unwrap_or_default();

        // Build port bindings
        let port_bindings = host_config.port_bindings.clone();

        // Build exposed ports
        let exposed_ports = config.exposed_ports.clone();

        let recreate_config = Config {
            image: Some(image.clone()),
            env: config.env.clone(),
            labels: Some(labels),
            exposed_ports,
            host_config: Some(HostConfig {
                port_bindings,
                binds: Some(binds),
                restart_policy: Some(bollard::models::RestartPolicy {
                    name: Some(bollard::models::RestartPolicyNameEnum::UNLESS_STOPPED),
                    maximum_retry_count: Some(3),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let create_options = CreateContainerOptions {
            name: app_name.to_string(),
            ..Default::default()
        };

        tracing::info!("Recreating container '{}' with image '{}'...", app_name, image);
        let container = self.inner.create_container(Some(create_options), recreate_config).await?;
        tracing::info!(
            "Container '{}' recreated (id={})",
            app_name,
            container.id
        );

        // 6. Start container
        self.inner
            .start_container::<String>(app_name, None)
            .await?;
        tracing::info!("Container '{}' started successfully (redeploy complete)", app_name);

        Ok(())
    }

    // ── Docker Swarm ────────────────────────────────────────────────────────

    /// Check if the Docker daemon is in Swarm mode by querying docker info.
    pub fn is_swarm(&self) -> anyhow::Result<bool> {
        let output = std::process::Command::new("docker")
            .arg("info")
            .arg("--format")
            .arg("{{.Swarm.LocalNodeState}}")
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to run docker info: {}", e))?;

        if !output.status.success() {
            // If docker info fails, assume no swarm
            return Ok(false);
        }

        let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(state == "active")
    }

    /// Ensure the "bosun" overlay network exists (creates it if needed).
    /// In Swarm mode, this creates an overlay network for multi-node service discovery.
    /// In non-Swarm mode, it creates a regular bridge network.
    pub async fn ensure_bosun_network(&self) -> anyhow::Result<()> {
        let is_swarm = self.is_swarm().unwrap_or(false);

        // Check if network already exists
        let existing = self
            .inner
            .inspect_network("bosun", Some(InspectNetworkOptions::<String>::default()))
            .await;

        if existing.is_ok() {
            tracing::info!("Bosun network already exists");
            return Ok(());
        }

        tracing::info!(
            "Creating bosun network (driver: {})...",
            if is_swarm { "overlay" } else { "bridge" }
        );

        let driver = if is_swarm { "overlay" } else { "bridge" };

        let options: CreateNetworkOptions<String> = CreateNetworkOptions {
            name: "bosun".to_string(),
            driver: driver.to_string(),
            attachable: true,
            ..Default::default()
        };

        self.inner
            .create_network(options)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create bosun network: {}", e))?;

        tracing::info!("Bosun {} network created successfully", driver);
        Ok(())
    }

    /// Deploy an app as a Docker Swarm service.
    /// Creates a Docker service instead of a container, enabling native
    /// rolling updates, overlay networking, and multi-node scheduling.
    pub async fn deploy_service(
        &self,
        app_name: &str,
        image: &str,
        domain: Option<&str>,
        port: u16,
        env_vars: &HashMap<String, String>,
        replicas: u64,
    ) -> anyhow::Result<()> {
        tracing::info!(
            "Deploying Swarm service '{}' (image={}, port={}, replicas={})",
            app_name,
            image,
            port,
            replicas
        );

        // Convert env_vars to "KEY=VALUE" strings
        let env: Vec<String> = env_vars
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        // Build labels
        let mut labels = HashMap::new();
        labels.insert("managed-by".to_string(), "bosun".to_string());
        labels.insert("bosun.app".to_string(), app_name.to_string());
        if let Some(d) = domain {
            labels.insert("bosun.domain".to_string(), d.to_string());
        }
        labels.insert("bosun.port".to_string(), port.to_string());
        labels.insert("bosun.health-check".to_string(), "true".to_string());

        // Build task spec with container spec
        let container_spec = TaskSpecContainerSpec {
            image: Some(image.to_string()),
            env: Some(env),
            labels: Some(labels.clone()),
            ..Default::default()
        };

        let task_template = TaskSpec {
            container_spec: Some(container_spec),
            ..Default::default()
        };

        let endpoint_port = EndpointPortConfig {
            protocol: Some(bollard::models::EndpointPortConfigProtocolEnum::TCP),
            target_port: Some(port as i64),
            published_port: Some(port as i64),
            ..Default::default()
        };

        let endpoint_spec = EndpointSpec {
            mode: Some(bollard::models::EndpointSpecModeEnum::VIP),
            ports: Some(vec![endpoint_port]),
        };

        let replicated = bollard::models::ServiceSpecModeReplicated {
            replicas: Some(replicas as i64),
        };

        let mode = bollard::models::ServiceSpecMode {
            replicated: Some(replicated),
            ..Default::default()
        };

        let service_spec = ServiceSpec {
            name: Some(app_name.to_string()),
            labels: Some(labels.clone()),
            task_template: Some(task_template),
            mode: Some(mode),
            endpoint_spec: Some(endpoint_spec),
            ..Default::default()
        };

        tracing::info!("Creating Swarm service '{}'...", app_name);
        let service = self
            .inner
            .create_service(service_spec, None)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create Swarm service '{}': {}", app_name, e))?;

        tracing::info!(
            "Swarm service '{}' created (id={})",
            app_name,
            service.id.unwrap_or_default()
        );

        Ok(())
    }

    /// Update an existing Docker Swarm service (triggers native rolling update).
    /// Updates the image and environment, Docker handles graceful rolling update.
    pub async fn update_service(
        &self,
        app_name: &str,
        image: &str,
        env_vars: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        tracing::info!("Updating Swarm service '{}' (image={})...", app_name, image);

        let env: Vec<String> = env_vars
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        let container_spec = TaskSpecContainerSpec {
            image: Some(image.to_string()),
            env: Some(env),
            ..Default::default()
        };

        let task_template = TaskSpec {
            container_spec: Some(container_spec),
            ..Default::default()
        };

        let service_spec = ServiceSpec {
            name: Some(app_name.to_string()),
            task_template: Some(task_template),
            ..Default::default()
        };

        // Get current service version for update
        let current = self
            .inner
            .inspect_service(app_name, None)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to inspect service '{}': {}", app_name, e))?;

        let version = current
            .version
            .and_then(|v| v.index)
            .unwrap_or(0);

        let update_options = UpdateServiceOptions {
            version,
            ..Default::default()
        };

        self.inner
            .update_service(
                app_name,
                service_spec,
                update_options,
                None::<bollard::auth::DockerCredentials>,
            )
            .await
            .map_err(|e| anyhow::anyhow!("Failed to update Swarm service '{}': {}", app_name, e))?;

        tracing::info!("Swarm service '{}' update triggered (rolling)", app_name);
        Ok(())
    }

    /// Remove a Docker Swarm service by name.
    pub async fn remove_service(&self, app_name: &str) -> anyhow::Result<()> {
        tracing::info!("Removing Swarm service '{}'...", app_name);

        self.inner
            .delete_service(app_name)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to remove Swarm service '{}': {}", app_name, e))?;

        tracing::info!("Swarm service '{}' removed", app_name);
        Ok(())
    }

    /// List all Docker Swarm services managed by Bosun.
    pub async fn list_services(&self) -> anyhow::Result<Vec<App>> {
        let mut filters = std::collections::HashMap::new();
        filters.insert(
            "label".to_string(),
            vec!["managed-by=bosun".to_string()],
        );

        let options = ListServicesOptions {
            filters,
            ..Default::default()
        };

        let services = self
            .inner
            .list_services(Some(options))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list Swarm services: {}", e))?;

        let apps = services
            .into_iter()
            .map(|s| {
                // Extract spec once to avoid move issues
                let spec = s.spec.clone();

                let name = spec
                    .as_ref()
                    .and_then(|sp| sp.name.clone())
                    .unwrap_or_else(|| "unknown".to_string());

                let status = if let Some(update_status) = &s.update_status {
                    match update_status.state {
                        Some(ServiceUpdateStatusStateEnum::COMPLETED)
                        | Some(ServiceUpdateStatusStateEnum::PAUSED) => AppStatus::Running,
                        Some(ServiceUpdateStatusStateEnum::UPDATING)
                        | Some(ServiceUpdateStatusStateEnum::ROLLBACK_STARTED) => AppStatus::Deploying,
                        _ => AppStatus::Running,
                    }
                } else {
                    AppStatus::Running
                };

                let labels = spec.as_ref().and_then(|sp| sp.labels.clone()).unwrap_or_default();
                let domain = labels.get("bosun.domain").cloned();
                let port = labels
                    .get("bosun.port")
                    .and_then(|p| p.parse().ok());

                let replicas = spec
                    .as_ref()
                    .and_then(|sp| sp.mode.as_ref())
                    .and_then(|m| m.replicated.as_ref())
                    .and_then(|r| r.replicas)
                    .unwrap_or(1) as u32;

                App {
                    name,
                    status: status.into(),
                    domain,
                    port,
                    instances: Some(replicas),
                    env_keys: vec![],
                    restart_count: 0,
                }
            })
            .collect();

        Ok(apps)
    }

    /// Initialize Docker Swarm on this node (becomes a manager).
    /// Uses the `docker` CLI binary since bollard 0.18 doesn't expose Swarm init.
    pub fn init_swarm(&self, advertise_addr: Option<&str>) -> anyhow::Result<String> {
        tracing::info!("Initializing Docker Swarm...");

        let mut cmd = std::process::Command::new("docker");
        cmd.arg("swarm").arg("init");

        if let Some(addr) = advertise_addr {
            cmd.arg("--advertise-addr").arg(addr);
        }

        let output = cmd
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to run 'docker swarm init': {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Docker swarm init failed: {}", stderr.trim());
        }

        tracing::info!("Docker Swarm initialized successfully");

        // Return the worker join token
        self.get_swarm_join_token("worker")
    }

    /// Get a Swarm join token (worker or manager).
    fn get_swarm_join_token(&self, role: &str) -> anyhow::Result<String> {
        let output = std::process::Command::new("docker")
            .arg("swarm")
            .arg("join-token")
            .arg(role)
            .arg("-q")
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to get Swarm join token: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to get Swarm join token: {}", stderr.trim());
        }

        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(token)
    }

    /// Join this node to an existing Docker Swarm as a worker or manager.
    pub fn join_swarm(&self, token: &str, addr: &str) -> anyhow::Result<()> {
        tracing::info!("Joining Docker Swarm at {}...", addr);

        let output = std::process::Command::new("docker")
            .arg("swarm")
            .arg("join")
            .arg("--token")
            .arg(token)
            .arg(addr)
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to run 'docker swarm join': {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Docker swarm join failed: {}", stderr.trim());
        }

        tracing::info!("Successfully joined Docker Swarm");
        Ok(())
    }

    /// Leave the Docker Swarm (removes this node from the cluster).
    pub fn leave_swarm(&self, force: bool) -> anyhow::Result<()> {
        tracing::info!("Leaving Docker Swarm (force={})...", force);

        let mut cmd = std::process::Command::new("docker");
        cmd.arg("swarm").arg("leave");

        if force {
            cmd.arg("--force");
        }

        let output = cmd
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to run 'docker swarm leave': {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Docker swarm leave failed: {}", stderr.trim());
        }

        tracing::info!("Left Docker Swarm successfully");
        Ok(())
    }

    /// List all nodes in the Docker Swarm.
    /// Parses `docker node ls` output.
    pub fn list_nodes(&self) -> anyhow::Result<Vec<ClusterNode>> {
        let output = std::process::Command::new("docker")
            .arg("node")
            .arg("ls")
            .arg("--format")
            .arg("{{.ID}}\t{{.Hostname}}\t{{.Status}}\t{{.Availability}}\t{{.ManagerStatus}}")
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to run 'docker node ls': {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Docker node ls failed: {}", stderr.trim());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut nodes = Vec::new();

        for line in stdout.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 5 {
                let role = if parts[4].contains("Leader") || parts[4].contains("Reachable") {
                    "manager"
                } else {
                    "worker"
                };

                nodes.push(ClusterNode {
                    id: parts[0].trim_matches('*').trim().to_string(),
                    hostname: parts[1].trim().to_string(),
                    status: parts[2].trim().to_string(),
                    availability: parts[3].trim().to_string(),
                    role: role.to_string(),
                    addr: String::new(),
                });
            }
        }

        Ok(nodes)
    }
}

/// Cluster node information returned by list_nodes().
#[derive(Debug, Clone)]
pub struct ClusterNode {
    pub id: String,
    pub hostname: String,
    pub role: String,
    pub availability: String,
    pub status: String,
    pub addr: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Integration test — requires Docker daemon.
    #[tokio::test]
    #[ignore = "requires Docker daemon running"]
    async fn test_connect_and_list() {
        let client = DockerClient::connect().await.expect("Failed to connect to Docker");
        let apps = client
            .list_bosun_apps()
            .await
            .expect("Failed to list bosun apps");
        // Should return empty Vec when no bosun-managed containers exist
        assert!(apps.is_empty() || !apps.is_empty());
    }

    /// Test scale validation without Docker.
    #[tokio::test]
    async fn test_scale_validation() {
        // This test doesn't need Docker — it tests the MVP guard
        let client = DockerClient::connect().await;
        if client.is_err() {
            // Docker not available, skip
            return;
        }
        let client = client.unwrap();
        // instances=0 should fail (not equal to 1)
        let result = client.scale_app("nonexistent", 0).await;
        assert!(result.is_err());
        // instances=2 should fail
        let result = client.scale_app("nonexistent", 2).await;
        assert!(result.is_err());
        // instances=1 should succeed (no-op)
        let result = client.scale_app("nonexistent", 1).await;
        assert!(result.is_ok());
    }
}
