//! CLI argument definitions and command implementations for Bosun.

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use crate::client::BosunClient;
use crate::proto::bosun::v1::DeployStrategy;

/// CLI argument enum for deploy strategies (maps to proto DeployStrategy).
#[derive(Clone, Debug, ValueEnum)]
pub enum StrategyArg {
    /// Direct deploy: build, stop old, start new
    Direct,
    /// Rolling deploy: gradually replace instances
    Rolling,
    /// Blue-green deploy: maintain two environments, switch traffic
    BlueGreen,
}

impl From<StrategyArg> for DeployStrategy {
    fn from(arg: StrategyArg) -> Self {
        match arg {
            StrategyArg::Direct => DeployStrategy::Direct,
            StrategyArg::Rolling => DeployStrategy::Rolling,
            StrategyArg::BlueGreen => DeployStrategy::BlueGreen,
        }
    }
}

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
            Command::Deploy { path, domain, ssl, strategy } => {
                let deploy_strategy: DeployStrategy = strategy.clone().into();
                self.run_deploy(&mut client, path, domain.as_deref(), *ssl, &deploy_strategy).await
            }
            Command::Metrics { app, live } => {
                self.run_metrics(&mut client, app.as_deref(), *live).await
            }
            Command::Env { sub } => self.run_env(&mut client, sub).await,
            Command::Config { sub } => self.run_config(sub).await,
            Command::Rollback { app } => {
                self.run_rollback(&mut client, app).await
            }
            Command::Login { username, password } => {
                self.run_login(&mut client, username, password.as_deref()).await
            }
            Command::Logout => self.run_logout().await,
            Command::Whoami => self.run_whoami().await,
            Command::Backup { sub } => self.run_backup(&mut client, sub).await,
        }
    }
}

// ── Output helpers ────────────────────────────────────────────────

/// ANSI color codes (only used when `no_color` is false).
mod color {
    pub const GREEN: &str = "\x1b[32m";
    pub const RED: &str = "\x1b[31m";
    pub const YELLOW: &str = "\x1b[33m";
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
                self.apps_logs(client, app, *follow, *tail).await
            }
            AppsCmd::Restart { app } => self.apps_restart(client, app).await,
            AppsCmd::Scale { app, instances } => self.apps_scale(client, app, *instances).await,
            AppsCmd::Templates => self.apps_templates(client).await,
            AppsCmd::Create {
                template_name,
                name,
                version,
            } => self.apps_create(client, template_name, name.as_deref(), version.as_deref()).await,
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

    // ── One-Click Apps ─────────────────────────────────────────────

    /// List available one-click app templates.
    async fn apps_templates(&self, client: &mut BosunClient) -> anyhow::Result<()> {
        let templates = client.list_templates().await?;

        if self.json {
            let json = serde_json::to_string_pretty(&serde_json::json!({
                "templates": templates.iter().map(|t| serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "category": t.category,
                    "default_port": t.default_port,
                    "versions": t.versions,
                    "icon": t.icon,
                })).collect::<Vec<_>>(),
            }))?;
            println!("{json}");
            return Ok(());
        }

        if templates.is_empty() {
            println!("No templates available.");
            return Ok(());
        }

        use tabled::{Table, Tabled};
        #[derive(Tabled)]
        struct TemplateRow {
            #[tabled(rename = "NAME")]
            name: String,
            #[tabled(rename = "VERSIONS")]
            versions: String,
            #[tabled(rename = "DESCRIPTION")]
            description: String,
            #[tabled(rename = "CATEGORY")]
            category: String,
            #[tabled(rename = "PORT")]
            port: String,
        }

        let rows: Vec<TemplateRow> = templates
            .iter()
            .map(|t| TemplateRow {
                name: t.name.clone(),
                versions: t.versions.join(", "),
                description: t.description.clone(),
                category: t.category.clone(),
                port: t.default_port.to_string(),
            })
            .collect();

        let table = Table::new(rows);
        println!("{table}");
        Ok(())
    }

    /// Create an app from a one-click template.
    async fn apps_create(
        &self,
        client: &mut BosunClient,
        template_name: &str,
        name_override: Option<&str>,
        version: Option<&str>,
    ) -> anyhow::Result<()> {
        // Validate template exists by listing templates
        let templates = client.list_templates().await?;
        let template = templates
            .iter()
            .find(|t| t.name == template_name);

        if template.is_none() {
            let available: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
            anyhow::bail!(
                "Unknown template '{}'. Available templates: {}\n\
                 Run 'bosun apps templates' to see the full list with descriptions.",
                template_name,
                available.join(", ")
            );
        }

        let template = template.unwrap();
        let app_name = name_override.unwrap_or(template_name);
        let version_label = version.unwrap_or("default");

        if !self.json {
            eprintln!(
                "🚀 Creating '{}' from template '{}' (version: {}, port: {})...",
                app_name, template_name, version_label, template.default_port
            );
        }

        // Pass version request through env var (server resolves it)
        let mut env = std::collections::HashMap::new();
        if let Some(v) = version {
            env.insert("BOSUN_TEMPLATE_VERSION".to_string(), v.to_string());
        }

        let response = client
            .deploy(
                template_name,
                None,
                false,
                env,
                Some(template.default_port),
                DeployStrategy::Direct,
            )
            .await?;

        if self.json {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "app_name": response.app_name,
                "template": template_name,
                "status": response.status,
            }))?);
        } else {
            println!(
                "✔ Created '{}' from template '{}' successfully (status: {})",
                response.app_name, template_name, response.status
            );
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
        strategy: &DeployStrategy,
    ) -> anyhow::Result<()> {
        let strategy_label = strategy_label(strategy);
        if !self.json {
            eprintln!("🚀 Deploying {path} (strategy: {strategy_label})...");
        }

        let response = client
            .deploy(
                path,
                domain,
                ssl,
                std::collections::HashMap::new(),
                None,
                *strategy,
            )
            .await?;

        if self.json {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "app_name": response.app_name,
                "status": response.status,
                "strategy": strategy_label,
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

    // ── Rollback ──────────────────────────────────────────────────

    async fn run_rollback(
        &self,
        client: &mut BosunClient,
        app: &str,
    ) -> anyhow::Result<()> {
        if !self.json {
            eprintln!("⏪ Rolling back '{app}'...");
        }

        let response = client.rollback_app(app).await?;

        if self.json {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "app": app,
                "status": response.status,
                "message": response.message,
            }))?);
        } else {
            match response.status.as_str() {
                "rolled_back" => {
                    println!("✔ {}", response.message);
                }
                "not_available" => {
                    println!("⚠ {}", response.message);
                }
                _ => {
                    println!("{}: {}", response.status, response.message);
                }
            }
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
            EnvCmd::List { app } => self.env_list(client, app).await,
            EnvCmd::Set { app, key, value } => self.env_set(client, app, key, value).await,
            EnvCmd::Unset { app, key } => self.env_unset(client, app, key).await,
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
            ConfigCmd::Set { key, value } => self.config_set(key, value).await,
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

    // ── Auth ───────────────────────────────────────────────────────

    async fn run_login(
        &self,
        client: &mut BosunClient,
        username: &str,
        password: Option<&str>,
    ) -> anyhow::Result<()> {
        let password = match password {
            Some(p) => p.to_string(),
            None => {
                // Prompt for password
                eprint!("Password: ");
                use std::io::Write;
                let _ = std::io::stderr().flush();
                let mut input = String::new();
                std::io::stdin()
                    .read_line(&mut input)
                    .context("Failed to read password")?;
                input.trim().to_string()
            }
        };

        let response = client.login(username, &password).await?;
        client.save_credentials(&response.token, &response.username, &response.role)?;

        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "action": "login",
                    "username": response.username,
                    "role": response.role,
                    "status": "ok"
                }))?
            );
        } else {
            println!("✔ Logged in as '{}' (role: {})", response.username, response.role);
        }
        Ok(())
    }

    async fn run_logout(&self) -> anyhow::Result<()> {
        let creds_path = BosunClient::credentials_path();
        if creds_path.exists() {
            std::fs::remove_file(&creds_path)
                .context("Failed to remove credentials file")?;
        }

        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "action": "logout",
                    "status": "ok"
                }))?
            );
        } else {
            println!("✔ Logged out. Credentials removed.");
        }
        Ok(())
    }

    async fn run_whoami(&self) -> anyhow::Result<()> {
        let creds = BosunClient::load_credentials()?;

        match creds {
            Some(creds) => {
                // Decode the JWT without signature validation to extract claims
                // (the server validates; client-side just reads for display)
                use jsonwebtoken::DecodingKey;
                let header = jsonwebtoken::decode_header(&creds.token)
                    .context("Failed to decode token header")?;

                // Decode the payload without verifying signature
                // Use a dummy key and skip validation
                let mut validation = jsonwebtoken::Validation::default();
                validation.insecure_disable_signature_validation();
                validation.validate_exp = false; // Show expired tokens too

                let token_data = jsonwebtoken::decode::<serde_json::Value>(
                    &creds.token,
                    &DecodingKey::from_secret(b""),
                    &validation,
                );

                if self.json {
                    let mut json_out = serde_json::json!({
                        "logged_in": true,
                        "token_file": BosunClient::credentials_path().display().to_string(),
                        "token_algorithm": format!("{:?}", header.alg),
                        "username": creds.username,
                        "role": creds.role,
                    });
                    if let Ok(ref data) = token_data {
                        if let Some(exp) = data.claims.get("exp").and_then(|v| v.as_u64()) {
                            json_out["expires_at_unix"] = serde_json::json!(exp);
                            let dt = chrono::DateTime::from_timestamp(exp as i64, 0);
                            if let Some(dt) = dt {
                                json_out["expires_at"] = serde_json::json!(dt.to_rfc3339());
                            }
                        }
                        if let Some(iat) = data.claims.get("iat").and_then(|v| v.as_u64()) {
                            json_out["issued_at_unix"] = serde_json::json!(iat);
                        }
                    }
                    println!("{}", serde_json::to_string_pretty(&json_out)?);
                } else {
                    println!("✔ Logged in");
                    println!("  Username:  {}", creds.username);
                    println!("  Role:      {}", creds.role);
                    match token_data {
                        Ok(data) => {
                            if let Some(exp) = data.claims.get("exp").and_then(|v| v.as_u64()) {
                                let dt = chrono::DateTime::from_timestamp(exp as i64, 0);
                                match dt {
                                    Some(dt) => {
                                        let now = chrono::Utc::now();
                                        if dt > now {
                                            let remaining = dt - now;
                                            let hours = remaining.num_hours();
                                            let mins = remaining.num_minutes() % 60;
                                            println!("  Expires:   {} (in {}h {}m)", dt.format("%Y-%m-%d %H:%M:%S UTC"), hours, mins);
                                        } else {
                                            println!("  Expired:   {} (token has expired)", dt.format("%Y-%m-%d %H:%M:%S UTC"));
                                        }
                                    }
                                    None => println!("  Expires:   timestamp={}", exp),
                                }
                            }
                        }
                        Err(_) => {
                            println!("  (Unable to decode token payload — use 'bosun login' to re-authenticate)");
                        }
                    }
                    println!("  Token:     {}", BosunClient::credentials_path().display());
                }
            }
            None => {
                if self.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "logged_in": false
                        }))?
                    );
                } else {
                    println!("Not logged in. Use 'bosun login' to authenticate.");
                }
            }
        }
        Ok(())
    }

    // ── Backup ──────────────────────────────────────────────────────

    async fn run_backup(&self, client: &mut BosunClient, sub: &BackupCmd) -> anyhow::Result<()> {
        match sub {
            BackupCmd::Create { app } => self.backup_create(client, app).await,
            BackupCmd::List { app } => self.backup_list(client, app.as_deref()).await,
            BackupCmd::Restore { backup_id } => self.backup_restore(client, backup_id).await,
        }
    }

    async fn backup_create(&self, client: &mut BosunClient, app: &str) -> anyhow::Result<()> {
        if !self.json {
            eprintln!("\u{1f4e6} Creating backup for '{app}'...");
        }

        let response = client.create_backup(app).await?;
        let backup = response
            .backup
            .ok_or_else(|| anyhow::anyhow!("No backup info returned"))?;

        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "action": "backup_create",
                    "id": backup.id,
                    "app_name": backup.app_name,
                    "timestamp_unix": backup.timestamp_unix,
                    "size_bytes": backup.size_bytes,
                    "status": "ok"
                }))?
            );
        } else {
            let size_mb = backup.size_bytes as f64 / 1_048_576.0;
            let ts = chrono_human(backup.timestamp_unix);
            println!(
                "\u{2705} Backup '{}' created for '{}' ({} -- {:.1} MB)",
                backup.id, backup.app_name, ts, size_mb
            );
        }
        Ok(())
    }

    async fn backup_list(
        &self,
        client: &mut BosunClient,
        app: Option<&str>,
    ) -> anyhow::Result<()> {
        let backups = client.list_backups(app).await?;

        if self.json {
            let json = serde_json::to_string_pretty(&serde_json::json!({
                "backups": backups.iter().map(|b| serde_json::json!({
                    "id": b.id,
                    "app_name": b.app_name,
                    "timestamp_unix": b.timestamp_unix,
                    "size_bytes": b.size_bytes,
                })).collect::<Vec<_>>(),
            }))?;
            println!("{json}");
            return Ok(());
        }

        if backups.is_empty() {
            println!("No backups found.");
            return Ok(());
        }

        use tabled::{Table, Tabled};
        #[derive(Tabled)]
        struct BackupRow {
            #[tabled(rename = "BACKUP ID")]
            id: String,
            #[tabled(rename = "APP")]
            app: String,
            #[tabled(rename = "TIMESTAMP")]
            timestamp: String,
            #[tabled(rename = "SIZE")]
            size: String,
        }

        let rows: Vec<BackupRow> = backups
            .iter()
            .map(|b| {
                let size_mb = b.size_bytes as f64 / 1_048_576.0;
                let ts = chrono::DateTime::from_timestamp(b.timestamp_unix as i64, 0)
                    .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_else(|| chrono_human(b.timestamp_unix));
                BackupRow {
                    id: b.id.clone(),
                    app: b.app_name.clone(),
                    timestamp: ts,
                    size: format!("{:.1} MB", size_mb),
                }
            })
            .collect();

        let table = Table::new(rows);
        println!("{table}");
        Ok(())
    }

    async fn backup_restore(
        &self,
        client: &mut BosunClient,
        backup_id: &str,
    ) -> anyhow::Result<()> {
        if !self.json {
            eprintln!("\u{1f504} Restoring backup '{backup_id}'...");
        }

        let response = client.restore_backup(backup_id).await?;

        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "action": "backup_restore",
                    "backup_id": backup_id,
                    "app_name": response.app_name,
                    "status": response.status,
                }))?
            );
        } else {
            println!(
                "\u{2705} Backup '{backup_id}' restored successfully for app '{}' (status: {})",
                response.app_name, response.status
            );
        }
        Ok(())
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
        /// Enable HTTPS via Let's Encrypt (requires Caddy and a public domain)
        #[arg(long)]
        ssl: bool,
        /// Deploy strategy (direct, rolling, or blue-green)
        #[arg(long, value_enum, default_value = "direct")]
        strategy: StrategyArg,
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
    /// Rollback an app (blue-green switch or no-op for direct/rolling)
    Rollback {
        /// App name to rollback
        app: String,
    },
    /// Log in to the bosun daemon
    Login {
        /// Username
        username: String,
        /// Password (prompts if not provided)
        #[arg(long)]
        password: Option<String>,
    },
    /// Log out (remove saved credentials)
    Logout,
    /// Show current login info
    Whoami,
    /// Manage backups (create, list, restore)
    Backup {
        #[command(subcommand)]
        sub: BackupCmd,
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
    /// Create an app from a one-click template (e.g., redis, postgres)
    Create {
        /// Template name (use 'bosun apps templates' to list)
        template_name: String,
        /// Custom app name (defaults to template name if omitted)
        #[arg(long)]
        name: Option<String>,
        /// Specific version to deploy (defaults to the template's default version)
        #[arg(long)]
        version: Option<String>,
    },
    /// List available one-click app templates
    Templates,
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

#[derive(Subcommand)]
pub enum BackupCmd {
    /// Create a backup of an app's volumes and configuration
    Create {
        /// App name to back up
        app: String,
    },
    /// List all backups with timestamps and sizes
    List {
        /// Optional: filter by app name
        #[arg(long)]
        app: Option<String>,
    },
    /// Restore a backup by ID
    Restore {
        /// Backup ID to restore (from `bosun backup list`)
        backup_id: String,
    },
}

// ── Helpers ───────────────────────────────────────────────────────

/// Convert a DeployStrategy to a human-readable string.
fn strategy_label(strategy: &DeployStrategy) -> &'static str {
    match strategy {
        DeployStrategy::Direct => "direct",
        DeployStrategy::Rolling => "rolling",
        DeployStrategy::BlueGreen => "blue-green",
    }
}

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
