//! gRPC server implementation.
//!
//! Implements the Bosun service defined in proto/bosun/v1/bosun.proto.

pub mod v1 {
    tonic::include_proto!("bosun.v1");
}

use v1::bosun_server::{Bosun, BosunServer};
use v1::*;
use tonic::{Request, Response, Status, Streaming};

pub struct BosunService {
    pub docker: std::sync::Arc<tokio::sync::Mutex<crate::docker::DockerClient>>,
    pub store: std::sync::Arc<crate::persist::Store>,
}

impl BosunService {
    pub fn new(
        docker: crate::docker::DockerClient,
        store: crate::persist::Store,
    ) -> Self {
        Self {
            docker: std::sync::Arc::new(tokio::sync::Mutex::new(docker)),
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
        let docker = self.docker.lock().await;
        let apps = docker.list_bosun_apps().await.map_err(|e| {
            Status::internal(format!("Failed to list containers: {}", e))
        })?;
        Ok(Response::new(ListAppsResponse { apps }))
    }

    type GetAppLogsStream = tonic::codec::Streaming<LogEntry>;
    async fn get_app_logs(
        &self,
        _request: Request<GetAppLogsRequest>,
    ) -> Result<Response<Self::GetAppLogsStream>, Status> {
        todo!("get_app_logs — Task 3/5")
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
        _request: Request<GetMetricsRequest>,
    ) -> Result<Response<GetMetricsResponse>, Status> {
        todo!("get_metrics — Task 4")
    }

    type StreamMetricsStream = tonic::codec::Streaming<AppMetric>;
    async fn stream_metrics(
        &self,
        _request: Request<GetMetricsRequest>,
    ) -> Result<Response<Self::StreamMetricsStream>, Status> {
        todo!("stream_metrics — Task 4")
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
