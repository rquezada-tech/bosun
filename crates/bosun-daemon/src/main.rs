//! Bosun daemon — lightweight PaaS orchestrator.
//!
//! Responsibilities:
//!   - gRPC server for CLI communication
//!   - Docker orchestration via bollard
//!   - Metric collection and storage
//!   - Reverse proxy config generation (nginx/caddy)
//!   - SSL certificate management (Let's Encrypt)

use clap::Parser;

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

    let _args = Args::parse();

    tracing::info!("Starting bosun-daemon...");
    tracing::info!("Docker socket: {}", std::env::var("DOCKER_HOST").unwrap_or_else(|_| "/var/run/docker.sock".into()));

    // TODO: Initialize gRPC server, Docker client, metric collector

    Ok(())
}
