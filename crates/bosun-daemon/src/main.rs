//! Bosun daemon — lightweight PaaS orchestrator.
//!
//! Responsibilities:
//!   - gRPC server for CLI communication
//!   - Docker orchestration via bollard
//!   - Metric collection and storage
//!   - Reverse proxy config generation (nginx/caddy)
//!   - SSL certificate management (Let's Encrypt)

use clap::Parser;
use std::path::PathBuf;
use tokio::signal;
use tonic::transport::server::ServerTlsConfig;
use tonic::transport::{Identity, Server};

mod server;
mod docker;
mod metrics;
mod proxy;
mod persist;

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

    // Create the gRPC service
    let bosun_service = server::BosunService::new(docker, store);

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

    // Graceful shutdown on SIGTERM/SIGINT
    let shutdown_signal = async {
        let _ = signal::ctrl_c().await;
        tracing::info!("Shutdown signal received, draining connections...");
    };

    router
        .serve_with_shutdown(addr, shutdown_signal)
        .await?;

    tracing::info!("Server shut down gracefully");
    Ok(())
}
