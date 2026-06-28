//! gRPC server implementation.
//!
//! Implements the Bosun service defined in proto/bosun/v1/bosun.proto.

pub mod v1 {
    tonic::include_proto!("bosun.v1");
}

use std::pin::Pin;
use v1::bosun_server::Bosun;
use v1::*;
use tonic::{Request, Response, Status};
use tokio_stream::Stream;

pub struct BosunService {
    pub docker: std::sync::Arc<tokio::sync::Mutex<crate::docker::DockerClient>>,
    pub metrics: std::sync::Arc<crate::metrics::MetricCollector>,
    pub store: std::sync::Arc<crate::persist::Store>,
    pub proxy: Option<std::sync::Arc<crate::proxy::CaddyClient>>,
}

impl BosunService {
    pub fn new(
        docker: crate::docker::DockerClient,
        metrics: crate::metrics::MetricCollector,
        store: crate::persist::Store,
        proxy: Option<crate::proxy::CaddyClient>,
    ) -> Self {
        Self {
            docker: std::sync::Arc::new(tokio::sync::Mutex::new(docker)),
            metrics: std::sync::Arc::new(metrics),
            store: std::sync::Arc::new(store),
            proxy: proxy.map(std::sync::Arc::new),
        }
    }
}

#[tonic::async_trait]
impl Bosun for BosunService {
    async fn list_apps(
        &self,
        _request: Request<ListAppsRequest>,
    ) -> Result<Response<ListAppsResponse>, Status> {
        tracing::info!("list_apps called");
        let docker = self.docker.lock().await;
        let apps = docker.list_bosun_apps().await.map_err(|e| {
            Status::internal(format!("Failed to list containers: {}", e))
        })?;
        Ok(Response::new(ListAppsResponse { apps }))
    }

    type GetAppLogsStream = Pin<Box<dyn Stream<Item = Result<LogEntry, Status>> + Send + 'static>>;
    async fn get_app_logs(
        &self,
        request: Request<GetAppLogsRequest>,
    ) -> Result<Response<Self::GetAppLogsStream>, Status> {
        let req = request.into_inner();
        let follow = req.follow;
        let tail_lines = req.tail_lines;

        tracing::info!(
            "get_app_logs called for {} (follow={}, tail={})",
            req.app_name,
            follow,
            tail_lines
        );

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<LogEntry, Status>>(32);

        let docker = self.docker.clone();
        let app_name = req.app_name.clone();

        tokio::spawn(async move {
            use futures_util::StreamExt;
            let stream = {
                let client = docker.lock().await;
                client.get_logs(&app_name, follow, tail_lines)
            };
            tokio::pin!(stream);
            while let Some(result) = stream.next().await {
                let item = match result {
                    Ok(entry) => Ok(entry),
                    Err(e) => Err(Status::internal(format!("Log error: {}", e))),
                };
                if tx.send(item).await.is_err() {
                    break; // client disconnected
                }
            }
        });

        let output_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(output_stream)))
    }

    async fn restart_app(
        &self,
        request: Request<RestartAppRequest>,
    ) -> Result<Response<RestartAppResponse>, Status> {
        let req = request.into_inner();
        tracing::info!("restart_app called for {}", req.app_name);
        let docker = self.docker.lock().await;
        docker
            .restart_container(&req.app_name)
            .await
            .map_err(|e| Status::internal(format!("Restart failed: {}", e)))?;
        Ok(Response::new(RestartAppResponse {}))
    }

    async fn scale_app(
        &self,
        request: Request<ScaleAppRequest>,
    ) -> Result<Response<ScaleAppResponse>, Status> {
        let req = request.into_inner();
        tracing::info!("scale_app called for {} to {} instances", req.app_name, req.instances);
        let docker = self.docker.lock().await;
        docker
            .scale_app(&req.app_name, req.instances)
            .await
            .map_err(|e| Status::invalid_argument(format!("Scale failed: {}", e)))?;
        Ok(Response::new(ScaleAppResponse {}))
    }

    async fn deploy(
        &self,
        request: Request<DeployRequest>,
    ) -> Result<Response<DeployResponse>, Status> {
        let req = request.into_inner();
        // Derive app name from context path directory name
        let app_name = std::path::Path::new(&req.context_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("app")
            .to_string();
        let port = req.port.unwrap_or(8080) as u16;
        let domain = req.domain.as_deref();
        let enable_ssl = req.enable_ssl;

        // Validate SSL flag: requires a domain for Let's Encrypt via Caddy
        if enable_ssl {
            if domain.is_none() {
                return Err(Status::invalid_argument(
                    "SSL requires --domain. Let's Encrypt needs a public domain name to issue a certificate.",
                ));
            }
            tracing::info!(
                "SSL enabled for domain {} — SSL will be provisioned via Caddy (Let's Encrypt)",
                domain.unwrap()
            );
        }

        let env_vars: std::collections::HashMap<String, String> = req.env;

        // Check if context_path is a known built-in template name
        // If so, use template deploy (no Dockerfile needed).
        if let Some(template) = crate::templates::get_template(&app_name) {
            tracing::info!(
                "Deploy request: template={}, app={}, domain={:?}, port={}, ssl={}",
                template.name,
                app_name,
                domain,
                port,
                enable_ssl
            );

            let docker = self.docker.lock().await;
            let port_override = if req.port.is_some() {
                Some(port)
            } else {
                None // use template default
            };

            docker
                .deploy_template(template, &app_name, domain, port_override)
                .await
                .map_err(|e| Status::internal(format!("Template deploy failed: {}", e)))?;

            // Health check
            tracing::info!(
                "Waiting up to 30s for container '{}' to become healthy...",
                app_name
            );
            docker
                .wait_for_container_healthy(&app_name, 30)
                .await
                .map_err(|e| {
                    Status::internal(format!(
                        "Container '{}' failed health check: {}. Check container logs for details.",
                        app_name, e
                    ))
                })?;

            // Persist app metadata and env vars
            let _ = self.store.upsert_app(&app_name, domain, Some(port as u32), &env_vars);

            // Configure Caddy reverse proxy if domain was provided
            if let (Some(domain), Some(proxy)) = (domain, &self.proxy) {
                tracing::info!(
                    "Configuring Caddy reverse proxy: {} -> localhost:{}",
                    domain,
                    port
                );
                if let Err(e) = proxy.configure_app(domain, port).await {
                    tracing::error!(
                        "Failed to configure Caddy proxy for {}: {}. Container is running but won't receive HTTP traffic via domain.",
                        domain,
                        e
                    );
                }
            }

            return Ok(Response::new(DeployResponse {
                app_name,
                status: "deployed".to_string(),
            }));
        }

        // Not a template — use the existing build-from-directory flow
        tracing::info!(
            "Deploy request: app={}, path={}, domain={:?}, port={}, ssl={}",
            app_name,
            req.context_path,
            domain,
            port,
            enable_ssl
        );

        let docker = self.docker.lock().await;
        docker
            .deploy(
                std::path::Path::new(&req.context_path),
                &app_name,
                domain,
                port,
                &env_vars,
            )
            .await
            .map_err(|e| Status::internal(format!("Deploy failed: {}", e)))?;

        // Health check: wait up to 30s for the container to reach running state
        // before configuring the proxy (Caddy)
        tracing::info!(
            "Waiting up to 30s for container '{}' to become healthy...",
            app_name
        );
        docker
            .wait_for_container_healthy(&app_name, 30)
            .await
            .map_err(|e| {
                Status::internal(format!(
                    "Container '{}' failed health check: {}. Check container logs for details.",
                    app_name, e
                ))
            })?;

        // Persist app metadata and env vars
        let _ = self.store.upsert_app(&app_name, domain, Some(port as u32), &env_vars);

        // Configure Caddy reverse proxy if domain was provided
        if let (Some(domain), Some(proxy)) = (domain, &self.proxy) {
            tracing::info!(
                "Configuring Caddy reverse proxy: {} -> localhost:{}",
                domain,
                port
            );
            if let Err(e) = proxy.configure_app(domain, port).await {
                tracing::error!(
                    "Failed to configure Caddy proxy for {}: {}. Container is running but won't receive HTTP traffic via domain.",
                    domain,
                    e
                );
            }
        }

        Ok(Response::new(DeployResponse {
            app_name,
            status: "deployed".to_string(),
        }))
    }

    async fn get_metrics(
        &self,
        request: Request<GetMetricsRequest>,
    ) -> Result<Response<GetMetricsResponse>, Status> {
        let req = request.into_inner();
        tracing::info!("get_metrics called for {:?}", req.app_name);

        if let Some(app_name) = &req.app_name {
            // Single app snapshot
            let metric = self.metrics.get_snapshot(app_name).await.map_err(|e| {
                Status::internal(format!("Metrics error: {}", e))
            })?;
            Ok(Response::new(GetMetricsResponse {
                metrics: vec![metric],
            }))
        } else {
            // All apps — get list then snapshot each
            let docker = self.docker.lock().await;
            let apps = docker.list_bosun_apps().await.map_err(|e| {
                Status::internal(format!("Failed to list containers: {}", e))
            })?;

            let mut metrics = Vec::new();
            for app in &apps {
                if app.status() == AppStatus::Running {
                    match self.metrics.get_snapshot(&app.name).await {
                        Ok(m) => metrics.push(m),
                        Err(e) => {
                            tracing::warn!("Failed to get metrics for {}: {}", app.name, e);
                        }
                    }
                }
            }
            Ok(Response::new(GetMetricsResponse { metrics }))
        }
    }

    type StreamMetricsStream = Pin<Box<dyn Stream<Item = Result<AppMetric, Status>> + Send + 'static>>;
    async fn stream_metrics(
        &self,
        request: Request<GetMetricsRequest>,
    ) -> Result<Response<Self::StreamMetricsStream>, Status> {
        let req = request.into_inner();
        let app_name = req.app_name.ok_or_else(|| {
            Status::invalid_argument("app_name is required for streaming metrics")
        })?;

        tracing::info!("stream_metrics called for {}", app_name);

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<AppMetric, Status>>(32);
        let metrics = self.metrics.clone();
        let name = app_name.clone();

        tokio::spawn(async move {
            use futures_util::StreamExt;
            let mut stream = metrics.stream_live(&name);
            while let Some(result) = stream.next().await {
                let item = match result {
                    Ok(metric) => Ok(metric),
                    Err(e) => Err(Status::internal(format!("Stream error: {}", e))),
                };
                if tx.send(item).await.is_err() {
                    break; // client disconnected
                }
            }
        });

        let output_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(output_stream)))
    }

    async fn get_env(
        &self,
        request: Request<GetEnvRequest>,
    ) -> Result<Response<GetEnvResponse>, Status> {
        let req = request.into_inner();
        tracing::info!("get_env called for {}", req.app_name);
        let env = self
            .store
            .get_app_env(&req.app_name)
            .map_err(|e| Status::internal(format!("Failed to get env: {}", e)))?;
        Ok(Response::new(GetEnvResponse { env }))
    }

    async fn set_env(
        &self,
        request: Request<SetEnvRequest>,
    ) -> Result<Response<SetEnvResponse>, Status> {
        let req = request.into_inner();
        tracing::info!(
            "set_env called: app={}, key={}, value={}",
            req.app_name,
            req.key,
            req.value
        );
        self.store
            .set_app_env(&req.app_name, &req.key, &req.value)
            .map_err(|e| Status::internal(format!("Failed to set env: {}", e)))?;
        Ok(Response::new(SetEnvResponse {}))
    }

    async fn unset_env(
        &self,
        request: Request<UnsetEnvRequest>,
    ) -> Result<Response<UnsetEnvResponse>, Status> {
        let req = request.into_inner();
        tracing::info!("unset_env called: app={}, key={}", req.app_name, req.key);
        self.store
            .unset_app_env(&req.app_name, &req.key)
            .map_err(|e| Status::internal(format!("Failed to unset env: {}", e)))?;
        Ok(Response::new(UnsetEnvResponse {}))
    }

    async fn list_templates(
        &self,
        _request: Request<ListTemplatesRequest>,
    ) -> Result<Response<ListTemplatesResponse>, Status> {
        tracing::info!("list_templates called");
        let templates = crate::templates::list_templates()
            .iter()
            .map(|t| TemplateInfo {
                name: t.name.to_string(),
                description: t.description.to_string(),
                category: t.category.as_str().to_string(),
                default_port: t.default_port as u32,
            })
            .collect();
        Ok(Response::new(ListTemplatesResponse { templates }))
    }
}
