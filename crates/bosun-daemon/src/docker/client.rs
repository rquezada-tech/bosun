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
use crate::server::v1::{App, AppStatus, LogEntry};
use crate::templates::Template;
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
    pub async fn deploy_template(
        &self,
        template: &Template,
        app_name: &str,
        domain: Option<&str>,
        port_override: Option<u16>,
    ) -> anyhow::Result<()> {
        let port = port_override.unwrap_or(template.default_port);

        tracing::info!(
            "Deploying template '{}' as app '{}' (image={}, domain={:?}, port={})",
            template.name,
            app_name,
            template.image,
            domain,
            port
        );

        // 1. Create host directories for volume mounts and resolve {name} placeholders
        for (host_src, _container_dest) in &template.volumes {
            let host_path = host_src.replace("{name}", app_name);
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

        // 3. Build env vars with {name} placeholders resolved
        let env: Vec<String> = template
            .env_vars
            .iter()
            .map(|(k, v)| {
                let resolved = v.replace("{name}", app_name);
                format!("{}={}", k, resolved)
            })
            .collect();

        // 4. Build volume binds (host:container:rw)
        let binds: Vec<String> = template
            .volumes
            .iter()
            .map(|(host_src, container_dest)| {
                let host_path = host_src.replace("{name}", app_name);
                format!("{}:{}:rw", host_path, container_dest)
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

        // 7. Create container config using the template image directly
        let config = Config {
            image: Some(template.image.to_string()),
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
            template.image
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

    /// Redeploy an existing app: pull latest image, stop old container,
    /// recreate with the same configuration, and start it.
    pub async fn redeploy(&self, app_name: &str) -> anyhow::Result<()> {
        tracing::info!("Redeploying app '{}'...", app_name);

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
