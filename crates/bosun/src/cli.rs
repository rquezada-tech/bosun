//! CLI argument definitions for Bosun.

use clap::{Parser, Subcommand};

/// Bosun — minimal PaaS orchestration from your terminal.
#[derive(Parser)]
#[command(name = "bosun", version, about)]
pub struct Cli {
    /// Address of the bosun daemon (e.g., "https://my-server:9090")
    #[arg(short, long, env = "BOSUN_DAEMON", default_value = "https://localhost:9090")]
    pub daemon: String,

    /// TLS client certificate for mTLS
    #[arg(long, env = "BOSUN_CERT")]
    pub cert: Option<String>,

    /// TLS client key for mTLS
    #[arg(long, env = "BOSUN_KEY")]
    pub key: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    pub async fn run(self) -> anyhow::Result<()> {
        match self.command {
            Command::Apps { .. } => todo!("apps command"),
            Command::Deploy { .. } => todo!("deploy command"),
            Command::Metrics { .. } => todo!("metrics command"),
            Command::Env { .. } => todo!("env command"),
            Command::Config { .. } => todo!("config command"),
        }
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Manage deployed applications
    Apps {
        #[command(subcommand)]
        sub: AppsCmd,
    },
    /// Deploy an application
    Deploy {
        /// Path to the app source (Dockerfile dir or docker-compose.yml)
        path: String,
        /// Domain name for the app
        #[arg(long)]
        domain: Option<String>,
        /// Enable Let's Encrypt SSL
        #[arg(long)]
        ssl: bool,
    },
    /// Show resource metrics for apps
    Metrics {
        /// App name (omit for all)
        app: Option<String>,
        /// Live-updating view
        #[arg(long)]
        live: bool,
    },
    /// Manage environment variables
    Env {
        #[command(subcommand)]
        sub: EnvCmd,
    },
    /// View or change daemon configuration
    Config {
        #[command(subcommand)]
        sub: ConfigCmd,
    },
}

#[derive(Subcommand)]
pub enum AppsCmd {
    /// List all apps with status
    List,
    /// Show logs for an app
    Logs {
        app: String,
        /// Follow log output
        #[arg(long)]
        follow: bool,
    },
    /// Restart an app
    Restart { app: String },
    /// Scale an app to N instances
    Scale { app: String, instances: u32 },
}

#[derive(Subcommand)]
pub enum EnvCmd {
    /// List environment variables for an app
    List { app: String },
    /// Set an environment variable
    Set {
        app: String,
        key: String,
        value: String,
    },
    /// Remove an environment variable
    Unset { app: String, key: String },
}

#[derive(Subcommand)]
pub enum ConfigCmd {
    /// Show current configuration
    Show,
    /// Set a configuration value
    Set { key: String, value: String },
}
