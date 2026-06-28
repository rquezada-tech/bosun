//! Bosun daemon — lightweight PaaS orchestrator.
//!
//! Responsibilities:
//!   - gRPC server for CLI communication
//!   - Docker orchestration via bollard
//!   - Metric collection and storage
//!   - Reverse proxy config generation (nginx/caddy)
//!   - SSL certificate management (Let's Encrypt)
//!   - Webhook HTTP server for git push auto-deploy

use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::signal;
use tonic::transport::server::ServerTlsConfig;
use tonic::transport::{Identity, Server};

mod server;
mod docker;
mod health;
mod metrics;
mod proxy;
mod persist;
mod templates;
mod webhook;

/// Bosun daemon arguments.
#[derive(Parser)]
#[command(name = "bosun-daemon", version)]
struct Args {
    /// gRPC listen address
    #[arg(long, default_value = "0.0.0.0:9090")]
    listen: String,

    /// TLS certificate path
    #[arg(long)]
    cert: Option<String>,

    /// TLS key path
    #[arg(long)]
    key: Option<String>,

    /// Data directory for persistent state
    #[arg(long, default_value = "/var/lib/bosun")]
    data_dir: String,

    /// Webhook HTTP server listen address
    #[arg(long, default_value = "0.0.0.0:9091", env = "BOSUN_WEBHOOK_LISTEN")]
    webhook_listen: String,

    /// Webhook shared secret for authentication
    #[arg(long, env = "BOSUN_WEBHOOK_SECRET")]
    webhook_secret: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "bosun_daemon=info".into()),
        )
        .init();

    let args = Args::parse();

    tracing::info!("Starting bosun-daemon...");
    tracing::info!(
        "Docker socket: {}",
        std::env::var("DOCKER_HOST").unwrap_or_else(|_| "/var/run/docker.sock".into())
    );
    tracing::info!("Data directory: {}", args.data_dir);

    // Initialize data directory
    std::fs::create_dir_all(&args.data_dir)?;

    // Initialize persistence store
    let db_path = PathBuf::from(&args.data_dir).join("bosun.db");
    let store = persist::Store::open(&db_path)?;
    tracing::info!("Persistence store opened at {}", db_path.display());

    // Connect to Docker
    let docker = docker::DockerClient::connect().await?;

    // Clone DockerClient for the webhook server before moving the original
    // into the shared Arc for gRPC + health checker.
    let docker_webhook = docker.clone();

    // Clone the inner bollard handle for metrics (needs a raw Docker ref).
    let docker_inner = docker.inner.clone();

    // Wrap DockerClient in Arc<Mutex<>> so it can be shared between
    // the gRPC service and the health checker.
    let docker_arc = Arc::new(tokio::sync::Mutex::new(docker));

    // Start the health checker daemon (30s interval, rate-limited auto-restart).
    let health_checker = health::HealthChecker::new(docker_arc.clone(), 30);
    let restart_counts = health_checker.restart_counts.clone();
    health_checker.start();

    // Create metric collector (shares the Docker connection)
    let metrics = metrics::MetricCollector::new(docker_inner);

    // Create the gRPC service
    let proxy = match proxy::CaddyClient::new().await {
        Ok(client) => {
            tracing::info!("Caddy reverse proxy integration enabled");
            Some(client)
        }
        Err(e) => {
            tracing::warn!(
                "Caddy Admin API unreachable at http://localhost:2019: {}. \
                 Reverse proxy disabled — apps will not receive HTTP traffic via domain. \
                 Install Caddy and ensure the Admin API is enabled to use domain-based routing.",
                e
            );
            None
        }
    };

    let bosun_service =
        server::BosunService::new(docker_arc, metrics, store, proxy, restart_counts);

    // Build the gRPC server
    let addr = args.listen.parse()?;
    let mut builder = Server::builder();

    // Configure TLS if cert and key are provided
    if let (Some(cert_path), Some(key_path)) = (&args.cert, &args.key) {
        tracing::info!("TLS enabled: cert={}, key={}", cert_path, key_path);
        let cert = tokio::fs::read(cert_path).await?;
        let key = tokio::fs::read(key_path).await?;
        let identity = Identity::from_pem(cert, key);
        builder = builder.tls_config(ServerTlsConfig::new().identity(identity))?;
    } else {
        tracing::warn!("TLS disabled — running without encryption");
    }

    let router = builder
        .add_service(server::v1::bosun_server::BosunServer::new(bosun_service));

    tracing::info!("gRPC server listening on {}", args.listen);

    // Start webhook HTTP server in a separate tokio task
    let webhook_docker = Arc::new(tokio::sync::Mutex::new(docker_webhook));
    let webhook_secret = args.webhook_secret.unwrap_or_default();
    if webhook_secret.is_empty() {
        tracing::warn!(
            "Webhook secret not set — webhook auth is disabled (dev mode). \
             Set --webhook-secret or BOSUN_WEBHOOK_SECRET for production."
        );
    }
    let webhook_server =
        webhook::WebhookServer::new(args.webhook_listen, webhook_docker, webhook_secret);
    let webhook_handle = tokio::spawn(async move {
        if let Err(e) = webhook_server.serve().await {
            tracing::error!("Webhook server exited with error: {e}");
        }
    });

    // Graceful shutdown on SIGTERM/SIGINT
    let shutdown_signal = async {
        let _ = signal::ctrl_c().await;
        tracing::info!("Shutdown signal received, draining connections...");
    };

    router
        .serve_with_shutdown(addr, shutdown_signal)
        .await?;

    // Cancel webhook server when gRPC shuts down
    webhook_handle.abort();
    tracing::info!("Webhook server stopped");

    tracing::info!("Server shut down gracefully");
    Ok(())
}
