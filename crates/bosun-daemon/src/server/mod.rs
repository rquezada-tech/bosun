//! gRPC server implementation.
//!
//! Implements the Bosun service defined in proto/bosun/v1/bosun.proto.

pub mod v1 {
    tonic::include_proto!("bosun.v1");
}

use std::pin::Pin;
use v1::bosun_server::{Bosun, BosunServer};
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
        _request: Request<GetAppLogsRequest>,
    ) -> Result<Response<Self::GetAppLogsStream>, Status> {
        todo!("get_app_logs")
    }

    async fn restart_app(
        &self,
        _request: Request<RestartAppRequest>,
    ) -> Result<Response<RestartAppResponse>, Status> {
        todo!("restart_app")
    }

    async fn scale_app(
        &self,
        _request: Request<ScaleAppRequest>,
    ) -> Result<Response<ScaleAppResponse>, Status> {
        todo!("scale_app")
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
        _request: Request<GetEnvRequest>,
    ) -> Result<Response<GetEnvResponse>, Status> {
        todo!("get_env")
    }

    async fn set_env(
        &self,
        _request: Request<SetEnvRequest>,
    ) -> Result<Response<SetEnvResponse>, Status> {
        todo!("set_env")
    }

    async fn unset_env(
        &self,
        _request: Request<UnsetEnvRequest>,
    ) -> Result<Response<UnsetEnvResponse>, Status> {
        todo!("unset_env")
    }
}
