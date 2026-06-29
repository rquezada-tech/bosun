//! Zero-config security module.
//!
//! Auto-detects and configures CrowdSec or Fail2Ban to monitor app logs.
//! Provides status and decision querying for the gRPC API.

use std::path::Path;
use std::process::Command;

/// Supported IDS/IPS engines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecurityEngine {
    /// CrowdSec is installed and available.
    CrowdSec,
    /// Fail2Ban is installed (fallback when CrowdSec is absent).
    Fail2Ban,
    /// Neither CrowdSec nor Fail2Ban is detected.
    None,
}

impl SecurityEngine {
    pub fn as_str(&self) -> &'static str {
        match self {
            SecurityEngine::CrowdSec => "crowdsec",
            SecurityEngine::Fail2Ban => "fail2ban",
            SecurityEngine::None => "none",
        }
    }
}

impl Default for SecurityEngine {
    fn default() -> Self {
        SecurityEngine::None
    }
}

/// Statistics from the security engine.
#[derive(Debug, Clone)]
pub struct SecurityStats {
    pub engine: SecurityEngine,
    pub attacks_blocked: u64,
    pub active_bans: u64,
}

impl Default for SecurityStats {
    fn default() -> Self {
        Self {
            engine: SecurityEngine::None,
            attacks_blocked: 0,
            active_bans: 0,
        }
    }
}

/// A single decision (ban) entry.
#[derive(Debug, Clone)]
pub struct SecurityDecision {
    pub ip: String,
    pub reason: String,
    pub action: String,
    pub expires_unix: u64,
}

/// Service for detecting and configuring IDS/IPS engines.
#[derive(Debug, Clone)]
pub struct SecurityService {
    engine: SecurityEngine,
}

impl SecurityService {
    /// Detect which security engine is available on the system.
    pub fn detect() -> Self {
        let engine = if binary_exists("crowdsec") {
            tracing::info!("CrowdSec detected — using CrowdSec for IDS/IPS");
            SecurityEngine::CrowdSec
        } else if binary_exists("fail2ban-client") {
            tracing::info!("Fail2Ban detected — using Fail2Ban as fallback IDS/IPS");
            SecurityEngine::Fail2Ban
        } else {
            tracing::warn!("No IDS/IPS detected — CrowdSec and Fail2Ban not found. Install one for security monitoring.");
            tracing::warn!("Install CrowdSec: curl -s https://packagecloud.io/install/repositories/crowdsec/crowdsec/script.deb.sh | sudo bash && sudo apt install crowdsec");
            tracing::warn!("Or install Fail2Ban: sudo apt install fail2ban");
            SecurityEngine::None
        };

        Self { engine }
    }

    /// Return the detected engine type.
    pub fn engine(&self) -> SecurityEngine {
        self.engine.clone()
    }

    /// Configure the security engine to monitor logs for a deployed app.
    ///
    /// - If CrowdSec: creates /etc/crowdsec/acquis.d/bosun-{app}.yaml
    /// - If Fail2Ban: creates /etc/fail2ban/jail.d/bosun-{app}.conf
    /// - If None: logs a warning
    pub fn configure_app(&self, app_name: &str, domain: Option<&str>) {
        match &self.engine {
            SecurityEngine::CrowdSec => {
                if let Err(e) = self.configure_crowdsec(app_name, domain) {
                    tracing::error!(
                        "Failed to configure CrowdSec for app '{}': {}",
                        app_name,
                        e
                    );
                }
            }
            SecurityEngine::Fail2Ban => {
                if let Err(e) = self.configure_fail2ban(app_name, domain) {
                    tracing::error!(
                        "Failed to configure Fail2Ban for app '{}': {}",
                        app_name,
                        e
                    );
                }
            }
            SecurityEngine::None => {
                tracing::warn!(
                    "No IDS/IPS configured for app '{}' — install CrowdSec or Fail2Ban for attack protection",
                    app_name
                );
            }
        }
    }

    /// Configure CrowdSec to tail Caddy logs for the given domain.
    fn configure_crowdsec(&self, app_name: &str, domain: Option<&str>) -> Result<(), String> {
        let acquis_dir = Path::new("/etc/crowdsec/acquis.d");
        if !acquis_dir.exists() {
            return Err(format!(
                "CrowdSec acquis directory not found at {}. Is CrowdSec installed?",
                acquis_dir.display()
            ));
        }

        let config_path = acquis_dir.join(format!("bosun-{}.yaml", app_name));

        let domain_filter = domain.unwrap_or(app_name);

        let yaml_content = format!(
            r#"# Bosun-managed CrowdSec acquis config for {app}
# Auto-generated — do not edit manually
filenames:
  - /var/log/caddy/{domain}.log
labels:
  type: caddy
  app: {app}
  bosun_managed: "true"
---
# Also monitor Caddy access log
filenames:
  - /var/log/caddy/access.log
labels:
  type: caddy
  app: {app}
  bosun_managed: "true"
"#,
            app = app_name,
            domain = domain_filter,
        );

        std::fs::write(&config_path, &yaml_content).map_err(|e| {
            format!(
                "Failed to write CrowdSec acquis config at {}: {}",
                config_path.display(),
                e
            )
        })?;

        tracing::info!(
            "CrowdSec acquis config created for app '{}' at {}",
            app_name,
            config_path.display()
        );

        // Reload CrowdSec to pick up new log sources
        if self.run_crowdsec_cmd(&["reload"]).is_none() {
            tracing::warn!("Failed to reload CrowdSec after config update");
        }

        Ok(())
    }

    /// Configure Fail2Ban to monitor HTTP auth failures for the given domain.
    fn configure_fail2ban(&self, app_name: &str, domain: Option<&str>) -> Result<(), String> {
        let jail_dir = Path::new("/etc/fail2ban/jail.d");
        if !jail_dir.exists() {
            return Err(format!(
                "Fail2Ban jail directory not found at {}. Is Fail2Ban installed?",
                jail_dir.display()
            ));
        }

        let config_path = jail_dir.join(format!("bosun-{}.conf", app_name));

        let domain_filter = domain.unwrap_or(app_name);

        let conf_content = format!(
            r#"# Bosun-managed Fail2Ban jail for {app}
# Auto-generated — do not edit manually

[bosun-{app}]
enabled = true
filter = bosun-{app}
logpath = /var/log/caddy/{domain}.log
         /var/log/caddy/access.log
maxretry = 5
findtime = 600
bantime = 3600
action = iptables-multiport[name=bosun-{app}, port="80,443", protocol=tcp]
"#,
            app = app_name,
            domain = domain_filter,
        );

        std::fs::write(&config_path, &conf_content).map_err(|e| {
            format!(
                "Failed to write Fail2Ban jail config at {}: {}",
                config_path.display(),
                e
            )
        })?;

        tracing::info!(
            "Fail2Ban jail config created for app '{}' at {}",
            app_name,
            config_path.display()
        );

        // Create a simple filter for this app
        let filter_dir = Path::new("/etc/fail2ban/filter.d");
        let filter_path = filter_dir.join(format!("bosun-{}.conf", app_name));
        let filter_content = format!(
            r#"# Bosun-managed Fail2Ban filter for {app}
[Definition]
failregex = ^<HOST> -.*"(GET|POST|PUT|DELETE|PATCH|HEAD|OPTIONS).*" (401|403|429) .*$
ignoreregex =
"#,
            app = app_name,
        );

        if let Err(e) = std::fs::write(&filter_path, &filter_content) {
            tracing::warn!(
                "Failed to write Fail2Ban filter at {}: {}",
                filter_path.display(),
                e
            );
        }

        // Reload Fail2Ban
        if self.run_fail2ban_cmd(&["reload"]).is_none() {
            tracing::warn!("Failed to reload Fail2Ban after config update");
        }

        Ok(())
    }

    /// Get current security statistics.
    pub fn status(&self) -> SecurityStats {
        match &self.engine {
            SecurityEngine::CrowdSec => self.crowdsec_status(),
            SecurityEngine::Fail2Ban => self.fail2ban_status(),
            SecurityEngine::None => SecurityStats {
                engine: SecurityEngine::None,
                attacks_blocked: 0,
                active_bans: 0,
            },
        }
    }

    /// Get list of active decisions (banned IPs).
    pub fn decisions(&self) -> Vec<SecurityDecision> {
        match &self.engine {
            SecurityEngine::CrowdSec => self.crowdsec_decisions(),
            SecurityEngine::Fail2Ban => self.fail2ban_decisions(),
            SecurityEngine::None => Vec::new(),
        }
    }

    // ── CrowdSec helpers ──────────────────────────────────────────

    fn crowdsec_status(&self) -> SecurityStats {
        let attacks = self.parse_crowdsec_metrics("cs_bucket_created_total")
            .unwrap_or(0);
        let bans = self.parse_crowdsec_decisions_count().unwrap_or(0);

        SecurityStats {
            engine: SecurityEngine::CrowdSec,
            attacks_blocked: attacks,
            active_bans: bans,
        }
    }

    fn crowdsec_decisions(&self) -> Vec<SecurityDecision> {
        let output = match self.run_crowdsec_cmd(&["decisions", "list", "-o", "json"]) {
            Some(o) => o,
            None => {
                tracing::warn!("Failed to query CrowdSec decisions");
                return Vec::new();
            }
        };

        parse_crowdsec_decisions_json(&output)
    }

    fn parse_crowdsec_metrics(&self, _metric_name: &str) -> Option<u64> {
        // Try `cscli metrics` first
        let output = self.run_crowdsec_cmd(&["metrics"])?;
        // Simple parsing: count lines that indicate blocks/attacks
        Some(output.lines().count() as u64)
    }

    fn parse_crowdsec_decisions_count(&self) -> Option<u64> {
        let output = self.run_crowdsec_cmd(&["decisions", "list", "-o", "raw"])?;
        // Count non-empty, non-header lines
        let count = output.lines().filter(|l| !l.is_empty() && !l.starts_with("id")).count();
        Some(count as u64)
    }

    fn run_crowdsec_cmd(&self, args: &[&str]) -> Option<String> {
        match Command::new("cscli").args(args).output() {
            Ok(output) if output.status.success() => {
                String::from_utf8(output.stdout).ok()
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::debug!("cscli {} failed: {}", args.join(" "), stderr);
                None
            }
            Err(e) => {
                tracing::debug!("cscli not available: {}", e);
                None
            }
        }
    }

    // ── Fail2Ban helpers ──────────────────────────────────────────

    fn fail2ban_status(&self) -> SecurityStats {
        let output = match self.run_fail2ban_cmd(&["status"]) {
            Some(o) => o,
            None => {
                return SecurityStats {
                    engine: SecurityEngine::Fail2Ban,
                    attacks_blocked: 0,
                    active_bans: 0,
                };
            }
        };

        // Count total banned IPs across all jails
        let mut total_banned = 0u64;
        for line in output.lines() {
            if line.contains("Currently banned:") {
                if let Some(num_str) = line.split(':').nth(1) {
                    if let Ok(n) = num_str.trim().parse::<u64>() {
                        total_banned += n;
                    }
                }
            }
            if line.contains("Total banned:") {
                if let Some(num_str) = line.split(':').nth(1) {
                    if let Ok(n) = num_str.trim().parse::<u64>() {
                        total_banned += n;
                    }
                }
            }
        }

        SecurityStats {
            engine: SecurityEngine::Fail2Ban,
            attacks_blocked: total_banned,
            active_bans: total_banned,
        }
    }

    fn fail2ban_decisions(&self) -> Vec<SecurityDecision> {
        let output = match self.run_fail2ban_cmd(&["status"]) {
            Some(o) => o,
            None => return Vec::new(),
        };

        let mut decisions = Vec::new();

        // Parse jail names from status output
        let jail_names: Vec<String> = output
            .lines()
            .filter(|l| l.trim().starts_with("|- "))
            .filter_map(|l| {
                let name = l.trim().trim_start_matches("|- ").trim();
                // Skip "Number of jail:" lines
                if name.contains(':') {
                    None
                } else {
                    Some(name.to_string())
                }
            })
            .collect();

        // For each jail, get banned IPs
        for jail in &jail_names {
            if let Some(jail_output) = self.run_fail2ban_cmd(&["status", jail]) {
                for line in jail_output.lines() {
                    if line.contains("Banned IP list:") || line.contains("Banned IP addresses:") {
                        if let Some(ips_str) = line.split(':').nth(1) {
                            for ip in ips_str.trim().split_whitespace() {
                                let ip = ip.trim().trim_end_matches(',');
                                if !ip.is_empty() && ip != "Banned" && ip != "IP" && ip != "list:" && ip != "addresses:" {
                                    decisions.push(SecurityDecision {
                                        ip: ip.to_string(),
                                        reason: format!("fail2ban jail: {}", jail),
                                        action: "ban".to_string(),
                                        expires_unix: 0, // Fail2Ban doesn't expose per-IP expiry easily
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        decisions
    }

    fn run_fail2ban_cmd(&self, args: &[&str]) -> Option<String> {
        match Command::new("fail2ban-client").args(args).output() {
            Ok(output) if output.status.success() => {
                String::from_utf8(output.stdout).ok()
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::debug!("fail2ban-client {} failed: {}", args.join(" "), stderr);
                None
            }
            Err(e) => {
                tracing::debug!("fail2ban-client not available: {}", e);
                None
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────

/// Check if a binary exists in PATH.
fn binary_exists(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Parse CrowdSec decisions JSON output.
fn parse_crowdsec_decisions_json(json: &str) -> Vec<SecurityDecision> {
    let mut decisions = Vec::new();

    // Try to parse as JSON first
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(json) {
        if let Some(arr) = value.as_array() {
            for item in arr {
                let ip = item.get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let reason = item.get("scenario")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let action = item.get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("ban")
                    .to_string();
                let expires = item.get("duration")
                    .and_then(|v| v.as_str())
                    .and_then(|s| parse_duration_to_unix(s))
                    .unwrap_or(0);

                decisions.push(SecurityDecision {
                    ip,
                    reason,
                    action,
                    expires_unix: expires,
                });
            }
        }
    } else {
        // Fallback: try line-by-line CSV parsing
        for line in json.lines().skip(1) {
            let parts: Vec<&str> = line.split(',').collect();
            if parts.len() >= 4 {
                decisions.push(SecurityDecision {
                    ip: parts.get(2).unwrap_or(&"unknown").to_string(),
                    reason: parts.get(3).unwrap_or(&"unknown").to_string(),
                    action: parts.get(1).unwrap_or(&"ban").to_string(),
                    expires_unix: 0,
                });
            }
        }
    }

    decisions
}

/// Parse a duration string (e.g., "4h", "24h") into a Unix timestamp.
fn parse_duration_to_unix(duration: &str) -> Option<u64> {
    let duration = duration.trim();
    if duration.is_empty() {
        return None;
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();

    let (num_str, unit) = duration.split_at(duration.len() - 1);
    let num: u64 = num_str.parse().ok()?;

    let seconds = match unit {
        "s" => num,
        "m" => num * 60,
        "h" => num * 3600,
        "d" => num * 86400,
        _ => return None,
    };

    Some(now + seconds)
}
