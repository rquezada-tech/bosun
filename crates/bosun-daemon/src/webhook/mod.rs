//! Webhook HTTP server for git push auto-deploy.
//!
//! Listens for HTTP POST webhook requests (e.g., GitHub push events)
//! and triggers redeployment of the matching Bosun-managed app.
//! Uses secret-based auth via the `X-Bosun-Secret` header.
//!
//! Routes:
//!   - POST /hooks/:app_name     — triggers redeploy of app_name
//!   - GET  /hooks/:app_name/health — returns 200 if app is running
//!   - GET  /health              — returns 200 OK (webhook server is alive)

use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::docker::DockerClient;

/// Shared application state for the webhook server.
#[derive(Clone)]
pub struct WebhookState {
    /// Docker client (shared with the gRPC server).
    pub docker: Arc<Mutex<DockerClient>>,
    /// Shared secret for webhook authentication.
    pub secret: String,
}

/// The Bosun webhook HTTP server.
///
/// Runs alongside the gRPC server and listens for webhook
/// events that trigger auto-redeployment of matching apps.
pub struct WebhookServer {
    /// Address to listen on (e.g., `0.0.0.0:9091`).
    listen_addr: String,
    /// Application state (Docker client + secret).
    state: WebhookState,
}

impl WebhookServer {
    /// Create a new webhook server bound to `listen_addr`.
    pub fn new(listen_addr: String, docker: Arc<Mutex<DockerClient>>, secret: String) -> Self {
        Self {
            listen_addr,
            state: WebhookState { docker, secret },
        }
    }

    /// Start the webhook server. Runs until the process is terminated.
    pub async fn serve(self) -> anyhow::Result<()> {
        let app = Router::new()
            .route("/hooks/{app_name}", post(handle_webhook_post))
            .route("/hooks/{app_name}/health", get(handle_app_health))
            .route("/health", get(handle_server_health))
            .with_state(self.state);

        let listener = tokio::net::TcpListener::bind(&self.listen_addr).await?;
        tracing::info!("Webhook server listening on http://{}", self.listen_addr);

        axum::serve(listener, app).await?;
        Ok(())
    }
}

/// Validates the `X-Bosun-Secret` header against the configured secret.
fn validate_secret(headers: &HeaderMap, secret: &str) -> bool {
    if secret.is_empty() {
        // If no secret is configured, accept all requests (dev mode).
        return true;
    }
    match headers.get("x-bosun-secret") {
        Some(value) => {
            let value = value.to_str().unwrap_or_default();
            // Constant-time comparison to prevent timing attacks
            constant_time_eq(value.as_bytes(), secret.as_bytes())
        }
        None => false,
    }
}

/// Constant-time byte comparison (prevents timing side-channel attacks).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// POST /hooks/:app_name — triggers redeploy of the matching app.
async fn handle_webhook_post(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    Path(app_name): Path<String>,
    body: Body,
) -> Response {
    // Validate secret
    if !validate_secret(&headers, &state.secret) {
        return (
            StatusCode::UNAUTHORIZED,
            "invalid or missing X-Bosun-Secret header",
        )
            .into_response();
    }

    // Read the body for logging (webhook payload, e.g., GitHub push event)
    let body_bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("failed to read request body: {e}"),
            )
                .into_response();
        }
    };
    let body_str = String::from_utf8_lossy(&body_bytes);

    tracing::info!(
        "Webhook received for app '{}' ({} bytes of payload)",
        app_name,
        body_bytes.len()
    );
    tracing::debug!("Webhook payload: {}", body_str);

    // Trigger redeploy
    let docker = state.docker.lock().await;
    match docker.redeploy(&app_name).await {
        Ok(()) => {
            tracing::info!("App '{}' redeployed successfully via webhook", app_name);
            (StatusCode::OK, format!("ok: app '{}' redeployed\n", app_name)).into_response()
        }
        Err(e) => {
            tracing::error!("Webhook redeploy failed for '{}': {e}", app_name);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("redeploy failed: {e}\n"),
            )
                .into_response()
        }
    }
}

/// GET /hooks/:app_name/health — returns 200 if the app container is running.
async fn handle_app_health(
    State(state): State<WebhookState>,
    Path(app_name): Path<String>,
) -> Response {
    let docker = state.docker.lock().await;
    match docker.inspect_container(&app_name).await {
        Ok(info) => {
            let running = info
                .state
                .as_ref()
                .and_then(|s| s.running)
                .unwrap_or(false);
            if running {
                (
                    StatusCode::OK,
                    format!("healthy: app '{}' is running\n", app_name),
                )
                    .into_response()
            } else {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("unhealthy: app '{}' is not running\n", app_name),
                )
                    .into_response()
            }
        }
        Err(_) => (
            StatusCode::NOT_FOUND,
            format!("not found: app '{}' does not exist\n", app_name),
        )
            .into_response(),
    }
}

/// GET /health — returns 200 OK (webhook server is alive).
async fn handle_server_health() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}
