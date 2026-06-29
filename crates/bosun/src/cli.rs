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
            Command::Deploy { path, domain, ssl, strategy, pre_hooks, post_hooks } => {
                let deploy_strategy: DeployStrategy = strategy.clone().into();
                self.run_deploy(&mut client, path, domain.as_deref(), *ssl, &deploy_strategy, pre_hooks, post_hooks).await
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
            Command::Gateway { .. } | Command::Security { .. } => {
                anyhow::bail!("Command not yet available in this build")
            }
            Command::Cluster { sub } => self.run_cluster(sub).await,
            Command::Dashboard => {
                use crate::dashboard::Dashboard;
                let mut dashboard = Dashboard::new(client);
                dashboard.run()?;
                Ok(())
            }
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
        let templates = client.list_templates().await?;
        let template = templates.iter().find(|t| t.name == template_name);

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
                &[],
                &[],
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
        pre_hooks: &[String],
        post_hooks: &[String],
    ) -> anyhow::Result<()> {
        let strategy_label = strategy_label(strategy);

        // Auto-detect bosun.hooks.toml if present in the build directory
        let mut all_pre_hooks = pre_hooks.to_vec();
        let mut all_post_hooks = post_hooks.to_vec();

        let hooks_path = std::path::Path::new(path).join("bosun.hooks.toml");
        if hooks_path.exists() {
            match std::fs::read_to_string(&hooks_path) {
                Ok(content) => {
                    match toml::from_str::<toml::Table>(&content) {
                        Ok(table) => {
                            let mut found = false;
                            if let Some(pre) = table.get("pre_deploy").and_then(|v| v.get("commands")).and_then(|v| v.as_array()) {
                                let cmds: Vec<String> = pre.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
                                if !cmds.is_empty() {
                                    if !self.json {
                                        eprintln!("📋 Found {cnt} pre-deploy hook(s) in bosun.hooks.toml", cnt = cmds.len());
                                    }
                                    all_pre_hooks.extend(cmds);
                                    found = true;
                                }
                            }
                            if let Some(post) = table.get("post_deploy").and_then(|v| v.get("commands")).and_then(|v| v.as_array()) {
                                let cmds: Vec<String> = post.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
                                if !cmds.is_empty() {
                                    if !self.json {
                                        eprintln!("📋 Found {cnt} post-deploy hook(s) in bosun.hooks.toml", cnt = cmds.len());
                                    }
                                    all_post_hooks.extend(cmds);
                                    found = true;
                                }
                            }
                            if !found && !self.json {
                                eprintln!("📋 bosun.hooks.toml found but no hooks defined");
                            }
                        }
                        Err(e) => {
                            if !self.json {
                                eprintln!("⚠ Failed to parse bosun.hooks.toml: {e}");
                            }
                        }
                    }
                }
                Err(e) => {
                    if !self.json {
                        eprintln!("⚠ Failed to read bosun.hooks.toml: {e}");
                    }
                }
            }
        }

        if !all_pre_hooks.is_empty() && !self.json {
            eprintln!("🔧 Will run {} pre-deploy hook(s)", all_pre_hooks.len());
        }
        if !all_post_hooks.is_empty() && !self.json {
            eprintln!("🔧 Will run {} post-deploy hook(s)", all_post_hooks.len());
        }

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
                &all_pre_hooks,
                &all_post_hooks,
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
                let token_data = jsonwebtoken::decode_header(&creds.token)
                    .context("Failed to decode token header")?;

                if self.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "logged_in": true,
                            "token_file": creds_path_display(),
                            "token_algorithm": format!("{:?}", token_data.alg),
                        }))?
                    );
                } else {
                    let creds_path = BosunClient::credentials_path();
                    println!("✔ Logged in");
                    println!("  Token file: {}", creds_path.display());
                    println!("  Token algorithm: {:?}", token_data.alg);
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
            eprintln!("📦 Creating backup for '{app}'...");
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
                "✅ Backup '{}' created for '{}' ({} -- {:.1} MB)",
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
            eprintln!("🔄 Restoring backup '{backup_id}'...");
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
                "✅ Backup '{backup_id}' restored successfully for app '{}' (status: {})",
                response.app_name, response.status
            );
        }
        Ok(())
    }
    // ── Gateway ────────────────────────────────────────────────────

    async fn run_gateway(
        &self,
        client: &mut BosunClient,
        sub: &GatewayCmd,
    ) -> anyhow::Result<()> {
        match sub {
            GatewayCmd::Status => self.gateway_status(client).await,
            GatewayCmd::Routes => self.gateway_routes(client).await,
            GatewayCmd::Plugin {
                app,
                plugin,
                config,
            } => self.gateway_plugin(client, app, plugin, config.as_deref()).await,
            GatewayCmd::Cache { sub } => self.gateway_cache(client, sub).await,
            GatewayCmd::Metrics => self.gateway_metrics(client).await,
        }
    }

    async fn gateway_status(&self, client: &mut BosunClient) -> anyhow::Result<()> {
        let status = client.get_gateway_status().await?;

        if self.json {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "gateway_status": "ok",
                "message": format!("{:?}", status.status),
            }))?);
            return Ok(());
        }

        let enabled = format!("{:?}", status.status) == "Some(Running)";
        let version = "unknown";

        if enabled {
            println!("✔ APISIX Gateway: ENABLED");
            println!("  Version: {version}");
        } else {
            println!("✘ APISIX Gateway: DISABLED");
            println!("  {version}");
            println!("\n  To enable APISIX, start it via Docker:");
            println!("    docker run -d --name apisix --network bosun \\");
            println!("      -p 9080:9080 -p 9180:9180 apache/apisix");
        }
        Ok(())
    }

    async fn gateway_routes(&self, client: &mut BosunClient) -> anyhow::Result<()> {
        let resp = client.list_gateway_routes().await?;
        let routes = resp.routes;

        if self.json {
            let json = serde_json::to_string_pretty(&serde_json::json!({
                "route_count": routes.len(),
            }))?;
            println!("{json}");
            return Ok(());
        }

        if routes.is_empty() {
            println!("No APISIX routes configured.");
            return Ok(());
        }

        use tabled::{Table, Tabled};
        #[derive(Tabled)]
        struct RouteRow {
            #[tabled(rename = "APP")]
            name: String,
            #[tabled(rename = "DOMAIN")]
            domain: String,
            #[tabled(rename = "PORT")]
            port: String,
            #[tabled(rename = "URI")]
            uri: String,
            #[tabled(rename = "PLUGINS")]
            plugins: String,
        }

        let rows: Vec<RouteRow> = routes
            .iter()
            .map(|r| RouteRow {
                name: r.name.clone(),
                domain: r.domain.clone(),
                port: r.port.to_string(),
                uri: r.uri.clone(),
                plugins: r.plugins.join(", "),
            })
            .collect();

        let table = Table::new(rows);
        println!("{table}");
        Ok(())
    }

    async fn gateway_plugin(
        &self,
        client: &mut BosunClient,
        app: &str,
        plugin: &str,
        config: Option<&str>,
    ) -> anyhow::Result<()> {
        let config_json = config.unwrap_or("{}");
        client
            .enable_gateway_plugin(app, plugin, config_json)
            .await?;

        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "action": "enable_plugin",
                    "app": app,
                    "plugin": plugin,
                    "status": "ok"
                }))?
            );
        } else {
            println!("✔ Plugin '{plugin}' enabled for app '{app}'.");
        }
        Ok(())
    }

    async fn gateway_cache(
        &self,
        client: &mut BosunClient,
        sub: &CacheCmd,
    ) -> anyhow::Result<()> {
        match sub {
            CacheCmd::Enable { app, ttl } => {
                // Enable cache by enabling proxy-cache plugin
                let config = serde_json::json!({
                    "cache_ttl": ttl,
                    "cache_strategy": "disk",
                    "cache_http_status": [200, 301, 302],
                    "cache_method": ["GET", "HEAD"],
                });
                client
                    .enable_gateway_plugin(app, "proxy-cache", &config.to_string())
                    .await?;

                if self.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "action": "cache_enable",
                            "app": app,
                            "ttl_secs": ttl,
                            "status": "ok"
                        }))?
                    );
                } else {
                    println!("✔ Cache enabled for '{app}' (TTL: {ttl}s).");
                }
            }
            CacheCmd::Disable { app } => {
                client.disable_gateway_plugin(app, "proxy-cache").await?;

                if self.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "action": "cache_disable",
                            "app": app,
                            "status": "ok"
                        }))?
                    );
                } else {
                    println!("✔ Cache disabled for '{app}'.");
                }
            }
            CacheCmd::Stats { app } => {
                let stats = client.get_gateway_cache_stats(app).await?;

                if self.json {
                    let s = stats.stats.as_ref();
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "cache_stats": "ok",
                            "hits": s.map(|s| s.hits).unwrap_or(0),
                            "misses": s.map(|s| s.misses).unwrap_or(0),
                            "size_bytes": s.map(|s| s.size_bytes).unwrap_or(0),
                        }))?
                    );
                    return Ok(());
                }

                let s = stats.stats.as_ref();
                let hits = s.map(|s| s.hits).unwrap_or(0);
                let misses = s.map(|s| s.misses).unwrap_or(0);
                let size_mb = s.map(|s| s.size_bytes).unwrap_or(0) as f64 / 1_048_576.0;

                println!("Cache stats for '{app}':");
                println!("  Hits:   {hits}");
                println!("  Misses: {misses}");
                println!("  Size:   {:.1} MB", size_mb);
            }
            CacheCmd::Purge { app } => {
                client.purge_gateway_cache(app).await?;

                if self.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "action": "cache_purge",
                            "app": app,
                            "status": "ok"
                        }))?
                    );
                } else {
                    println!("✔ Cache purged for '{app}'.");
                }
            }
        }
        Ok(())
    }

    async fn gateway_metrics(&self, client: &mut BosunClient) -> anyhow::Result<()> {
        let resp = client.get_gateway_metrics().await?;
        let metrics_text = resp.metrics_text;

        if self.json {
            let lines: Vec<&str> = metrics_text.lines().collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "metrics": lines,
                }))?
            );
            return Ok(());
        }

        if metrics_text.is_empty() {
            println!("No Prometheus metrics available from APISIX.");
            println!("Ensure the Prometheus plugin is enabled in APISIX config.");
            return Ok(());
        }

        // Print a summary: show key metrics with human-readable names
        println!("APISIX Prometheus Metrics Summary:");
        println!("──────────────────────────────────");

        let summary_keys = [
            ("apisix_http_status", "HTTP requests by status code"),
            ("apisix_http_latency", "HTTP request latency"),
            ("apisix_bandwidth", "Bandwidth (ingress/egress)"),
            ("apisix_etcd_reachable", "etcd reachable"),
            ("apisix_node_info", "Node info"),
        ];

        for (prefix, label) in &summary_keys {
            let mut found = false;
            for line in metrics_text.lines() {
                if line.starts_with(prefix) && !line.starts_with(&format!("{}_", prefix)) {
                    if !found {
                        println!("\n  {label}:");
                        found = true;
                    }
                    // Extract metric name and value
                    if let Some(rest) = line.strip_prefix(prefix) {
                        let metric_part = rest.trim_start_matches('{');
                        if let Some(brace_end) = metric_part.find('}') {
                            let val = metric_part[brace_end + 1..].trim();
                            let labels = &metric_part[..brace_end];
                            println!("    {}{} = {val}", prefix, labels);
                        } else if let Some(space) = rest.find(' ') {
                            println!("    {}{}", prefix, rest);
                        }
                    }
                }
            }
            if !found {
                println!("\n  {label}: (no data)");
            }
        }

        // Show cache metrics if present
        let cache_prefixes = ["apisix_cache_hit", "apisix_cache_miss", "apisix_cache_size"];
        let mut has_cache = false;
        for line in metrics_text.lines() {
            for cp in &cache_prefixes {
                if line.starts_with(cp) {
                    if !has_cache {
                        println!("\n  Cache Metrics:");
                        has_cache = true;
                    }
                    println!("    {line}");
                    break;
                }
            }
        }

        println!("\n  (Run `bosun gateway metrics --json` for full Prometheus output)");
        Ok(())
    }

    // ── Security ──────────────────────────────────────────────────

    async fn run_security(
        &self,
        client: &mut BosunClient,
        sub: &SecurityCmd,
    ) -> anyhow::Result<()> {
        match sub {
            SecurityCmd::Status => self.security_status(client).await,
            SecurityCmd::Decisions => self.security_decisions(client).await,
            SecurityCmd::Scan { app, host } => {
                self.security_scan(app, host.as_deref()).await
            }
            SecurityCmd::Report { app, output, host } => {
                self.security_report(
                    app,
                    host.as_deref(),
                    output.as_deref(),
                )
                .await
            }
        }
    }

    async fn security_status(&self, client: &mut BosunClient) -> anyhow::Result<()> {
        let resp = client.get_security_status().await?;
        let status = resp.status.as_ref();

        let engine = status
            .map(|s| s.engine.as_str())
            .unwrap_or("unknown");
        let attacks = status.map(|s| s.attacks_blocked).unwrap_or(0);
        let bans = status.map(|s| s.active_bans).unwrap_or(0);

        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "engine": engine,
                    "attacks_blocked": attacks,
                    "active_bans": bans,
                }))?
            );
            return Ok(());
        }

        println!("Security Engine: {engine}");
        println!("  Attacks blocked: {attacks}");
        println!("  Active bans:     {bans}");
        Ok(())
    }

    async fn security_decisions(&self, client: &mut BosunClient) -> anyhow::Result<()> {
        let resp = client.get_security_decisions().await?;
        let decisions = resp.decisions;

        if self.json {
            let decisions_json: Vec<serde_json::Value> = decisions.iter().map(|d| {
                serde_json::json!({
                    "ip": d.ip,
                    "reason": d.reason,
                    "action": d.action,
                    "expires_unix": d.expires_unix,
                })
            }).collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&decisions_json)?
            );
            return Ok(());
        }

        if decisions.is_empty() {
            println!("No active bans/decisions.");
            return Ok(());
        }

        use tabled::{Table, Tabled};
        #[derive(Tabled)]
        struct DecisionRow {
            #[tabled(rename = "IP")]
            ip: String,
            #[tabled(rename = "REASON")]
            reason: String,
            #[tabled(rename = "ACTION")]
            action: String,
            #[tabled(rename = "EXPIRES")]
            expires: String,
        }

        let rows: Vec<DecisionRow> = decisions
            .iter()
            .map(|d| {
                let expires = if d.expires_unix == 0 {
                    "N/A".to_string()
                } else {
                    chrono_human(d.expires_unix)
                };
                DecisionRow {
                    ip: d.ip.clone(),
                    reason: d.reason.clone(),
                    action: d.action.clone(),
                    expires,
                }
            })
            .collect();

        let table = Table::new(rows);
        println!("{table}");
        Ok(())
    }

    async fn security_scan(
        &self,
        app: &str,
        host: Option<&str>,
    ) -> anyhow::Result<()> {
        let target = host.unwrap_or("localhost");

        eprintln!("Running security scan for '{app}' on {target}...");
        eprintln!();

        if self.json {
            let mut results = serde_json::Map::new();
            results.insert(
                "app".to_string(),
                serde_json::Value::String(app.to_string()),
            );
            results.insert(
                "host".to_string(),
                serde_json::Value::String(target.to_string()),
            );
            results.insert(
                "ports".to_string(),
                serde_json::to_value(self.scan_ports_json(target).await)?,
            );
            results.insert(
                "ssl".to_string(),
                serde_json::to_value(self.scan_ssl_json(target).await)?,
            );
            results.insert(
                "headers".to_string(),
                serde_json::to_value(self.scan_headers_json(target).await)?,
            );
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::Value::Object(results))?
            );
            return Ok(());
        }

        // Port scan
        println!("--- Port Scan ---");
        let ports = self.scan_ports(target).await;
        for (port, open) in &ports {
            let status = if *open { "OPEN  " } else { "closed" };
            let label = match port {
                80 => "HTTP",
                443 => "HTTPS",
                8080 => "HTTP-alt",
                3000 => "Webapp",
                5432 => "PostgreSQL",
                6379 => "Redis",
                _ => "other",
            };
            println!("  {status}  {port:>5}/tcp  ({label})");
        }

        // SSL check
        println!();
        println!("--- SSL Check ---");
        let ssl_result = self.scan_ssl(target).await;
        println!("  {ssl_result}");

        // Headers check
        println!();
        println!("--- Security Headers ---");
        let header_results = self.scan_headers(target).await;
        for (header, present) in &header_results {
            let status = if *present { "[+]" } else { "[-]" };
            println!("  {status} {header}");
        }

        Ok(())
    }

    /// Scan common ports on a host. Returns Vec of (port, is_open).
    async fn scan_ports(&self, host: &str) -> Vec<(u16, bool)> {
        let ports = [80, 443, 8080, 3000, 5432, 6379];
        let mut results = Vec::new();

        for &port in &ports {
            let addr = format!("{host}:{port}");
            let is_open = tokio::net::TcpStream::connect(&addr).await.is_ok();
            results.push((port, is_open));
        }

        results
    }

    async fn scan_ports_json(&self, host: &str) -> serde_json::Value {
        let ports = self.scan_ports(host).await;
        let open_ports: Vec<u16> = ports
            .iter()
            .filter(|(_, open)| *open)
            .map(|(p, _)| *p)
            .collect();
        serde_json::json!({
            "open": open_ports,
            "total_scanned": ports.len(),
        })
    }

    /// Check SSL certificate on host:443.
    async fn scan_ssl(&self, host: &str) -> String {
        let url = format!("https://{host}/");
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(e) => return format!("[FAIL] Failed to create HTTPS client: {e}"),
        };

        match client.get(&url).send().await {
            Ok(_resp) => "[OK] SSL/TLS active".to_string(),
            Err(e) => {
                if e.is_connect() {
                    // Try HTTP as fallback
                    let http_url = format!("http://{host}/");
                    match client.get(&http_url).send().await {
                        Ok(_) => "[WARN] No SSL/TLS on port 443 (HTTP only on port 80)".to_string(),
                        Err(_) => "[FAIL] No connection on port 443 or 80".to_string(),
                    }
                } else if e.is_timeout() {
                    "[WARN] SSL/TLS timeout - port may be filtered".to_string()
                } else {
                    format!("[FAIL] SSL/TLS check failed: {e}")
                }
            }
        }
    }

    async fn scan_ssl_json(&self, host: &str) -> serde_json::Value {
        let result = self.scan_ssl(host).await;
        let has_ssl = result.starts_with("[OK]");
        serde_json::json!({
            "has_ssl": has_ssl,
            "detail": result,
        })
    }

    /// Check for missing security headers on the target.
    async fn scan_headers(&self, host: &str) -> Vec<(String, bool)> {
        let required_headers = [
            "Content-Security-Policy",
            "Strict-Transport-Security",
            "X-Frame-Options",
            "X-Content-Type-Options",
            "Referrer-Policy",
        ];

        let url = format!("https://{host}/");
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(std::time::Duration::from_secs(5))
            .build();

        let client = match client {
            Ok(c) => c,
            Err(_) => {
                return required_headers
                    .iter()
                    .map(|h| (h.to_string(), false))
                    .collect();
            }
        };

        match client.get(&url).send().await {
            Ok(resp) => {
                let headers = resp.headers();
                required_headers
                    .iter()
                    .map(|h| {
                        (
                            h.to_string(),
                            headers.contains_key(*h),
                        )
                    })
                    .collect()
            }
            Err(_) => {
                // Try HTTP as fallback
                let url = format!("http://{host}/");
                match client.get(&url).send().await {
                    Ok(resp) => {
                        let headers = resp.headers();
                        required_headers
                            .iter()
                            .map(|h| {
                                (
                                    h.to_string(),
                                    headers.contains_key(*h),
                                )
                            })
                            .collect()
                    }
                    Err(_) => required_headers
                        .iter()
                        .map(|h| (h.to_string(), false))
                        .collect(),
                }
            }
        }
    }

    async fn scan_headers_json(&self, host: &str) -> serde_json::Value {
        let results = self.scan_headers(host).await;
        let present: Vec<&str> = results
            .iter()
            .filter(|(_, p)| *p)
            .map(|(h, _)| h.as_str())
            .collect();
        let missing: Vec<&str> = results
            .iter()
            .filter(|(_, p)| !*p)
            .map(|(h, _)| h.as_str())
            .collect();
        serde_json::json!({
            "present": present,
            "missing": missing,
        })
    }

    /// Generate a pentest HTML report.
    async fn security_report(
        &self,
        app: &str,
        host: Option<&str>,
        output: Option<&str>,
    ) -> anyhow::Result<()> {
        let target = host.unwrap_or("localhost");
        let output_path = output
            .map(|o| o.to_string())
            .unwrap_or_else(|| format!("bosun-pentest-{app}.html"));

        eprintln!("Generating pentest report for '{app}' on {target}...");

        let ports = self.scan_ports(target).await;
        let ssl = self.scan_ssl(target).await;
        let headers = self.scan_headers(target).await;

        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

        let mut ports_html = String::new();
        for (port, open) in &ports {
            let cls = if *open { "open" } else { "closed" };
            let label = match port {
                80 => "HTTP",
                443 => "HTTPS",
                8080 => "HTTP-alt",
                3000 => "Webapp",
                5432 => "PostgreSQL",
                6379 => "Redis",
                _ => "other",
            };
            ports_html.push_str(&format!(
                "<tr><td>{port}</td><td class=\"{cls}\">{}</td><td>{label}</td></tr>",
                if *open { "Open" } else { "Closed" }
            ));
        }

        let mut headers_html = String::new();
        for (header, present) in &headers {
            let cls = if *present { "present" } else { "missing" };
            headers_html.push_str(&format!(
                "<tr><td>{header}</td><td class=\"{cls}\">{}</td></tr>",
                if *present { "[+] Present" } else { "[-] Missing" }
            ));
        }

        let html = format!(
            r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Bosun Pentest Report - {app}</title>
<style>
body {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; max-width: 800px; margin: 0 auto; padding: 20px; background: #0d1117; color: #c9d1d9; }}
h1 {{ color: #58a6ff; border-bottom: 2px solid #30363d; padding-bottom: 10px; }}
h2 {{ color: #f0883e; margin-top: 30px; }}
table {{ width: 100%; border-collapse: collapse; margin: 10px 0; }}
th, td {{ padding: 8px 12px; text-align: left; border-bottom: 1px solid #30363d; }}
th {{ background: #161b22; color: #8b949e; }}
.open, .present {{ color: #3fb950; font-weight: bold; }}
.closed, .missing {{ color: #f85149; }}
.ssl-ok {{ color: #3fb950; }}
.ssl-warn {{ color: #d2991d; }}
.ssl-err {{ color: #f85149; }}
footer {{ margin-top: 40px; font-size: 0.85em; color: #8b949e; border-top: 1px solid #30363d; padding-top: 10px; }}
</style>
</head>
<body>
<h1>Bosun Pentest Report</h1>
<p><strong>Target:</strong> {app} ({target})</p>
<p><strong>Generated:</strong> {now}</p>

<h2>Port Scan</h2>
<table>
    <tr><th>Port</th><th>Status</th><th>Service</th></tr>
    {ports_html}
</table>

<h2>SSL/TLS Check</h2>
<p class="{ssl_class}">{ssl}</p>

<h2>Security Headers</h2>
<table>
    <tr><th>Header</th><th>Status</th></tr>
    {headers_html}
</table>

<footer>
    Generated by Bosun CLI - <code>bosun security report {app}</code>
</footer>
</body>
</html>"#,
            app = app,
            target = target,
            now = now,
            ports_html = ports_html,
            ssl = ssl,
            ssl_class = if ssl.starts_with("[OK]") {
                "ssl-ok"
            } else if ssl.starts_with("[WARN]") {
                "ssl-warn"
            } else {
                "ssl-err"
            },
            headers_html = headers_html,
        );

        std::fs::write(&output_path, &html)?;
        eprintln!("Report saved to: {output_path}");

        Ok(())
    }

    // ── Cluster ────────────────────────────────────────────────────

    async fn run_cluster(&self, sub: &ClusterCmd) -> anyhow::Result<()> {
        match sub {
            ClusterCmd::Init { advertise_addr } => {
                self.cluster_init(advertise_addr.as_deref()).await
            }
            ClusterCmd::Join { token, addr } => {
                self.cluster_join(token, addr).await
            }
            ClusterCmd::Nodes => self.cluster_nodes().await,
            ClusterCmd::Leave { force } => self.cluster_leave(*force).await,
        }
    }

    async fn cluster_init(&self, advertise_addr: Option<&str>) -> anyhow::Result<()> {
        if !self.json {
            eprintln!("🐝 Initializing Docker Swarm...");
        }

        let mut cmd = std::process::Command::new("docker");
        cmd.arg("swarm").arg("init");

        if let Some(addr) = advertise_addr {
            cmd.arg("--advertise-addr").arg(addr);
        }

        let output = cmd.output().context("Failed to run 'docker swarm init'. Is Docker installed?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Docker swarm init failed:\n{}", stderr.trim());
        }

        // Get the worker join token
        let token_output = std::process::Command::new("docker")
            .arg("swarm")
            .arg("join-token")
            .arg("worker")
            .arg("-q")
            .output()
            .context("Failed to get Swarm join token")?;

        let worker_token = String::from_utf8_lossy(&token_output.stdout).trim().to_string();

        if self.json {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "action": "swarm_init",
                "status": "ok",
                "worker_join_token": worker_token,
            }))?);
        } else {
            println!("\n✔ Docker Swarm initialized successfully!\n");
            println!("  To join additional worker nodes, run this on each worker:");
            println!("    docker swarm join --token {} \\", worker_token);
            println!("      <MANAGER_IP>:2377\n");
            println!("  For manager nodes (use with caution):");
            println!("    bosun cluster init  (on manager)");
            println!("    docker swarm join-token manager  (to get manager token)");
        }

        Ok(())
    }

    async fn cluster_join(&self, token: &str, addr: &str) -> anyhow::Result<()> {
        if !self.json {
            eprintln!("🐝 Joining Docker Swarm at {}...", addr);
        }

        let output = std::process::Command::new("docker")
            .arg("swarm")
            .arg("join")
            .arg("--token")
            .arg(token)
            .arg(addr)
            .output()
            .context("Failed to run 'docker swarm join'. Is Docker installed?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Docker swarm join failed:\n{}", stderr.trim());
        }

        if self.json {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "action": "swarm_join",
                "status": "ok",
                "manager": addr,
            }))?);
        } else {
            println!("✔ Successfully joined Docker Swarm at {}", addr);
        }

        Ok(())
    }

    async fn cluster_nodes(&self) -> anyhow::Result<()> {
        let output = std::process::Command::new("docker")
            .arg("node")
            .arg("ls")
            .arg("--format")
            .arg("{{.ID}}\t{{.Hostname}}\t{{.Status}}\t{{.Availability}}\t{{.ManagerStatus}}")
            .output()
            .context("Failed to run 'docker node ls'. Is Docker in Swarm mode?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Docker node ls failed:\n{}", stderr.trim());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);

        if self.json {
            let nodes: Vec<serde_json::Value> = stdout
                .lines()
                .map(|line| {
                    let parts: Vec<&str> = line.split('\t').collect();
                    serde_json::json!({
                        "id": parts.get(0).map(|s| s.trim_matches('*').trim()).unwrap_or(""),
                        "hostname": parts.get(1).map(|s| s.trim()).unwrap_or(""),
                        "status": parts.get(2).map(|s| s.trim()).unwrap_or(""),
                        "availability": parts.get(3).map(|s| s.trim()).unwrap_or(""),
                        "role": if parts.get(4).map(|s| s.contains("Leader") || s.contains("Reachable")).unwrap_or(false) {
                            "manager"
                        } else {
                            "worker"
                        },
                    })
                })
                .collect();

            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "nodes": nodes,
            }))?);
        } else {
            use tabled::{Table, Tabled};
            #[derive(Tabled)]
            struct NodeRow {
                #[tabled(rename = "ID")]
                id: String,
                #[tabled(rename = "HOSTNAME")]
                hostname: String,
                #[tabled(rename = "STATUS")]
                status: String,
                #[tabled(rename = "AVAILABILITY")]
                availability: String,
                #[tabled(rename = "ROLE")]
                role: String,
            }

            let rows: Vec<NodeRow> = stdout
                .lines()
                .map(|line| {
                    let parts: Vec<&str> = line.split('\t').collect();
                    let role = if parts.get(4).map(|s| s.contains("Leader") || s.contains("Reachable")).unwrap_or(false) {
                        "manager"
                    } else {
                        "worker"
                    };
                    NodeRow {
                        id: parts.get(0).map(|s| s.trim_matches('*').trim().to_string()).unwrap_or_default(),
                        hostname: parts.get(1).map(|s| s.trim().to_string()).unwrap_or_default(),
                        status: parts.get(2).map(|s| s.trim().to_string()).unwrap_or_default(),
                        availability: parts.get(3).map(|s| s.trim().to_string()).unwrap_or_default(),
                        role: role.to_string(),
                    }
                })
                .collect();

            if rows.is_empty() {
                println!("No nodes found. Is Docker in Swarm mode?");
            } else {
                let table = Table::new(rows);
                println!("{table}");
            }
        }

        Ok(())
    }

    async fn cluster_leave(&self, force: bool) -> anyhow::Result<()> {
        if !self.json {
            if force {
                eprintln!("🐝 Force-leaving Docker Swarm...");
            } else {
                eprintln!("🐝 Leaving Docker Swarm...");
            }
        }

        let mut cmd = std::process::Command::new("docker");
        cmd.arg("swarm").arg("leave");

        if force {
            cmd.arg("--force");
        }

        let output = cmd.output().context("Failed to run 'docker swarm leave'. Is Docker installed?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("use --force") && !force {
                anyhow::bail!(
                    "This is the last manager. Use --force to leave:\n  bosun cluster leave --force"
                );
            }
            anyhow::bail!("Docker swarm leave failed:\n{}", stderr.trim());
        }

        if self.json {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "action": "swarm_leave",
                "status": "ok",
            }))?);
        } else {
            println!("✔ Left Docker Swarm");
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
        /// Shell command to run on the host before build (can be specified multiple times)
        #[arg(long = "pre")]
        pre_hooks: Vec<String>,
        /// Shell command to run on the host after deploy + health check (can be specified multiple times)
        #[arg(long = "post")]
        post_hooks: Vec<String>,
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
    /// Manage APISIX API Gateway (routes, plugins, cache, metrics)
    Gateway {
        #[command(subcommand)]
        sub: GatewayCmd,
    },
    /// Security scanning and pentesting tools
    Security {
        #[command(subcommand)]
        sub: SecurityCmd,
    },
    /// Launch interactive real-time dashboard TUI
    Dashboard,
    /// Manage Docker Swarm cluster (init, join, nodes, leave)
    Cluster {
        #[command(subcommand)]
        sub: ClusterCmd,
    },
}

#[derive(Subcommand)]
pub enum ClusterCmd {
    /// Initialize Docker Swarm on this node
    Init {
        /// Advertise address for other nodes to connect
        #[arg(long)]
        advertise_addr: Option<String>,
    },
    /// Join an existing Docker Swarm cluster
    Join {
        /// Swarm join token (worker or manager)
        token: String,
        /// Address of the Swarm manager (e.g., 192.168.1.10:2377)
        addr: String,
    },
    /// List all nodes in the Docker Swarm
    Nodes,
    /// Leave the Docker Swarm (use --force for managers)
    Leave {
        /// Force leave even if this is the last manager
        #[arg(long)]
        force: bool,
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

#[derive(Subcommand)]
pub enum GatewayCmd {
    /// Show APISIX gateway status (version, uptime)
    Status,
    /// List all APISIX routes managed by bosun
    Routes,
    /// Enable a plugin on a route
    Plugin {
        /// App/route name
        app: String,
        /// Plugin name (e.g., rate-limit, jwt-auth, proxy-cache, cors)
        plugin: String,
        /// JSON config for the plugin (e.g., '{"count": 100, "time_window": 60}')
        #[arg(long)]
        config: Option<String>,
    },
    /// Manage proxy-cache (enable, disable, stats, purge)
    Cache {
        #[command(subcommand)]
        sub: CacheCmd,
    },
    /// Show Prometheus metrics from APISIX
    Metrics,
}

#[derive(Subcommand)]
pub enum CacheCmd {
    /// Enable proxy-cache for an app
    Enable {
        /// App/route name
        app: String,
        /// Cache TTL in seconds
        #[arg(long, default_value = "300")]
        ttl: u64,
    },
    /// Disable proxy-cache for an app
    Disable {
        /// App/route name
        app: String,
    },
    /// Show cache statistics for an app
    Stats {
        /// App/route name
        app: String,
    },
    /// Purge (clear) the cache for an app
    Purge {
        /// App/route name
        app: String,
    },
}

#[derive(Subcommand)]
pub enum SecurityCmd {
    /// Show IDS/IPS security engine status
    Status,
    /// List active decisions (banned IPs)
    Decisions,
    /// Run a basic security scan (ports, SSL, headers)
    Scan {
        /// App or domain to scan
        app: String,
        /// Target host (defaults to localhost)
        #[arg(long)]
        host: Option<String>,
    },
    /// Generate a pentest HTML report
    Report {
        /// App or domain to generate report for
        app: String,
        /// Output file path (defaults to bosun-pentest-{app}.html)
        #[arg(long)]
        output: Option<String>,
        /// Target host (defaults to localhost)
        #[arg(long)]
        host: Option<String>,
    },
}

// ── Helpers ───────────────────────────────────────────────────────

/// Display helper for credentials path
fn creds_path_display() -> String {
    BosunClient::credentials_path().display().to_string()
}

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
