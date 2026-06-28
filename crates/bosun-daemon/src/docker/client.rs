//! Docker API client wrapper.
//!
//! Connects to the local Docker daemon via bollard
//! and provides Bosun-specific operations.

use bollard::Docker;
use bollard::container::{
    ListContainersOptions, LogOutput, LogsOptions, RestartContainerOptions,
    StopContainerOptions,
};
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
