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
    /// APISIX API Gateway client (optional — None if gateway is not running).
    pub gateway: Option<crate::gateway::GatewayClient>,
    /// Shared restart-count map populated by the health checker.
    pub restart_counts: crate::health::RestartCounts,
    /// Auth service for user management and token validation.
    pub auth_service: Arc<crate::auth::AuthService>,
    /// Template catalog for one-click apps.
    pub catalog: Arc<crate::templates::Catalog>,
    /// Backup and restore service.
    pub backup: Arc<crate::backup::BackupService>,
    /// Security engine (CrowdSec/Fail2Ban) for IDS/IPS monitoring.
    pub security: crate::security::SecurityService,
    /// Multi-node cluster controller.
    pub cluster: crate::cluster::ClusterController,
}

impl BosunService {
    pub fn new(
        docker: Arc<tokio::sync::Mutex<crate::docker::DockerClient>>,
        metrics: Arc<crate::metrics::MetricCollector>,
        store: Arc<crate::persist::Store>,
        proxy: Option<crate::proxy::CaddyClient>,
        gateway: Option<crate::gateway::GatewayClient>,
        restart_counts: crate::health::RestartCounts,
        auth_service: Arc<crate::auth::AuthService>,
        catalog: Arc<crate::templates::Catalog>,
        backup: Arc<crate::backup::BackupService>,
        security: crate::security::SecurityService,
    ) -> Self {
        let cluster = crate::cluster::ClusterController::new(store.clone());
        Self {
            docker,
            metrics,
            store,
            proxy: proxy.map(Arc::new),
            gateway,
            restart_counts,
            auth_service,
            catalog,
            backup,
            security,
            cluster,
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

        // Extract hooks from request + auto-detected bosun.hooks.toml
        let context_dir = std::path::Path::new(&req.context_path);
        let mut pre_hooks: Vec<String> = req.pre_hooks.clone();
        let mut post_hooks: Vec<String> = req.post_hooks.clone();

        if let Some(hooks_config) = crate::hooks::load_hooks_from_dir(context_dir)
            .map_err(|e| Status::internal(format!("Failed to load bosun.hooks.toml: {e}")))?
        {
            if let Some(pre_section) = hooks_config.pre_deploy {
                pre_hooks.extend(pre_section.commands);
            }
            if let Some(post_section) = hooks_config.post_deploy {
                post_hooks.extend(post_section.commands);
            }
        }

        // Map proto DeployStrategy enum (i32) to crate deploy strategy
        // 0 = DIRECT (default), 1 = ROLLING, 2 = BLUE_GREEN
        let strategy = match req.strategy {
            1 => crate::deploy::DeployStrategy::Rolling,
            2 => crate::deploy::DeployStrategy::BlueGreen,
            _ => crate::deploy::DeployStrategy::Direct,
        };

        tracing::info!(
            "Deploy request: strategy={:?}, created_by='{}', pre_hooks={}, post_hooks={}",
            strategy,
            created_by,
            pre_hooks.len(),
            post_hooks.len()
        );

        // Run pre-deploy hooks on the host before build
        let env_vars_for_hooks: std::collections::HashMap<String, String> = req.env.clone();
        crate::hooks::run_hooks(&pre_hooks, context_dir, &env_vars_for_hooks)
            .await
            .map_err(|e| {
                tracing::error!("Pre-deploy hooks failed: {e}");
                Status::internal(format!("Pre-deploy hooks failed: {e}"))
            })?;

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

            // Run post-deploy hooks after deploy + health check
            crate::hooks::run_hooks(&post_hooks, context_dir, &env_vars)
                .await
                .map_err(|e| {
                    tracing::error!("Post-deploy hooks failed: {e}");
                    Status::internal(format!("Post-deploy hooks failed: {e}"))
                })?;

            // Configure security monitoring for this app
            self.security.configure_app(&app_name, domain);

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
        .map_err(|e| Status::internal(format!("Deploy failed: {e}")))?;

        // Track created-by via docker labels
        tracing::debug!("App '{}' created by '{}' (non-template deploy)", app_name, created_by);

        // Persist app metadata and env vars
        let _ = self.store.upsert_app(&app_name, domain, Some(port as u32), &env_vars);

        // Run post-deploy hooks after deploy + health check
        crate::hooks::run_hooks(&post_hooks, context_dir, &env_vars)
            .await
            .map_err(|e| {
                tracing::error!("Post-deploy hooks failed: {e}");
                Status::internal(format!("Post-deploy hooks failed: {e}"))
            })?;

        // Configure security monitoring for this app
        self.security.configure_app(&app_name, domain);

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

    // ── Backup & Restore ────────────────────────────────────────────

    async fn create_backup(
        &self,
        request: Request<CreateBackupRequest>,
    ) -> Result<Response<CreateBackupResponse>, Status> {
        let req = request.into_inner();
        tracing::info!("create_backup called for {}", req.app_name);

        let backup = self
            .backup
            .create_backup(&req.app_name)
            .await
            .map_err(|e| Status::internal(format!("Backup failed: {:#}", e)))?;

        Ok(Response::new(CreateBackupResponse {
            backup: Some(backup),
        }))
    }

    async fn list_backups(
        &self,
        request: Request<ListBackupsRequest>,
    ) -> Result<Response<ListBackupsResponse>, Status> {
        let req = request.into_inner();
        tracing::info!(
            "list_backups called (app={:?})",
            req.app_name
        );

        let backups = self
            .backup
            .list_backups(req.app_name.as_deref())
            .map_err(|e| Status::internal(format!("Failed to list backups: {:#}", e)))?;

        Ok(Response::new(ListBackupsResponse { backups }))
    }

    async fn restore_backup(
        &self,
        request: Request<RestoreBackupRequest>,
    ) -> Result<Response<RestoreBackupResponse>, Status> {
        let req = request.into_inner();
        tracing::info!("restore_backup called for backup_id={}", req.backup_id);

        let (app_name, status) = self
            .backup
            .restore_backup(&req.backup_id)
            .await
            .map_err(|e| Status::internal(format!("Restore failed: {:#}", e)))?;

        Ok(Response::new(RestoreBackupResponse {
            app_name,
            status,
        }))
    }

    // ── Gateway (APISIX) ────────────────────────────────────────────

    async fn get_gateway_status(
        &self,
        _request: Request<GetGatewayStatusRequest>,
    ) -> Result<Response<GetGatewayStatusResponse>, Status> {
        tracing::info!("get_gateway_status called");

        let status = match &self.gateway {
            Some(gw) => {
                match gw.get_status().await {
                    Ok(info) => GatewayStatus {
                        enabled: info.enabled,
                        version: info.version,
                        uptime: info.uptime,
                    },
                    Err(e) => {
                        tracing::warn!("Failed to get gateway status: {e}");
                        GatewayStatus {
                            enabled: false,
                            version: format!("error: {e}"),
                            uptime: String::new(),
                        }
                    }
                }
            }
            None => GatewayStatus {
                enabled: false,
                version: "APISIX not configured".to_string(),
                uptime: String::new(),
            },
        };

        Ok(Response::new(GetGatewayStatusResponse {
            status: Some(status),
        }))
    }

    async fn list_gateway_routes(
        &self,
        _request: Request<ListGatewayRoutesRequest>,
    ) -> Result<Response<ListGatewayRoutesResponse>, Status> {
        tracing::info!("list_gateway_routes called");

        let gateway = self.gateway.as_ref().ok_or_else(|| {
            Status::unavailable("APISIX gateway is not configured. Run APISIX via Docker to enable.")
        })?;

        let routes = gateway
            .list_routes()
            .await
            .map_err(|e| Status::internal(format!("Failed to list gateway routes: {e}")))?;

        let proto_routes: Vec<GatewayRoute> = routes
            .into_iter()
            .map(|r| GatewayRoute {
                name: r.name,
                domain: r.domain,
                port: r.port,
                plugins: r.plugins,
                uri: r.uri,
            })
            .collect();

        Ok(Response::new(ListGatewayRoutesResponse {
            routes: proto_routes,
        }))
    }

    async fn enable_gateway_plugin(
        &self,
        request: Request<EnableGatewayPluginRequest>,
    ) -> Result<Response<EnableGatewayPluginResponse>, Status> {
        let req = request.into_inner();
        tracing::info!(
            "enable_gateway_plugin: app={}, plugin={}",
            req.app_name,
            req.plugin_name
        );

        let gateway = self.gateway.as_ref().ok_or_else(|| {
            Status::unavailable("APISIX gateway is not configured. Run APISIX via Docker to enable.")
        })?;

        gateway
            .enable_plugin(&req.app_name, &req.plugin_name, &req.config_json)
            .await
            .map_err(|e| Status::internal(format!("Failed to enable plugin: {e}")))?;

        Ok(Response::new(EnableGatewayPluginResponse {}))
    }

    async fn disable_gateway_plugin(
        &self,
        request: Request<DisableGatewayPluginRequest>,
    ) -> Result<Response<DisableGatewayPluginResponse>, Status> {
        let req = request.into_inner();
        tracing::info!(
            "disable_gateway_plugin: app={}, plugin={}",
            req.app_name,
            req.plugin_name
        );

        let gateway = self.gateway.as_ref().ok_or_else(|| {
            Status::unavailable("APISIX gateway is not configured. Run APISIX via Docker to enable.")
        })?;

        gateway
            .disable_plugin(&req.app_name, &req.plugin_name)
            .await
            .map_err(|e| Status::internal(format!("Failed to disable plugin: {e}")))?;

        Ok(Response::new(DisableGatewayPluginResponse {}))
    }

    async fn get_gateway_cache_stats(
        &self,
        request: Request<GetGatewayCacheStatsRequest>,
    ) -> Result<Response<GetGatewayCacheStatsResponse>, Status> {
        let req = request.into_inner();
        tracing::info!("get_gateway_cache_stats: app={}", req.app_name);

        let gateway = self.gateway.as_ref().ok_or_else(|| {
            Status::unavailable("APISIX gateway is not configured. Run APISIX via Docker to enable.")
        })?;

        let stats = gateway
            .get_cache_stats(&req.app_name)
            .await
            .map_err(|e| Status::internal(format!("Failed to get cache stats: {e}")))?;

        Ok(Response::new(GetGatewayCacheStatsResponse {
            stats: Some(GatewayCacheStats {
                app_name: stats.app_name,
                hits: stats.hits,
                misses: stats.misses,
                size_bytes: stats.size_bytes,
            }),
        }))
    }

    async fn purge_gateway_cache(
        &self,
        request: Request<PurgeGatewayCacheRequest>,
    ) -> Result<Response<PurgeGatewayCacheResponse>, Status> {
        let req = request.into_inner();
        tracing::info!("purge_gateway_cache: app={}", req.app_name);

        let gateway = self.gateway.as_ref().ok_or_else(|| {
            Status::unavailable("APISIX gateway is not configured. Run APISIX via Docker to enable.")
        })?;

        // Purge cache by disabling and re-enabling proxy-cache plugin
        gateway
            .disable_cache(&req.app_name)
            .await
            .map_err(|e| Status::internal(format!("Failed to purge cache: {e}")))?;

        Ok(Response::new(PurgeGatewayCacheResponse {}))
    }

    async fn get_gateway_metrics(
        &self,
        _request: Request<GetGatewayMetricsRequest>,
    ) -> Result<Response<GetGatewayMetricsResponse>, Status> {
        tracing::info!("get_gateway_metrics called");

        let gateway = self.gateway.as_ref().ok_or_else(|| {
            Status::unavailable("APISIX gateway is not configured. Run APISIX via Docker to enable.")
        })?;

        let metrics_text = gateway
            .get_metrics()
            .await
            .map_err(|e| Status::internal(format!("Failed to get gateway metrics: {e}")))?;

        Ok(Response::new(GetGatewayMetricsResponse { metrics_text }))
    }

    // ── Cross-VPS Peer Management (mTLS) ───────────────────────────

    async fn add_peer(
        &self,
        request: Request<AddPeerRequest>,
    ) -> Result<Response<AddPeerResponse>, Status> {
        let req = request.into_inner();
        tracing::info!("add_peer: name={}, addr={}", req.name, req.addr);

        let peer = crate::gateway::GatewayClient::add_peer(&req.name, &req.addr, &req.cert_path)
            .map_err(|e| Status::internal(format!("Failed to add peer: {e}")))?;

        Ok(Response::new(AddPeerResponse {
            peer: Some(PeerInfo {
                name: peer.name,
                addr: peer.addr,
                cert_path: peer.cert_path,
                status: peer.status.to_string(),
            }),
        }))
    }

    async fn list_peers(
        &self,
        _request: Request<ListPeersRequest>,
    ) -> Result<Response<ListPeersResponse>, Status> {
        tracing::info!("list_peers called");

        let peers = crate::gateway::GatewayClient::list_peers()
            .map_err(|e| Status::internal(format!("Failed to list peers: {e}")))?;

        let proto_peers: Vec<PeerInfo> = peers
            .into_iter()
            .map(|p| PeerInfo {
                name: p.name,
                addr: p.addr,
                cert_path: p.cert_path,
                status: p.status.to_string(),
            })
            .collect();

        Ok(Response::new(ListPeersResponse {
            peers: proto_peers,
        }))
    }

    async fn remove_peer(
        &self,
        request: Request<RemovePeerRequest>,
    ) -> Result<Response<RemovePeerResponse>, Status> {
        let req = request.into_inner();
        tracing::info!("remove_peer: name={}", req.name);

        crate::gateway::GatewayClient::remove_peer(&req.name)
            .map_err(|e| Status::internal(format!("Failed to remove peer: {e}")))?;

        Ok(Response::new(RemovePeerResponse {}))
    }

    async fn test_peer(
        &self,
        request: Request<TestPeerRequest>,
    ) -> Result<Response<TestPeerResponse>, Status> {
        let req = request.into_inner();
        tracing::info!("test_peer: name={}", req.name);

        let peer = crate::gateway::GatewayClient::test_peer(&req.name)
            .await
            .map_err(|e| Status::internal(format!("Failed to test peer: {e}")))?;

        Ok(Response::new(TestPeerResponse {
            peer: Some(PeerInfo {
                name: peer.name,
                addr: peer.addr,
                cert_path: peer.cert_path,
                status: peer.status.to_string(),
            }),
        }))
    }

    // ── Security RPCs ─────────────────────────────────────────────

    async fn get_security_status(
        &self,
        _request: Request<GetSecurityStatusRequest>,
    ) -> Result<Response<GetSecurityStatusResponse>, Status> {
        tracing::info!("get_security_status called");

        let stats = self.security.status();

        Ok(Response::new(GetSecurityStatusResponse {
            status: Some(SecurityStatus {
                engine: stats.engine.as_str().to_string(),
                attacks_blocked: stats.attacks_blocked,
                active_bans: stats.active_bans,
            }),
        }))
    }

    async fn get_security_decisions(
        &self,
        _request: Request<GetSecurityDecisionsRequest>,
    ) -> Result<Response<GetSecurityDecisionsResponse>, Status> {
        tracing::info!("get_security_decisions called");

        let decisions = self.security.decisions();

        Ok(Response::new(GetSecurityDecisionsResponse {
            decisions: decisions
                .into_iter()
                .map(|d| SecurityDecision {
                    ip: d.ip,
                    reason: d.reason,
                    action: d.action,
                    expires_unix: d.expires_unix,
                })
                .collect(),
        }))
    }

    // ── Cluster RPCs ─────────────────────────────────────────────

    async fn add_node(
        &self,
        request: Request<AddNodeRequest>,
    ) -> Result<Response<AddNodeResponse>, Status> {
        let req = request.into_inner();
        tracing::info!("add_node called: name={}, addr={}", req.name, req.addr);

        let response = self
            .cluster
            .add_node(&req.name, &req.addr, req.labels)
            .map_err(|e| Status::invalid_argument(format!("Failed to add node: {e}")))?;

        Ok(Response::new(response))
    }

    async fn remove_node(
        &self,
        request: Request<RemoveNodeRequest>,
    ) -> Result<Response<RemoveNodeResponse>, Status> {
        let req = request.into_inner();
        tracing::info!("remove_node called: name={}", req.name);

        let response = self
            .cluster
            .remove_node(&req.name)
            .map_err(|e| Status::not_found(format!("Failed to remove node: {e}")))?;

        Ok(Response::new(response))
    }

    async fn list_node(
        &self,
        _request: Request<ListNodeRequest>,
    ) -> Result<Response<ListNodeResponse>, Status> {
        tracing::info!("list_node called");

        let nodes = self
            .cluster
            .list_nodes()
            .map_err(|e| Status::internal(format!("Failed to list nodes: {e}")))?;

        let node_infos: Vec<NodeInfo> = nodes.iter().map(|n| n.to_proto()).collect();

        Ok(Response::new(ListNodeResponse { nodes: node_infos }))
    }

    async fn deploy_to_node(
        &self,
        request: Request<DeployToNodeRequest>,
    ) -> Result<Response<DeployToNodeResponse>, Status> {
        let req = request.into_inner();
        tracing::info!("deploy_to_node called: node={}", req.node_name);

        let deploy_req = req
            .deploy
            .ok_or_else(|| Status::invalid_argument("deploy request is required"))?;

        let response = self
            .cluster
            .deploy_to_node(&req.node_name, &deploy_req)
            .await
            .map_err(|e| Status::internal(format!("Failed to deploy to node: {e}")))?;

        Ok(Response::new(response))
    }

    async fn cluster_metrics(
        &self,
        _request: Request<ClusterMetricsRequest>,
    ) -> Result<Response<ClusterMetricsResponse>, Status> {
        tracing::info!("cluster_metrics called");

        let docker = self.docker.lock().await;
        let node_infos = self
            .cluster
            .collect_cluster_metrics(&docker, &self.metrics)
            .await
            .map_err(|e| Status::internal(format!("Failed to collect cluster metrics: {e}")))?;
        drop(docker);

        Ok(Response::new(ClusterMetricsResponse { nodes: node_infos }))
    }
}
