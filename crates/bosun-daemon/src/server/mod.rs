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
}

impl BosunService {
    pub fn new(
        docker: crate::docker::DockerClient,
        metrics: crate::metrics::MetricCollector,
        store: crate::persist::Store,
    ) -> Self {
        Self {
            docker: std::sync::Arc::new(tokio::sync::Mutex::new(docker)),
            metrics: std::sync::Arc::new(metrics),
            store: std::sync::Arc::new(store),
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

        let env_vars: std::collections::HashMap<String, String> = req.env;

        tracing::info!(
            "Deploy request: app={}, path={}, domain={:?}, port={}",
            app_name,
            req.context_path,
            domain,
            port
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

        // Persist app metadata and env vars
        let _ = self.store.upsert_app(&app_name, domain, Some(port as u32), &env_vars);

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
}
