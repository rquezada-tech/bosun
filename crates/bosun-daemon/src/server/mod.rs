//! gRPC server implementation.
//!
//! Implements the Bosun service defined in proto/bosun/v1/bosun.proto.

pub mod v1 {
    tonic::include_proto!("bosun.v1");
}

use std::pin::Pin;
use std::sync::Arc;
use v1::bosun_server::Bosun;
use v1::*;
use tonic::{Request, Response, Status};
use tokio_stream::Stream;

use crate::auth::interceptor;

pub struct BosunService {
    pub docker: Arc<tokio::sync::Mutex<crate::docker::DockerClient>>,
    pub metrics: Arc<crate::metrics::MetricCollector>,
    pub store: Arc<crate::persist::Store>,
    pub proxy: Option<Arc<crate::proxy::CaddyClient>>,
    /// Shared restart-count map populated by the health checker.
    pub restart_counts: crate::health::RestartCounts,
    /// Auth service for user management and token validation.
    pub auth_service: Arc<crate::auth::AuthService>,
    /// Template catalog for one-click apps.
    pub catalog: Arc<crate::templates::Catalog>,
}

impl BosunService {
    pub fn new(
        docker: Arc<tokio::sync::Mutex<crate::docker::DockerClient>>,
        metrics: crate::metrics::MetricCollector,
        store: Arc<crate::persist::Store>,
        proxy: Option<crate::proxy::CaddyClient>,
        restart_counts: crate::health::RestartCounts,
        auth_service: Arc<crate::auth::AuthService>,
        catalog: Arc<crate::templates::Catalog>,
    ) -> Self {
        Self {
            docker,
            metrics: Arc::new(metrics),
            store,
            proxy: proxy.map(Arc::new),
            restart_counts,
            auth_service,
            catalog,
        }
    }
}

#[tonic::async_trait]
impl Bosun for BosunService {
    // ── Auth RPCs ──────────────────────────────────────────────────

    async fn login(
        &self,
        request: Request<LoginRequest>,
    ) -> Result<Response<LoginResponse>, Status> {
        let req = request.into_inner();
        tracing::info!("login called for user '{}'", req.username);

        let token = self
            .auth_service
            .login(&req.username, &req.password)
            .map_err(|e| {
                tracing::warn!("Login failed for '{}': {}", req.username, e);
                Status::unauthenticated(format!("Login failed: {}", e))
            })?;

        // Get user role from the store
        let user = self
            .store
            .get_user(&req.username)
            .map_err(|e| Status::internal(format!("Failed to look up user: {}", e)))?
            .ok_or_else(|| Status::internal("User not found after successful login"))?;

        tracing::info!("User '{}' logged in successfully", req.username);

        Ok(Response::new(LoginResponse {
            token,
            username: req.username,
            role: user.role,
        }))
    }

    async fn list_users(
        &self,
        request: Request<ListUsersRequest>,
    ) -> Result<Response<ListUsersResponse>, Status> {
        // Require admin
        interceptor::require_admin(&request)?;

        tracing::info!("list_users called");
        let users = self
            .auth_service
            .list_users()
            .map_err(|e| Status::internal(format!("Failed to list users: {}", e)))?;

        let user_infos: Vec<UserInfo> = users
            .iter()
            .map(|u| UserInfo {
                username: u.username.clone(),
                role: u.role.clone(),
            })
            .collect();

        Ok(Response::new(ListUsersResponse { users: user_infos }))
    }

    async fn create_user(
        &self,
        request: Request<CreateUserRequest>,
    ) -> Result<Response<CreateUserResponse>, Status> {
        // Require admin
        interceptor::require_admin(&request)?;

        let req = request.into_inner();
        tracing::info!(
            "create_user called: username='{}', role='{}'",
            req.username,
            req.role
        );

        self.auth_service
            .create_user(&req.username, &req.password, &req.role)
            .map_err(|e| Status::invalid_argument(format!("Failed to create user: {}", e)))?;

        Ok(Response::new(CreateUserResponse {}))
    }

    async fn delete_user(
        &self,
        request: Request<DeleteUserRequest>,
    ) -> Result<Response<DeleteUserResponse>, Status> {
        // Require admin — extract claims before consuming request
        let claims = interceptor::require_admin(&request)?.clone();
        let req = request.into_inner();

        // Prevent deleting your own account
        if claims.sub == req.username {
            return Err(Status::invalid_argument(
                "Cannot delete your own account",
            ));
        }

        tracing::info!("delete_user called for '{}'", req.username);

        self.auth_service
            .delete_user(&req.username)
            .map_err(|e| Status::not_found(format!("Failed to delete user: {}", e)))?;

        Ok(Response::new(DeleteUserResponse {}))
    }

    // ── App management ─────────────────────────────────────────────

    async fn list_apps(
        &self,
        _request: Request<ListAppsRequest>,
    ) -> Result<Response<ListAppsResponse>, Status> {
        tracing::info!("list_apps called");
        let docker = self.docker.lock().await;
        let mut apps = docker.list_bosun_apps().await.map_err(|e| {
            Status::internal(format!("Failed to list containers: {}", e))
        })?;
        drop(docker);

        // Inject restart counts from the health checker.
        let counts = self.restart_counts.lock().await;
        for app in &mut apps {
            if let Some(&c) = counts.get(&app.name) {
                app.restart_count = c;
            }
        }

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
        // Extract claims for created-by label BEFORE consuming the request
        let created_by = interceptor::get_claims(&request)
            .map(|c| c.sub.clone())
            .unwrap_or_else(|_| "unknown".to_string());

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

        // Map proto DeployStrategy enum (i32) to crate deploy strategy
        // 0 = DIRECT (default), 1 = ROLLING, 2 = BLUE_GREEN
        let strategy = match req.strategy {
            1 => crate::deploy::DeployStrategy::Rolling,
            2 => crate::deploy::DeployStrategy::BlueGreen,
            _ => crate::deploy::DeployStrategy::Direct,
        };

        tracing::info!(
            "Deploy request: strategy={:?}, created_by='{}'",
            strategy,
            created_by
        );

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

        // Extract template version request BEFORE moving req.env
        let version_requested = req.env.get("BOSUN_TEMPLATE_VERSION").cloned();

        // Add created-by to env vars as a Docker label
        let mut env_vars: std::collections::HashMap<String, String> = req.env;
        env_vars.insert("BOSUN_CREATED_BY".to_string(), created_by.clone());

        // Check if context_path is a known built-in template name
        // If so, use template deploy (no Dockerfile needed).
        if let Some((template, resolved_image)) = self.catalog.get_template(&app_name, version_requested.as_deref()) {
            tracing::info!(
                "Deploy request: template={}, app={}, image={}, domain={:?}, port={}, ssl={}",
                template.name,
                app_name,
                resolved_image,
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
                .deploy_template(template, &resolved_image, &app_name, domain, port_override)
                .await
                .map_err(|e| Status::internal(format!("Template deploy failed: {:#}", e)))?;

            // Add bosun.created-by label — tracked via docker labels at deploy time
            tracing::debug!("App '{}' created by '{}'", app_name, created_by);

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

        // Not a template — use the strategy dispatcher for build-from-directory flow
        tracing::info!(
            "Deploy request: app={}, path={}, domain={:?}, port={}, ssl={}, strategy={:?}",
            app_name,
            req.context_path,
            domain,
            port,
            enable_ssl,
            strategy
        );

        let docker = self.docker.lock().await;
        let proxy = self.proxy.as_deref();

        let (_container_name, _actual_port) = crate::deploy::execute_deploy(
            strategy,
            &docker,
            proxy,
            std::path::Path::new(&req.context_path),
            &app_name,
            domain,
            port,
            &env_vars,
        )
        .await
        .map_err(|e| Status::internal(format!("Deploy failed: {}", e)))?;

        // Track created-by via docker labels
        tracing::debug!("App '{}' created by '{}' (non-template deploy)", app_name, created_by);

        // Persist app metadata and env vars
        let _ = self.store.upsert_app(&app_name, domain, Some(port as u32), &env_vars);

        Ok(Response::new(DeployResponse {
            app_name,
            status: "deployed".to_string(),
        }))
    }

    async fn rollback_app(
        &self,
        request: Request<RollbackAppRequest>,
    ) -> Result<Response<RollbackAppResponse>, Status> {
        let req = request.into_inner();
        tracing::info!("rollback_app called for {}", req.app_name);

        let docker = self.docker.lock().await;

        // Check if the app has a blue-green setup by inspecting labels
        match docker.inspect_container(&req.app_name).await {
            Ok(inspect) => {
                let labels = inspect
                    .config
                    .and_then(|c| c.labels)
                    .unwrap_or_default();
                let has_blue_green = labels.get("bosun.strategy").map(|s| s.as_str()) == Some("blue_green");

                if has_blue_green {
                    // Use the strategy dispatcher to handle rollback + Caddy swap
                    let domain = labels.get("bosun.domain").cloned();
                    let proxy = self.proxy.as_deref();

                    crate::deploy::execute_rollback(&docker, proxy, &req.app_name, domain.as_deref())
                        .await
                        .map_err(|e| Status::internal(format!("Rollback failed: {}", e)))?;

                    Ok(Response::new(RollbackAppResponse {
                        status: "rolled_back".to_string(),
                        message: format!(
                            "App '{}' rolled back to previous color",
                            req.app_name
                        ),
                    }))
                } else {
                    // No blue-green setup — can't rollback
                    tracing::info!(
                        "App '{}' does not have blue-green deploy setup. Rollback not available.",
                        req.app_name
                    );
                    Ok(Response::new(RollbackAppResponse {
                        status: "not_available".to_string(),
                        message: format!(
                            "Blue-green deploy required for rollback. App '{}' uses direct/rolling strategy.",
                            req.app_name
                        ),
                    }))
                }
            }
            Err(_) => {
                // App doesn't exist
                Err(Status::not_found(format!(
                    "App '{}' not found",
                    req.app_name
                )))
            }
        }
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
        let templates = self.catalog.list_templates()
            .iter()
            .map(|t| TemplateInfo {
                name: t.name.clone(),
                description: t.description.clone(),
                category: t.category.as_str().to_string(),
                default_port: t.default_port as u32,
                versions: t.versions.iter().map(|v| v.version.clone()).collect(),
                icon: t.icon.clone().unwrap_or_default(),
            })
            .collect();
        Ok(Response::new(ListTemplatesResponse { templates }))
    }

    // ── Backup & Restore (stubs) ────────────────────────────────────

    async fn create_backup(
        &self,
        _request: Request<CreateBackupRequest>,
    ) -> Result<Response<CreateBackupResponse>, Status> {
        Err(Status::unimplemented("Backup not yet implemented"))
    }

    async fn list_backups(
        &self,
        _request: Request<ListBackupsRequest>,
    ) -> Result<Response<ListBackupsResponse>, Status> {
        Err(Status::unimplemented("Backup listing not yet implemented"))
    }

    async fn restore_backup(
        &self,
        _request: Request<RestoreBackupRequest>,
    ) -> Result<Response<RestoreBackupResponse>, Status> {
        Err(Status::unimplemented("Restore not yet implemented"))
    }
}
