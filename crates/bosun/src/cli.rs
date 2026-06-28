//! CLI argument definitions and command implementations for Bosun.

use anyhow::Context;
use clap::{Parser, Subcommand};
use crate::client::BosunClient;

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

    /// Output as JSON instead of formatted tables
    #[arg(long, global = true)]
    pub json: bool,

    /// Disable colored output
    #[arg(long, global = true)]
    pub no_color: bool,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// Connect to the daemon and dispatch the subcommand.
    pub async fn run(self) -> anyhow::Result<()> {
        let mut client = BosunClient::connect(
            &self.daemon,
            self.cert.as_deref(),
            self.key.as_deref(),
        )
        .await?;

        match &self.command {
            Command::Apps { sub } => self.run_apps(&mut client, sub).await,
            Command::Deploy { path, domain, ssl } => {
                self.run_deploy(&mut client, path, domain.as_deref(), *ssl).await
            }
            Command::Metrics { app, live } => {
                self.run_metrics(&mut client, app.as_deref(), *live).await
            }
            Command::Env { sub } => self.run_env(&mut client, sub).await,
            Command::Config { sub } => self.run_config(sub).await,
        }
    }
}

// ── Output helpers ────────────────────────────────────────────────

/// ANSI color codes (only used when `no_color` is false).
mod color {
    pub const GREEN: &str = "\x1b[32m";
    pub const RED: &str = "\x1b[31m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const CYAN: &str = "\x1b[36m";
    pub const BOLD: &str = "\x1b[1m";
    pub const RESET: &str = "\x1b[0m";
}

fn status_color(status: i32, no_color: bool) -> (&'static str, &'static str, &'static str) {
    if no_color {
        return ("", "", "");
    }
    match status {
        1 => (color::GREEN, color::BOLD, color::RESET),  // RUNNING
        2 => (color::RED, "", color::RESET),              // STOPPED
        3 => (color::YELLOW, "", color::RESET),           // DEPLOYING
        4 => (color::RED, color::BOLD, color::RESET),     // FAILED
        _ => ("", "", ""),                                 // UNKNOWN
    }
}

fn status_label(status: i32) -> &'static str {
    match status {
        1 => "Running",
        2 => "Stopped",
        3 => "Deploying",
        4 => "Failed",
        _ => "Unknown",
    }
}

// ── Command implementations ───────────────────────────────────────

impl Cli {
    // ── Apps ──────────────────────────────────────────────────────

    async fn run_apps(&self, client: &mut BosunClient, sub: &AppsCmd) -> anyhow::Result<()> {
        match sub {
            AppsCmd::List => self.apps_list(client).await,
            AppsCmd::Logs { app, follow, tail } => {
                self.apps_logs(client, &app, follow, tail).await
            }
            AppsCmd::Restart { app } => self.apps_restart(client, &app).await,
            AppsCmd::Scale { app, instances } => self.apps_scale(client, &app, instances).await,
        }
    }

    async fn apps_list(&self, client: &mut BosunClient) -> anyhow::Result<()> {
        let apps = client.list_apps().await?;

        if self.json {
            let json = serde_json::to_string_pretty(&serde_json::json!({
                "apps": apps.iter().map(|a| serde_json::json!({
                    "name": a.name,
                    "status": status_label(a.status),
                    "domain": a.domain,
                    "port": a.port,
                    "instances": a.instances,
                })).collect::<Vec<_>>(),
            }))?;
            println!("{json}");
            return Ok(());
        }

        if apps.is_empty() {
            println!("No apps deployed.");
            return Ok(());
        }

        use tabled::{Table, Tabled};
        #[derive(Tabled)]
        struct AppRow {
            #[tabled(rename = "APP")]
            name: String,
            #[tabled(rename = "STATUS")]
            status: String,
            #[tabled(rename = "DOMAIN")]
            domain: String,
            #[tabled(rename = "PORT")]
            port: String,
        }

        let rows: Vec<AppRow> = apps
            .iter()
            .map(|a| {
                let (pre, bold, reset) = status_color(a.status, self.no_color);
                let status_str = format!("{pre}{bold}{}{reset}", status_label(a.status));
                let domain = a.domain.as_deref().unwrap_or("-");
                let port_str = a.port.map(|p| p.to_string()).unwrap_or_else(|| "-".to_string());
                AppRow {
                    name: a.name.clone(),
                    status: status_str,
                    domain: domain.to_string(),
                    port: port_str,
                }
            })
            .collect();

        let table = Table::new(rows);
        println!("{table}");
        Ok(())
    }

    async fn apps_logs(
        &self,
        client: &mut BosunClient,
        app: &str,
        follow: bool,
        tail: u32,
    ) -> anyhow::Result<()> {
        use futures_util::StreamExt;
        let mut stream = client.get_logs(app, follow, tail).await?;

        while let Some(entry) = stream.next().await {
            let entry = entry.context("Failed to read log entry from stream")?;
            let ts = chrono_human(entry.timestamp_unix);
            let stream_label = if entry.stream == "stderr" {
                if self.no_color {
                    "[stderr]"
                } else {
                    &format!("{}[stderr]{}", color::YELLOW, color::RESET)
                }
            } else {
                "[stdout]"
            };
            if self.json {
                let json = serde_json::to_string(&serde_json::json!({
                    "timestamp_unix": entry.timestamp_unix,
                    "stream": entry.stream,
                    "message": entry.message,
                }))?;
                println!("{json}");
            } else {
                println!("{ts} {stream_label} {}", entry.message.trim_end());
            }
        }
        Ok(())
    }

    async fn apps_restart(&self, client: &mut BosunClient, app: &str) -> anyhow::Result<()> {
        client.restart_app(app).await?;
        if self.json {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "action": "restart",
                "app": app,
                "status": "ok"
            }))?);
        } else {
            println!("✔ App '{app}' restarted successfully.");
        }
        Ok(())
    }

    async fn apps_scale(
        &self,
        client: &mut BosunClient,
        app: &str,
        instances: u32,
    ) -> anyhow::Result<()> {
        client.scale_app(app, instances).await?;
        if self.json {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "action": "scale",
                "app": app,
                "instances": instances,
                "status": "ok"
            }))?);
        } else {
            println!("✔ App '{app}' scaled to {instances} instance(s).");
        }
        Ok(())
    }

    // ── Deploy ────────────────────────────────────────────────────

    async fn run_deploy(
        &self,
        client: &mut BosunClient,
        path: &str,
        domain: Option<&str>,
        ssl: bool,
    ) -> anyhow::Result<()> {
        if !self.json {
            eprintln!("🚀 Deploying from '{path}'...");
        }

        let response = client
            .deploy(
                path,
                domain,
                ssl,
                std::collections::HashMap::new(),
                None,
            )
            .await?;

        if self.json {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "app_name": response.app_name,
                "status": response.status,
            }))?);
        } else {
            let domain_info = domain.map(|d| format!(" at https://{d}")).unwrap_or_default();
            println!(
                "✔ Deployed '{}' successfully{} (status: {})",
                response.app_name, domain_info, response.status
            );
        }
        Ok(())
    }

    // ── Metrics ───────────────────────────────────────────────────

    async fn run_metrics(
        &self,
        client: &mut BosunClient,
        app: Option<&str>,
        live: bool,
    ) -> anyhow::Result<()> {
        if live {
            use futures_util::StreamExt;
            let mut stream = client.stream_metrics(app).await?;
            let app_label = app.unwrap_or("all");

            if !self.json {
                eprintln!("📊 Live metrics for '{app_label}' (Ctrl+C to stop)");
            }

            while let Some(metric) = stream.next().await {
                let metric = metric.context("Failed to read metric from stream")?;
                if self.json {
                    println!("{}", serde_json::to_string(&serde_json::json!({
                        "app_name": metric.app_name,
                        "cpu_percent": metric.cpu_percent,
                        "ram_bytes": metric.ram_bytes,
                        "net_rx_bytes": metric.net_rx_bytes,
                        "net_tx_bytes": metric.net_tx_bytes,
                        "timestamp_unix": metric.timestamp_unix,
                    }))?);
                } else {
                    let ram_mb = metric.ram_bytes as f64 / 1_048_576.0;
                    let rx_kb = metric.net_rx_bytes as f64 / 1024.0;
                    let tx_kb = metric.net_tx_bytes as f64 / 1024.0;
                    // Use \r to update in-place for live view
                    print!(
                        "\r\x1b[K  {:<20}  CPU: {:>5.1}%  RAM: {:>6.1} MB  NET RX: {:>7.1} KB  NET TX: {:>7.1} KB",
                        metric.app_name, metric.cpu_percent, ram_mb, rx_kb, tx_kb
                    );
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }
            }
        } else {
            let metrics = client.get_metrics(app).await?;

            if self.json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                    "metrics": metrics.iter().map(|m| serde_json::json!({
                        "app_name": m.app_name,
                        "cpu_percent": m.cpu_percent,
                        "ram_bytes": m.ram_bytes,
                        "net_rx_bytes": m.net_rx_bytes,
                        "net_tx_bytes": m.net_tx_bytes,
                        "timestamp_unix": m.timestamp_unix,
                    })).collect::<Vec<_>>(),
                }))?);
                return Ok(());
            }

            if metrics.is_empty() {
                println!("No metrics available.");
                return Ok(());
            }

            use tabled::{Table, Tabled};
            #[derive(Tabled)]
            struct MetricRow {
                #[tabled(rename = "APP")]
                name: String,
                #[tabled(rename = "CPU%")]
                cpu: String,
                #[tabled(rename = "RAM")]
                ram: String,
                #[tabled(rename = "NET RX")]
                rx: String,
                #[tabled(rename = "NET TX")]
                tx: String,
            }

            let rows: Vec<MetricRow> = metrics
                .iter()
                .map(|m| {
                    let ram_mb = m.ram_bytes as f64 / 1_048_576.0;
                    let rx_kb = m.net_rx_bytes as f64 / 1024.0;
                    let tx_kb = m.net_tx_bytes as f64 / 1024.0;
                    MetricRow {
                        name: m.app_name.clone(),
                        cpu: format!("{:.1}%", m.cpu_percent),
                        ram: format!("{:.1} MB", ram_mb),
                        rx: format!("{:.1} KB", rx_kb),
                        tx: format!("{:.1} KB", tx_kb),
                    }
                })
                .collect();

            let table = Table::new(rows);
            println!("{table}");
        }
        Ok(())
    }

    // ── Env ───────────────────────────────────────────────────────

    async fn run_env(&self, client: &mut BosunClient, sub: &EnvCmd) -> anyhow::Result<()> {
        match sub {
            EnvCmd::List { app } => self.env_list(client, &app).await,
            EnvCmd::Set { app, key, value } => self.env_set(client, &app, &key, &value).await,
            EnvCmd::Unset { app, key } => self.env_unset(client, &app, &key).await,
        }
    }

    async fn env_list(&self, client: &mut BosunClient, app: &str) -> anyhow::Result<()> {
        let env = client.get_env(app).await?;

        if self.json {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "app": app,
                "env": env,
            }))?);
            return Ok(());
        }

        if env.is_empty() {
            println!("No environment variables set for '{app}'.");
            return Ok(());
        }

        println!("Environment for '{app}':");
        let mut keys: Vec<&String> = env.keys().collect();
        keys.sort();
        for key in keys {
            let value = &env[key];
            println!("  {key}={value}");
        }
        Ok(())
    }

    async fn env_set(
        &self,
        client: &mut BosunClient,
        app: &str,
        key: &str,
        value: &str,
    ) -> anyhow::Result<()> {
        client.set_env(app, key, value).await?;
        if self.json {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "action": "set_env",
                "app": app,
                "key": key,
                "value": value,
                "status": "ok"
            }))?);
        } else {
            println!("✔ Set {key}={value} for '{app}'.");
        }
        Ok(())
    }

    async fn env_unset(
        &self,
        client: &mut BosunClient,
        app: &str,
        key: &str,
    ) -> anyhow::Result<()> {
        client.unset_env(app, key).await?;
        if self.json {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "action": "unset_env",
                "app": app,
                "key": key,
                "status": "ok"
            }))?);
        } else {
            println!("✔ Unset {key} from '{app}'.");
        }
        Ok(())
    }

    // ── Config ────────────────────────────────────────────────────

    async fn run_config(&self, sub: &ConfigCmd) -> anyhow::Result<()> {
        match sub {
            ConfigCmd::Show => self.config_show().await,
            ConfigCmd::Set { key, value } => self.config_set(&key, &value).await,
        }
    }

    async fn config_show(&self) -> anyhow::Result<()> {
        if self.json {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "daemon": self.daemon,
                "tls": self.cert.is_some(),
                "cert": self.cert,
            }))?);
        } else {
            println!("Bosun CLI Configuration:");
            println!("  Daemon:  {}", self.daemon);
            if let Some(ref cert) = self.cert {
                println!("  TLS Cert: {cert}");
            }
            if let Some(ref key) = self.key {
                println!("  TLS Key:  {key}");
            }
            println!("  No Color: {}", self.no_color);
            println!("  JSON:     {}", self.json);
        }
        Ok(())
    }

    async fn config_set(&self, _key: &str, _value: &str) -> anyhow::Result<()> {
        anyhow::bail!(
            "Remote config is not yet supported by the daemon. \
             Use environment variables or CLI flags instead."
        );
    }
}

// ── Subcommand types ──────────────────────────────────────────────

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
        /// Number of lines to show from the end
        #[arg(long, default_value = "100")]
        tail: u32,
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

// ── Helpers ───────────────────────────────────────────────────────

/// Convert a Unix timestamp (seconds) to a human-readable string.
fn chrono_human(ts: u64) -> String {
    // Simple formatting without pulling in chrono crate
    let secs = ts;
    let mins = secs / 60;
    let hours = mins / 60;
    let days = hours / 24;

    let time_secs = secs % 86400;
    let h = time_secs / 3600;
    let m = (time_secs % 3600) / 60;
    let s = time_secs % 60;

    if days > 0 {
        format!("{days}d {h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}
