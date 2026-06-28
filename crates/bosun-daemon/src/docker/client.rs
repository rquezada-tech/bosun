//! Docker API client wrapper.
//!
//! Connects to the local Docker daemon via bollard
//! and provides Bosun-specific operations.

use bollard::Docker;
use bollard::container::ListContainersOptions;
use bollard::secret::ContainerSummary;
use crate::server::v1::{App, AppStatus};

pub struct DockerClient {
    inner: Docker,
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

        let apps = containers.into_iter().filter_map(|c| {
            let names = c.names?;
            let name = names.first()?.strip_prefix('/').unwrap_or(names.first()?).to_string();
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
        }).collect();

        Ok(apps)
    }
}
