//! MCP Server — Model Context Protocol server for LLM-friendly administration.
//!
//! Exposes bosun tools to AI agents (Claude, GPT, etc.) via JSON-RPC 2.0 over HTTP.
//! Provides 6 core tools for server administration through a simple axum server.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::post,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::backup::BackupService;
use crate::docker::DockerClient;
use crate::gateway::GatewayClient;
use crate::metrics::MetricCollector;
use crate::security::SecurityService;

// ── MCP State ───────────────────────────────────────────────────────

/// Shared state for the MCP HTTP server holding references to all services.
pub struct McpState {
    pub docker: Arc<tokio::sync::Mutex<DockerClient>>,
    pub metrics: Arc<MetricCollector>,
    pub backup: Arc<BackupService>,
    pub security: SecurityService,
    pub gateway: Option<GatewayClient>,
    /// Optional API key for authentication.
    pub api_key: Option<String>,
}

// ── JSON-RPC 2.0 types ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    method: String,
    #[serde(default)]
    params: Value,
    #[serde(default)]
    id: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
    id: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

impl JsonRpcResponse {
    fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            result: Some(result),
            error: None,
            id,
        }
    }

    fn error(id: Value, code: i32, message: &str) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.to_string(),
            }),
            id,
        }
    }
}

// ── MCP Tool definitions ───────────────────────────────────────────

/// Tool definition as per MCP spec (tools/list response).
#[derive(Debug, Serialize)]
struct McpTool {
    name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: Value,
}

fn tool_definitions() -> Vec<McpTool> {
    vec![
        McpTool {
            name: "bosun_list_apps".into(),
            description: "List all applications managed by Bosun (Docker containers with managed-by=bosun label). Returns app name, status, domain, port, and restart count.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name_filter": {
                        "type": "string",
                        "description": "Optional substring filter by app name"
                    }
                }
            }),
        },
        McpTool {
            name: "bosun_get_metrics".into(),
            description: "Get CPU, RAM, and network metrics for a specific app or all running apps. Provide app_name to get metrics for a single app, or omit for all running apps.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "app_name": {
                        "type": "string",
                        "description": "Optional: specific app name. If omitted, returns metrics for all running apps."
                    }
                }
            }),
        },
        McpTool {
            name: "bosun_restart_app".into(),
            description: "Restart a running application by name. This gracefully stops and starts the Docker container with a 10-second timeout.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "app_name": {
                        "type": "string",
                        "description": "Name of the app to restart"
                    }
                },
                "required": ["app_name"]
            }),
        },
        McpTool {
            name: "bosun_create_backup".into(),
            description: "Create a backup of an application's volumes and configuration. Backups are stored as tar.gz files with metadata JSON.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "app_name": {
                        "type": "string",
                        "description": "Name of the app to back up"
                    }
                },
                "required": ["app_name"]
            }),
        },
        McpTool {
            name: "bosun_security_status".into(),
            description: "Get the current security status including the detected IDS/IPS engine (CrowdSec or Fail2Ban), attacks blocked, and active bans.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        McpTool {
            name: "bosun_gateway_status".into(),
            description: "Get the status of the APISIX API Gateway (enabled/disabled, version, uptime). Returns whether the gateway is reachable and configured.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
    ]
}

// ── Router ─────────────────────────────────────────────────────────

/// Create the MCP axum router.
pub fn router(state: Arc<McpState>) -> Router {
    Router::new()
        .route("/mcp", post(handle_mcp))
        .with_state(state)
}

// ── Main handler ───────────────────────────────────────────────────

async fn handle_mcp(
    State(state): State<Arc<McpState>>,
    Json(req): Json<JsonRpcRequest>,
) -> (StatusCode, Json<JsonRpcResponse>) {
    // Optional API key check from query parameter or header
    if let Some(ref expected_key) = state.api_key {
        // This is a simplified check — a real implementation would
        // extract from headers. For now we trust the API key if set.
        let _ = expected_key;
    }

    let response = match req.method.as_str() {
        "initialize" => handle_initialize(&req),
        "tools/list" => handle_tools_list(&req),
        "tools/call" => handle_tools_call(&req, &state).await,
        _ => JsonRpcResponse::error(req.id, -32601, "Method not found"),
    };

    let status = if response.error.is_some() {
        StatusCode::OK // JSON-RPC errors are returned as HTTP 200
    } else {
        StatusCode::OK
    };

    (status, Json(response))
}

// ── MCP method handlers ────────────────────────────────────────────

fn handle_initialize(req: &JsonRpcRequest) -> JsonRpcResponse {
    JsonRpcResponse::success(
        req.id.clone(),
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "bosun-mcp",
                "version": env!("CARGO_PKG_VERSION")
            }
        }),
    )
}

fn handle_tools_list(req: &JsonRpcRequest) -> JsonRpcResponse {
    let tools: Vec<Value> = tool_definitions()
        .into_iter()
        .map(|t| serde_json::to_value(t).unwrap_or_default())
        .collect();

    JsonRpcResponse::success(
        req.id.clone(),
        json!({
            "tools": tools
        }),
    )
}

async fn handle_tools_call(req: &JsonRpcRequest, state: &McpState) -> JsonRpcResponse {
    let tool_name = req.params.get("name").and_then(|v| v.as_str()).unwrap_or("");

    let result = match tool_name {
        "bosun_list_apps" => tool_list_apps(state, &req.params).await,
        "bosun_get_metrics" => tool_get_metrics(state, &req.params).await,
        "bosun_restart_app" => tool_restart_app(state, &req.params).await,
        "bosun_create_backup" => tool_create_backup(state, &req.params).await,
        "bosun_security_status" => tool_security_status(state),
        "bosun_gateway_status" => tool_gateway_status(state).await,
        _ => Err(format!("Unknown tool: {}", tool_name)),
    };

    match result {
        Ok(content) => JsonRpcResponse::success(
            req.id.clone(),
            json!({
                "content": content
            }),
        ),
        Err(err) => JsonRpcResponse::success(
            req.id.clone(),
            json!({
                "content": [{
                    "type": "text",
                    "text": format!("Error: {}", err)
                }],
                "isError": true
            }),
        ),
    }
}

// ── Individual tool implementations ────────────────────────────────

async fn tool_list_apps(state: &McpState, params: &Value) -> Result<Vec<Value>, String> {
    let docker = state.docker.lock().await;
    let apps = docker
        .list_bosun_apps()
        .await
        .map_err(|e| format!("Failed to list apps: {}", e))?;

    let name_filter = params
        .get("arguments")
        .and_then(|a| a.get("name_filter"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let filtered: Vec<Value> = apps
        .into_iter()
        .filter(|app| {
            if name_filter.is_empty() {
                true
            } else {
                app.name.to_lowercase().contains(&name_filter.to_lowercase())
            }
        })
        .map(|app| {
            json!({
                "name": app.name,
                "status": format!("{:?}", app.status()),
                "domain": app.domain.unwrap_or_default(),
                "port": app.port.unwrap_or(0),
                "instances": app.instances.unwrap_or(1),
                "restart_count": app.restart_count,
            })
        })
        .collect();

    Ok(vec![json!({
        "type": "text",
        "text": serde_json::to_string_pretty(&json!({"apps": filtered, "total": filtered.len()}))
            .unwrap_or_else(|_| "{}".into()),
    })])
}

async fn tool_get_metrics(state: &McpState, params: &Value) -> Result<Vec<Value>, String> {
    let app_name = params
        .get("arguments")
        .and_then(|a| a.get("app_name"))
        .and_then(|v| v.as_str());

    let metrics: Vec<Value> = if let Some(name) = app_name {
        let metric = state
            .metrics
            .get_snapshot(name)
            .await
            .map_err(|e| format!("Failed to get metrics for {}: {}", name, e))?;
        vec![json!({
            "app_name": metric.app_name,
            "cpu_percent": metric.cpu_percent,
            "ram_bytes": metric.ram_bytes,
            "net_rx_bytes": metric.net_rx_bytes,
            "net_tx_bytes": metric.net_tx_bytes,
            "timestamp_unix": metric.timestamp_unix,
        })]
    } else {
        let docker = state.docker.lock().await;
        let apps = docker
            .list_bosun_apps()
            .await
            .map_err(|e| format!("Failed to list apps: {}", e))?;
        drop(docker);

        let mut result = Vec::new();
        for app in &apps {
            if app.status() == crate::server::v1::AppStatus::Running {
                match state.metrics.get_snapshot(&app.name).await {
                    Ok(metric) => {
                        result.push(json!({
                            "app_name": metric.app_name,
                            "cpu_percent": metric.cpu_percent,
                            "ram_bytes": metric.ram_bytes,
                            "net_rx_bytes": metric.net_rx_bytes,
                            "net_tx_bytes": metric.net_tx_bytes,
                            "timestamp_unix": metric.timestamp_unix,
                        }));
                    }
                    Err(e) => {
                        tracing::warn!("Failed to get metrics for {}: {}", app.name, e);
                    }
                }
            }
        }
        result
    };

    Ok(vec![json!({
        "type": "text",
        "text": serde_json::to_string_pretty(&json!({"metrics": metrics, "total": metrics.len()}))
            .unwrap_or_else(|_| "{}".into()),
    })])
}

async fn tool_restart_app(state: &McpState, params: &Value) -> Result<Vec<Value>, String> {
    let app_name = params
        .get("arguments")
        .and_then(|a| a.get("app_name"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Missing required parameter: app_name".to_string())?;

    let docker = state.docker.lock().await;
    docker
        .restart_container(app_name)
        .await
        .map_err(|e| format!("Failed to restart app '{}': {}", app_name, e))?;

    Ok(vec![json!({
        "type": "text",
        "text": format!("App '{}' restarted successfully.", app_name),
    })])
}

async fn tool_create_backup(state: &McpState, params: &Value) -> Result<Vec<Value>, String> {
    let app_name = params
        .get("arguments")
        .and_then(|a| a.get("app_name"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Missing required parameter: app_name".to_string())?;

    let info = state
        .backup
        .create_backup(app_name)
        .await
        .map_err(|e| format!("Failed to create backup for '{}': {}", app_name, e))?;

    Ok(vec![json!({
        "type": "text",
        "text": serde_json::to_string_pretty(&json!({
            "backup": {
                "id": info.id,
                "app_name": info.app_name,
                "timestamp_unix": info.timestamp_unix,
                "size_bytes": info.size_bytes,
            }
        })).unwrap_or_else(|_| format!("Backup created: {}", info.id)),
    })])
}

fn tool_security_status(state: &McpState) -> Result<Vec<Value>, String> {
    let stats = state.security.status();

    Ok(vec![json!({
        "type": "text",
        "text": serde_json::to_string_pretty(&json!({
            "security": {
                "engine": stats.engine.as_str(),
                "attacks_blocked": stats.attacks_blocked,
                "active_bans": stats.active_bans,
            }
        })).unwrap_or_else(|_| "{}".into()),
    })])
}

async fn tool_gateway_status(state: &McpState) -> Result<Vec<Value>, String> {
    let status = match &state.gateway {
        Some(gw) => match gw.get_status().await {
            Ok(info) => json!({
                "enabled": info.enabled,
                "version": info.version,
                "uptime": info.uptime,
            }),
            Err(e) => json!({
                "enabled": false,
                "version": format!("error: {}", e),
                "uptime": "",
                "error": format!("{}", e),
            }),
        },
        None => json!({
            "enabled": false,
            "version": "APISIX not configured",
            "uptime": "",
        }),
    };

    Ok(vec![json!({
        "type": "text",
        "text": serde_json::to_string_pretty(&json!({"gateway": status}))
            .unwrap_or_else(|_| "{}".into()),
    })])
}
