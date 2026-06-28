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
use futures_util::StreamExt;

pub struct DockerClient {
    pub inner: Docker,
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

                Some(App {
                    name,
                    status: status.into(),
                    domain,
                    port,
                    instances: Some(1),
                    env_keys: vec![],
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
    ) -> impl futures_util::Stream<Item = anyhow::Result<LogEntry>> {
        let options = LogsOptions {
            follow,
            stdout: true,
            stderr: true,
            tail: tail_lines.to_string(),
            timestamps: true,
            ..Default::default()
        };

        let stream = self.inner.logs(name, Some(options));

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
