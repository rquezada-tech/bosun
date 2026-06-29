//! Bosun CLI — terminal-based PaaS orchestration.
//!
//! Commands:
//!   apps list|logs|restart|scale
//!   deploy PATH --domain NAME [--ssl]
//!   metrics APP [--live]
//!   env set|list APP KEY [VALUE]
//!   config show|set

use clap::Parser;

mod cli;
mod client;
mod proto;
mod dashboard;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "bosun=info".into()),
        )
        .init();

    let cli = cli::Cli::parse();
    cli.run().await
}
