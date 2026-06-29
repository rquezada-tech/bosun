//! MCP (Model Context Protocol) server for LLM-friendly administration.
//!
//! Exposes 11 tools that AI agents (Claude Desktop, Cursor, etc.) can use
//! to administer a Bosun server without SSH. Uses JSON-RPC over SSE
//! transport via the `rmcp` crate.
//!
//! ## Tools
//!
//! 1.  `bosun_list_apps`      — List deployed apps with status, CPU, RAM, uptime
//! 2.  `bosun_deploy`         — Deploy an app from a path with domain/SSL/strategy
//! 3.  `bosun_get_metrics`    — Get CPU, RAM, network metrics for an app
//! 4.  `bosun_get_logs`       — Get recent log lines for an app
//! 5.  `bosun_restart_app`    — Restart a running app
//! 6.  `bosun_create_backup`  — Create a backup of an app's volumes + metadata
//! 7.  `bosun_list_backups`   — List backups, optionally filtered by app
//! 8.  `bosun_security_status` — Show attacks blocked and active bans
//! 9.  `bosun_gateway_status` — Show APISIX gateway info
//! 10. `bosun_cluster_nodes`  — List Docker Swarm cluster nodes
//! 11. `bosun_create_app`     — One-click app creation from a template

use std::sync::Arc;

use rmcp::{
    ErrorData as McpError,
    handler::server::wrapper::Parameters,
    model::*,
    schemars, tool, tool_router,
    handler::server::tool::ToolRouter,
    service::RoleServer,
    Service,
};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tokio::sync::Mutex;

use crate::{
    backup::BackupService,
    deploy::DeployStrategy,
    docker::DockerClient,
    gateway::GatewayClient,
    metrics::MetricCollector,
    persist::Store,
    proxy::CaddyClient,
    security::SecurityService,
    templates::Catalog,
    server::v1::AppStatus,
    health::RestartCounts,
};

// ── Shared state ────────────────────────────────────────────────────

/// Shared state accessible by all MCP tools.
#[derive(Clone)]
pub struct McpState {
    pub docker: Arc<Mutex<DockerClient>>,
    pub metrics: Arc<MetricCollector>,
    pub store: Arc<Store>,
    pub proxy: Option<Arc<CaddyClient>>,
    pub gateway: Option<GatewayClient>,
    pub restart_counts: RestartCounts,
    pub catalog: Arc<Catalog>,
    pub backup: Arc<BackupService>,
    pub security: SecurityService,
}

/// The MCP server wraps McpState and a ToolRouter.
#[derive(Clone)]
pub struct McpServer {
    pub state: McpState,
    tool_router: ToolRouter<Self>,
}

impl McpServer {
    pub fn new(state: McpState) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }
}

// ── Tool parameter structs ───────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EmptyParams {}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AppNameParams {
    /// Name of the application
    pub app_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GetLogsParams {
    /// Name of the application
    pub app_name: String,
    /// Number of log lines to return (default: 50)
    #[serde(default = "default_lines")]
    pub lines: u32,
}

fn default_lines() -> u32 { 50 }

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeployParams {
    /// Path to the application context directory on the server
    pub path: String,
    /// Domain name for reverse proxy (optional)
    #[serde(default)]
    pub domain: Option<String>,
    /// Enable SSL via Caddy/Let's Encrypt (requires domain)
    #[serde(default)]
    pub ssl: bool,
    /// Deploy strategy: "direct", "rolling", or "blue-green" (default: direct)
    #[serde(default = "default_strategy")]
    pub strategy: String,
    /// Template name if deploying a built-in template app (e.g. "redis", "postgres")
    #[serde(default)]
    pub template: Option<String>,
    /// Template version (optional, uses default if omitted)
    #[serde(default)]
    pub version: Option<String>,
}

fn default_strategy() -> String { "direct".to_string() }

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListBackupsParams {
    /// Optional app name filter (returns all backups if omitted)
    #[serde(default)]
    pub app_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CreateAppParams {
    /// Template name (e.g. "redis", "postgres", "nginx")
    pub template: String,
    /// Template version (optional, uses default if omitted)
    #[serde(default)]
    pub version: Option<String>,
    /// Domain name for reverse proxy (optional)
    #[serde(default)]
    pub domain: Option<String>,
}

// ── Tool implementation ──────────────────────────────────────────────

#[tool_router]
impl McpServer {
    /// List all deployed Bosun applications with status, CPU, RAM, and uptime.
    #[tool(description = "List all deployed Bosun applications with their status, CPU usage, RAM, and uptime")]
    async fn bosun_list_apps(
        &self,
        _params: Parameters<EmptyParams>,
    ) -> Result<CallToolResult, McpError> {
        let docker = self.state.docker.lock().await;
        let mut apps = docker.list_bosun_apps().await.map_err(|e| {
            McpError::internal_error(format!("Failed to list apps: {e}"))
        })?;
        drop(docker);

        // Inject restart counts
        let counts = self.state.restart_counts.lock().await;
        for app in &mut apps {
            if let Some(&c) = counts.get(&app.name) {
                app.restart_count = c;
            }
        }
        drop(counts);

        // Collect metrics for running apps
        let mut apps_json = Vec::new();
        for app in &apps {
            let mut entry = serde_json::json!({
                "name": app.name,
                "status": app.status().as_str_name(),
                "domain": app.domain,
                "port": app.port,
                "instances": app.instances,
                "restart_count": app.restart_count,
            });

            if app.status() == AppStatus::Running {
                match self.state.metrics.get_snapshot(&app.name).await {
                    Ok(metric) => {
                        entry["cpu_percent"] = serde_json::json!(metric.cpu_percent);
                        entry["ram_bytes"] = serde_json::json!(metric.ram_bytes);
                        entry["ram_mb"] = serde_json::json!(metric.ram_bytes / 1_048_576);
                        entry["net_rx_bytes"] = serde_json::json!(metric.net_rx_bytes);
                        entry["net_tx_bytes"] = serde_json::json!(metric.net_tx_bytes);
                    }
                    Err(e) => {
                        entry["metrics_error"] = serde_json::json!(format!("{e}"));
                    }
                }
            }

            apps_json.push(entry);
        }

        let result = serde_json::json!({
            "success": true,
            "apps": apps_json,
            "count": apps.len(),
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Deploy an application from a context path with optional domain, SSL, and strategy.
    #[tool(description = "Deploy an application from a context path. Supports domain routing, SSL via Caddy/Let's Encrypt, and deploy strategies (direct, rolling, blue-green)")]
    async fn bosun_deploy(
        &self,
        params: Parameters<DeployParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = params.0;
        let context_path = std::path::Path::new(&p.path);
        let app_name = context_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("app")
            .to_string();
        let port: u16 = 8080;
        let domain = p.domain.as_deref();
        let enable_ssl = p.ssl;
        let template_name = p.template.as_deref();
        let version = p.version.as_deref();

        // Validate SSL requires domain
        if enable_ssl && domain.is_none() {
            return Ok(CallToolResult::error(vec![Content::text(
                r#"{"success": false, "error": "SSL requires a domain name"}"#,
            )]));
        }

        // Parse strategy
        let strategy = match p.strategy.as_str() {
            "rolling" => DeployStrategy::Rolling,
            "blue-green" | "blue_green" => DeployStrategy::BlueGreen,
            _ => DeployStrategy::Direct,
        };

        // Check if it's a template deployment
        if let Some(tname) = template_name {
            let (template, resolved_image) = self.state.catalog.get_template(tname, version)
                .ok_or_else(|| McpError::invalid_params(format!(
                    "Template '{}' not found. Available: {}",
                    tname,
                    self.state.catalog.list_templates()
                        .iter()
                        .map(|t| t.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )))?;

            let docker = self.state.docker.lock().await;
            docker.deploy_template(
                &template,
                &resolved_image,
                &app_name,
                domain,
                Some(port),
            )
            .await
            .map_err(|e| McpError::internal_error(format!("Template deploy failed: {e}")))?;

            // Health check
            docker.wait_for_container_healthy(&app_name, 30).await.map_err(|e| {
                McpError::internal_error(format!("Health check failed: {e}"))
            })?;

            // Persist metadata
            let empty_env = std::collections::HashMap::new();
            let _ = self.state.store.upsert_app(&app_name, domain, Some(port as u32), &empty_env);

            // Configure reverse proxy if domain provided
            if let (Some(domain), Some(proxy)) = (domain, &self.state.proxy) {
                if let Err(e) = proxy.configure_app(domain, port).await {
                    tracing::warn!("Failed to configure Caddy for {}: {}", domain, e);
                }
            }

            // Configure security
            self.state.security.configure_app(&app_name, domain);

            let result = serde_json::json!({
                "success": true,
                "app_name": app_name,
                "status": "deployed",
                "template": tname,
                "image": resolved_image,
            });

            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&result).unwrap_or_default(),
            )]));
        }

        // Standard deploy from context path
        let docker = self.docker.lock().await;
        let empty_env = std::collections::HashMap::new();

        crate::deploy::execute_deploy(
            strategy,
            &docker,
            self.state.proxy.as_deref(),
            context_path,
            &app_name,
            domain,
            port,
            &empty_env,
        )
        .await
        .map_err(|e| McpError::internal_error(format!("Deploy failed: {e}")))?;

        // Persist metadata
        let _ = self.state.store.upsert_app(&app_name, domain, Some(port as u32), &empty_env);

        // Configure security
        self.state.security.configure_app(&app_name, domain);

        let result = serde_json::json!({
            "success": true,
            "app_name": app_name,
            "status": "deployed",
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Get CPU, RAM, and network metrics for a specific application.
    #[tool(description = "Get CPU, RAM, and network metrics for a specific application")]
    async fn bosun_get_metrics(
        &self,
        params: Parameters<AppNameParams>,
    ) -> Result<CallToolResult, McpError> {
        let app_name = &params.0.app_name;
        let metric = self.state.metrics.get_snapshot(app_name).await.map_err(|e| {
            McpError::internal_error(format!("Metrics error for '{app_name}': {e}"))
        })?;

        let result = serde_json::json!({
            "success": true,
            "app_name": metric.app_name,
            "cpu_percent": metric.cpu_percent,
            "ram_bytes": metric.ram_bytes,
            "ram_mb": metric.ram_bytes / 1_048_576,
            "net_rx_bytes": metric.net_rx_bytes,
            "net_tx_bytes": metric.net_tx_bytes,
            "timestamp_unix": metric.timestamp_unix,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Get recent log lines from an application.
    #[tool(description = "Get recent log lines from an application's container")]
    async fn bosun_get_logs(
        &self,
        params: Parameters<GetLogsParams>,
    ) -> Result<CallToolResult, McpError> {
        use futures_util::StreamExt;

        let p = params.0;
        let docker = self.state.docker.lock().await;
        let mut stream = docker.get_logs(&p.app_name, false, p.lines);

        let mut log_lines = Vec::new();
        while let Some(result) = stream.next().await {
            match result {
                Ok(entry) => {
                    log_lines.push(serde_json::json!({
                        "timestamp_unix": entry.timestamp_unix,
                        "stream": entry.stream,
                        "message": entry.message.trim_end().to_string(),
                    }));
                }
                Err(e) => {
                    log_lines.push(serde_json::json!({
                        "error": format!("{e}"),
                    }));
                }
            }
        }

        let result = serde_json::json!({
            "success": true,
            "app_name": p.app_name,
            "lines": log_lines,
            "count": log_lines.len(),
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Restart a running application.
    #[tool(description = "Restart a running application by name")]
    async fn bosun_restart_app(
        &self,
        params: Parameters<AppNameParams>,
    ) -> Result<CallToolResult, McpError> {
        let app_name = &params.0.app_name;
        let docker = self.state.docker.lock().await;
        docker.restart_container(app_name).await.map_err(|e| {
            McpError::internal_error(format!("Restart failed for '{app_name}': {e}"))
        })?;

        let result = serde_json::json!({
            "success": true,
            "app_name": app_name,
            "message": format!("App '{app_name}' restarted successfully"),
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Create a backup of an application's volumes and metadata.
    #[tool(description = "Create a backup of an application's volumes and configuration metadata")]
    async fn bosun_create_backup(
        &self,
        params: Parameters<AppNameParams>,
    ) -> Result<CallToolResult, McpError> {
        let app_name = &params.0.app_name;
        let backup = self.state.backup.create_backup(app_name).await.map_err(|e| {
            McpError::internal_error(format!("Backup failed for '{app_name}': {e}"))
        })?;

        let result = serde_json::json!({
            "success": true,
            "backup": {
                "id": backup.id,
                "app_name": backup.app_name,
                "timestamp_unix": backup.timestamp_unix,
                "size_bytes": backup.size_bytes,
                "size_mb": backup.size_bytes / 1_048_576,
            },
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// List backups, optionally filtered by application name.
    #[tool(description = "List backups, optionally filtered by application name")]
    async fn bosun_list_backups(
        &self,
        params: Parameters<ListBackupsParams>,
    ) -> Result<CallToolResult, McpError> {
        let app_filter = params.0.app_name.as_deref();
        let backups = self.state.backup.list_backups(app_filter).map_err(|e| {
            McpError::internal_error(format!("Failed to list backups: {e}"))
        })?;

        let backup_entries: Vec<JsonValue> = backups
            .iter()
            .map(|b| {
                serde_json::json!({
                    "id": b.id,
                    "app_name": b.app_name,
                    "timestamp_unix": b.timestamp_unix,
                    "size_bytes": b.size_bytes,
                    "size_mb": b.size_bytes / 1_048_576,
                })
            })
            .collect();

        let result = serde_json::json!({
            "success": true,
            "backups": backup_entries,
            "count": backups.len(),
            "filter": app_filter,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Get security status: attacks blocked and active bans.
    #[tool(description = "Get security status showing attacks blocked and active bans from CrowdSec or Fail2Ban")]
    async fn bosun_security_status(
        &self,
        _params: Parameters<EmptyParams>,
    ) -> Result<CallToolResult, McpError> {
        let stats = self.state.security.status();
        let decisions = self.state.security.decisions();

        let decision_entries: Vec<JsonValue> = decisions
            .iter()
            .map(|d| {
                serde_json::json!({
                    "ip": d.ip,
                    "reason": d.reason,
                    "action": d.action,
                    "expires_unix": d.expires_unix,
                })
            })
            .collect();

        let result = serde_json::json!({
            "success": true,
            "engine": stats.engine.as_str(),
            "attacks_blocked": stats.attacks_blocked,
            "active_bans": stats.active_bans,
            "active_decisions": decision_entries,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Get APISIX API Gateway status.
    #[tool(description = "Get APISIX API Gateway status and route information")]
    async fn bosun_gateway_status(
        &self,
        _params: Parameters<EmptyParams>,
    ) -> Result<CallToolResult, McpError> {
        let result = match &self.state.gateway {
            Some(gw) => {
                let status = gw.get_status().await;
                let routes = gw.list_routes().await;

                let mut resp = serde_json::json!({
                    "success": true,
                    "gateway": "APISIX",
                    "enabled": true,
                });

                match status {
                    Ok(info) => {
                        resp["version"] = serde_json::json!(info.version);
                        resp["uptime"] = serde_json::json!(info.uptime);
                    }
                    Err(e) => {
                        resp["status_error"] = serde_json::json!(format!("{e}"));
                    }
                }

                match routes {
                    Ok(route_list) => {
                        let route_entries: Vec<JsonValue> = route_list
                            .iter()
                            .map(|r| {
                                serde_json::json!({
                                    "name": r.name,
                                    "domain": r.domain,
                                    "port": r.port,
                                    "uri": r.uri,
                                    "plugins": r.plugins,
                                })
                            })
                            .collect();
                        resp["routes"] = serde_json::json!(route_entries);
                        resp["route_count"] = serde_json::json!(route_list.len());
                    }
                    Err(e) => {
                        resp["routes_error"] = serde_json::json!(format!("{e}"));
                    }
                }

                resp
            }
            None => {
                serde_json::json!({
                    "success": true,
                    "gateway": "none",
                    "enabled": false,
                    "message": "APISIX API Gateway is not configured. Run APISIX via Docker to enable.",
                })
            }
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// List Docker Swarm cluster nodes.
    #[tool(description = "List Docker Swarm cluster nodes with their roles and status")]
    async fn bosun_cluster_nodes(
        &self,
        _params: Parameters<EmptyParams>,
    ) -> Result<CallToolResult, McpError> {
        let docker = self.state.docker.lock().await;
        let nodes = docker.list_nodes().map_err(|e| {
            McpError::internal_error(format!("Failed to list cluster nodes: {e}"))
        })?;

        let node_entries: Vec<JsonValue> = nodes
            .iter()
            .map(|n| {
                serde_json::json!({
                    "id": n.id,
                    "hostname": n.hostname,
                    "role": n.role,
                    "availability": n.availability,
                    "status": n.status,
                    "addr": n.addr,
                })
            })
            .collect();

        let result = serde_json::json!({
            "success": true,
            "nodes": node_entries,
            "count": nodes.len(),
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Create an application from a built-in template (one-click).
    #[tool(description = "Create an application from a built-in template with one click (e.g., redis, postgres, nginx)")]
    async fn bosun_create_app(
        &self,
        params: Parameters<CreateAppParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = params.0;
        let template_name = &p.template;
        let version = p.version.as_deref();
        let domain = p.domain.as_deref();

        let (template, resolved_image) = self.state.catalog.get_template(template_name, version)
            .ok_or_else(|| McpError::invalid_params(format!(
                "Template '{}' not found. Available: {}",
                template_name,
                self.state.catalog.list_templates()
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )))?;

        let app_name = template_name.to_string();

        let docker = self.state.docker.lock().await;
        docker.deploy_template(
            &template,
            &resolved_image,
            &app_name,
            domain,
            Some(template.default_port),
        )
        .await
        .map_err(|e| McpError::internal_error(format!("Template deploy failed: {e}")))?;

        // Health check
        docker.wait_for_container_healthy(&app_name, 30).await.map_err(|e| {
            McpError::internal_error(format!("Health check failed: {e}"))
        })?;

        // Persist metadata
        let empty_env = std::collections::HashMap::new();
        let _ = self.state.store.upsert_app(&app_name, domain, Some(template.default_port as u32), &empty_env);

        // Configure reverse proxy if domain provided
        if let (Some(domain), Some(proxy)) = (domain, &self.state.proxy) {
            if let Err(e) = proxy.configure_app(domain, template.default_port).await {
                tracing::warn!("Failed to configure Caddy for {}: {}", domain, e);
            }
        }

        // Configure security
        self.state.security.configure_app(&app_name, domain);

        let result = serde_json::json!({
            "success": true,
            "app_name": app_name,
            "template": template_name,
            "image": resolved_image,
            "port": template.default_port,
            "category": template.category.as_str(),
            "description": template.description,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }
}

// ── ServerHandler implementation ─────────────────────────────────────

use rmcp::handler::server::ServerHandler;
use rmcp::model::ServerInfo;

#[rmcp::handler::server::tool_router]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            name: Some("bosun-mcp".into()),
            version: Some(env!("CARGO_PKG_VERSION", "0.1.0").into()),
            protocol_version: Some(Default::default()),
            instructions: Some(
                "Bosun MCP Server — administer your VPS without SSH.\n\
                 Use these tools to deploy apps, check metrics, view logs, \n\
                 manage backups, monitor security, and control the API gateway."
                    .into(),
            ),
            ..Default::default()
        }
    }
}

// ── Server launcher ──────────────────────────────────────────────────

use rmcp::transport::sse_server::{SseServer, SseServerConfig};
use std::net::SocketAddr;
use tokio_util::sync::CancellationToken;

/// Launch the MCP SSE server on the given bind address.
///
/// If `api_key` is provided, the server will validate the `X-API-Key`
/// header on all requests. If not set, the server should only listen
/// on 127.0.0.1 (local only).
pub async fn serve_mcp(
    bind: SocketAddr,
    state: McpState,
    _api_key: Option<String>,
    ct: CancellationToken,
) -> anyhow::Result<()> {
    let server = McpServer::new(state);

    let config = SseServerConfig {
        bind,
        sse_path: "/sse".to_string(),
        post_path: "/message".to_string(),
        ct,
        sse_keep_alive: Some(std::time::Duration::from_secs(30)),
    };

    let (sse_server, router) = SseServer::new(config);

    // Build the axum router with optional API key auth middleware
    let router = if let Some(ref key) = _api_key {
        let expected_key = key.clone();
        router.layer(axum::middleware::from_fn(move |request: axum::extract::Request, next: axum::middleware::Next| {
            let expected = expected_key.clone();
            async move {
                let api_key = request
                    .headers()
                    .get("X-API-Key")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");

                if api_key != expected {
                    return axum::response::Response::builder()
                        .status(axum::http::StatusCode::UNAUTHORIZED)
                        .body(axum::body::Body::from(
                            r#"{"error": "Unauthorized: invalid or missing X-API-Key header"}"#,
                        ))
                        .unwrap();
                }

                next.run(request).await
            }
        }))
    } else {
        router
    };

    let listener = tokio::net::TcpListener::bind(bind).await?;
    let axum_ct = sse_server.config.ct.child_token();

    tracing::info!("MCP server listening on {} (SSE)", bind);
    if _api_key.is_some() {
        tracing::info!("MCP API key authentication enabled");
    } else {
        tracing::info!("MCP server running without API key (local-only mode)");
    }

    // Start the SSE server transport handler
    sse_server.with_service(move || server.clone());

    // Serve axum
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            axum_ct.cancelled().await;
            tracing::info!("MCP server shutting down");
        })
        .await?;

    Ok(())
}
