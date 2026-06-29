//! gRPC client wrapper for the Bosun daemon.
//!
//! Provides a typed async client that handles connection, TLS,
//! credentials, and timeout logic.

use anyhow::Context;
use crate::proto::bosun::v1::bosun_client::BosunClient as GrpcBosunClient;
use crate::proto::bosun::v1::*;
use std::path::PathBuf;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint, Identity};
use std::time::Duration;

/// Credentials stored in ~/.bosun/credentials
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Credentials {
    pub token: String,
    pub username: String,
    pub role: String,
}

/// Wrapper around the gRPC BosunClient with convenient methods.
pub struct BosunClient {
    inner: GrpcBosunClient<Channel>,
    /// Auth token loaded from credentials file (if logged in)
    token: Option<String>,
}

impl BosunClient {
    /// Connect to the bosun daemon at `addr`, loading credentials from
    /// `~/.bosun/credentials` if available.
    ///
    /// `addr` should include the scheme, e.g. `https://localhost:9090` or
    /// `http://localhost:9090`. If the scheme is `https`, TLS is enabled.
    ///
    /// Optional `cert_path` and `key_path` enable mTLS.
    pub async fn connect(
        addr: &str,
        cert_path: Option<&str>,
        key_path: Option<&str>,
    ) -> anyhow::Result<Self> {
        let endpoint = Endpoint::from_shared(addr.to_string())
            .with_context(|| format!("Invalid daemon address: {addr}"))?
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(5));

        let channel = if addr.starts_with("https://") {
            let mut tls = ClientTlsConfig::new();

            if let (Some(cert_file), Some(key_file)) = (cert_path, key_path) {
                let cert_pem = std::fs::read_to_string(cert_file)
                    .with_context(|| format!("Failed to read TLS certificate: {cert_file}"))?;
                let key_pem = std::fs::read_to_string(key_file)
                    .with_context(|| format!("Failed to read TLS key: {key_file}"))?;
                let identity = Identity::from_pem(cert_pem, key_pem);
                tls = tls.identity(identity);
            }

            endpoint
                .tls_config(tls)
                .with_context(|| "Failed to configure TLS")?
                .connect()
                .await
                .with_context(|| {
                    format!(
                        "Failed to connect to daemon at {addr}. Is bosun-daemon running? \
                         Try: bosun-daemon --listen 0.0.0.0:9090"
                    )
                })?
        } else {
            endpoint
                .connect()
                .await
                .with_context(|| {
                    format!(
                        "Failed to connect to daemon at {addr}. Is bosun-daemon running? \
                         Try: bosun-daemon --listen 0.0.0.0:9090"
                    )
                })?
        };

        // Load credentials from ~/.bosun/credentials
        let token = Self::load_credentials()?.map(|c| c.token);

        Ok(Self {
            inner: GrpcBosunClient::new(channel),
            token,
        })
    }

    /// Path to the credentials file: ~/.bosun/credentials
    pub fn credentials_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(home).join(".bosun").join("credentials")
    }

    /// Load credentials from ~/.bosun/credentials
    pub fn load_credentials() -> anyhow::Result<Option<Credentials>> {
        let path = Self::credentials_path();
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read credentials from {}", path.display()))?;
        let creds: Credentials = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse credentials from {}", path.display()))?;
        Ok(Some(creds))
    }

    /// Save credentials to ~/.bosun/credentials from a login response.
    pub fn save_credentials(&self, token: &str, username: &str, role: &str) -> anyhow::Result<()> {
        let path = Self::credentials_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let creds = Credentials {
            token: token.to_string(),
            username: username.to_string(),
            role: role.to_string(),
        };
        let json = serde_json::to_string_pretty(&creds)?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    /// Add the auth token as a gRPC metadata header.
    fn auth_request<T>(&self, mut request: tonic::Request<T>) -> tonic::Request<T> {
        if let Some(ref token) = self.token {
            let bearer = format!("Bearer {}", token);
            if let Ok(metadata_value) = bearer.parse() {
                request.metadata_mut().insert(
                    "authorization",
                    metadata_value,
                );
            }
        }
        request
    }

    // ── Auth ───────────────────────────────────────────────────────

    /// Login to the bosun daemon and get a JWT token.
    pub async fn login(
        &mut self,
        username: &str,
        password: &str,
    ) -> anyhow::Result<LoginResponse> {
        let request = tonic::Request::new(LoginRequest {
            username: username.to_string(),
            password: password.to_string(),
        });
        // No auth header for login request

        let response = self
            .inner
            .login(request)
            .await
            .context("gRPC Login failed")?
            .into_inner();

        // Store the token for subsequent requests
        self.token = Some(response.token.clone());

        Ok(response)
    }

    // ── App management ────────────────────────────────────────────

    /// List all deployed applications.
    pub async fn list_apps(&mut self) -> anyhow::Result<Vec<App>> {
        let request = self.auth_request(tonic::Request::new(ListAppsRequest {}));
        let response = self
            .inner
            .list_apps(request)
            .await
            .context("gRPC ListApps failed")?
            .into_inner();
        Ok(response.apps)
    }

    /// Stream logs for an app.
    pub async fn get_logs(
        &mut self,
        app_name: &str,
        follow: bool,
        tail_lines: u32,
    ) -> anyhow::Result<tonic::Streaming<LogEntry>> {
        let request = self.auth_request(tonic::Request::new(GetAppLogsRequest {
            app_name: app_name.to_string(),
            follow,
            tail_lines,
        }));
        let response = self
            .inner
            .get_app_logs(request)
            .await
            .context(format!("gRPC GetAppLogs failed for '{app_name}'"))?
            .into_inner();
        Ok(response)
    }

    /// Restart an application.
    pub async fn restart_app(&mut self, app_name: &str) -> anyhow::Result<()> {
        let request = self.auth_request(tonic::Request::new(RestartAppRequest {
            app_name: app_name.to_string(),
        }));
        self.inner
            .restart_app(request)
            .await
            .context(format!("gRPC RestartApp failed for '{app_name}'"))?;
        Ok(())
    }

    /// Scale an application to the given number of instances.
    pub async fn scale_app(&mut self, app_name: &str, instances: u32) -> anyhow::Result<()> {
        let request = self.auth_request(tonic::Request::new(ScaleAppRequest {
            app_name: app_name.to_string(),
            instances,
        }));
        self.inner
            .scale_app(request)
            .await
            .context(format!(
                "gRPC ScaleApp failed for '{app_name}' to {instances} instances"
            ))?;
        Ok(())
    }

    // ── Deployment ────────────────────────────────────────────────

    /// Deploy an application from a context path on the server.
    pub async fn deploy(
        &mut self,
        context_path: &str,
        domain: Option<&str>,
        enable_ssl: bool,
        env: std::collections::HashMap<String, String>,
        port: Option<u32>,
        strategy: DeployStrategy,
        pre_hooks: &[String],
        post_hooks: &[String],
    ) -> anyhow::Result<DeployResponse> {
        let request = self.auth_request(tonic::Request::new(DeployRequest {
            context_path: context_path.to_string(),
            domain: domain.map(|s| s.to_string()),
            enable_ssl,
            env,
            port,
            strategy: strategy.into(),
            pre_hooks: pre_hooks.to_vec(),
            post_hooks: post_hooks.to_vec(),
        }));
        let response = self
            .inner
            .deploy(request)
            .await
            .context(format!("gRPC Deploy failed for '{context_path}'"))?
            .into_inner();
        Ok(response)
    }

    // ── Metrics ───────────────────────────────────────────────────

    /// Get a one-shot metrics snapshot.
    pub async fn get_metrics(
        &mut self,
        app_name: Option<&str>,
    ) -> anyhow::Result<Vec<AppMetric>> {
        let request = self.auth_request(tonic::Request::new(GetMetricsRequest {
            app_name: app_name.map(|s| s.to_string()),
            live: false,
        }));
        let response = self
            .inner
            .get_metrics(request)
            .await
            .context("gRPC GetMetrics failed")?
            .into_inner();
        Ok(response.metrics)
    }

    /// Stream metrics live, returning an async stream of AppMetric entries.
    pub async fn stream_metrics(
        &mut self,
        app_name: Option<&str>,
    ) -> anyhow::Result<tonic::Streaming<AppMetric>> {
        let request = self.auth_request(tonic::Request::new(GetMetricsRequest {
            app_name: app_name.map(|s| s.to_string()),
            live: true,
        }));
        let response = self
            .inner
            .stream_metrics(request)
            .await
            .context("gRPC StreamMetrics failed")?
            .into_inner();
        Ok(response)
    }

    // ── Environment ───────────────────────────────────────────────

    /// Get environment variables for an app.
    pub async fn get_env(
        &mut self,
        app_name: &str,
    ) -> anyhow::Result<std::collections::HashMap<String, String>> {
        let request = self.auth_request(tonic::Request::new(GetEnvRequest {
            app_name: app_name.to_string(),
        }));
        let response = self
            .inner
            .get_env(request)
            .await
            .context(format!("gRPC GetEnv failed for '{app_name}'"))?
            .into_inner();
        Ok(response.env)
    }

    /// Set an environment variable for an app.
    pub async fn set_env(&mut self, app_name: &str, key: &str, value: &str) -> anyhow::Result<()> {
        let request = self.auth_request(tonic::Request::new(SetEnvRequest {
            app_name: app_name.to_string(),
            key: key.to_string(),
            value: value.to_string(),
        }));
        self.inner
            .set_env(request)
            .await
            .context(format!("gRPC SetEnv failed for '{app_name}': {key}={value}"))?;
        Ok(())
    }

    /// Remove an environment variable from an app.
    pub async fn unset_env(&mut self, app_name: &str, key: &str) -> anyhow::Result<()> {
        let request = self.auth_request(tonic::Request::new(UnsetEnvRequest {
            app_name: app_name.to_string(),
            key: key.to_string(),
        }));
        self.inner
            .unset_env(request)
            .await
            .context(format!("gRPC UnsetEnv failed for '{app_name}': {key}"))?;
        Ok(())
    }

    // ── Templates ──────────────────────────────────────────────────

    /// List available one-click app templates.
    pub async fn list_templates(&mut self) -> anyhow::Result<Vec<TemplateInfo>> {
        let request = self.auth_request(tonic::Request::new(ListTemplatesRequest {}));
        let response = self
            .inner
            .list_templates(request)
            .await
            .context("gRPC ListTemplates failed")?
            .into_inner();
        Ok(response.templates)
    }

    /// Rollback an app (blue-green switch or no-op for other strategies).
    pub async fn rollback_app(&mut self, app_name: &str) -> anyhow::Result<RollbackAppResponse> {
        let request = self.auth_request(tonic::Request::new(RollbackAppRequest {
            app_name: app_name.to_string(),
        }));
        let response = self
            .inner
            .rollback_app(request)
            .await
            .context(format!("gRPC RollbackApp failed for '{app_name}'"))?
            .into_inner();
        Ok(response)
    }

    // ── Backup & Restore ────────────────────────────────────────────

    /// Create a backup of an app's volumes and configuration.
    pub async fn create_backup(&mut self, app_name: &str) -> anyhow::Result<CreateBackupResponse> {
        let request = self.auth_request(tonic::Request::new(CreateBackupRequest {
            app_name: app_name.to_string(),
        }));
        let response = self
            .inner
            .create_backup(request)
            .await
            .context(format!("gRPC CreateBackup failed for '{app_name}'"))?
            .into_inner();
        Ok(response)
    }

    /// List all backups, optionally filtered by app name.
    pub async fn list_backups(
        &mut self,
        app_name: Option<&str>,
    ) -> anyhow::Result<Vec<BackupInfo>> {
        let request = self.auth_request(tonic::Request::new(ListBackupsRequest {
            app_name: app_name.map(|s| s.to_string()),
        }));
        let response = self
            .inner
            .list_backups(request)
            .await
            .context("gRPC ListBackups failed")?
            .into_inner();
        Ok(response.backups)
    }

    /// Restore a backup by ID.
    pub async fn restore_backup(
        &mut self,
        backup_id: &str,
    ) -> anyhow::Result<RestoreBackupResponse> {
        let request = self.auth_request(tonic::Request::new(RestoreBackupRequest {
            backup_id: backup_id.to_string(),
        }));
        let response = self
            .inner
            .restore_backup(request)
            .await
            .context(format!("gRPC RestoreBackup failed for '{backup_id}'"))?
            .into_inner();
        Ok(response)
    }

    // ── Gateway (APISIX) ──────────────────────────────────────────────

    /// Get gateway status (enabled, version, uptime).
    pub async fn get_gateway_status(
        &mut self,
    ) -> anyhow::Result<GetGatewayStatusResponse> {
        let request = self.auth_request(tonic::Request::new(GetGatewayStatusRequest {}));
        let response = self
            .inner
            .get_gateway_status(request)
            .await
            .context("gRPC GetGatewayStatus failed")?
            .into_inner();
        Ok(response)
    }

    /// List all APISIX routes managed by bosun.
    pub async fn list_gateway_routes(
        &mut self,
    ) -> anyhow::Result<ListGatewayRoutesResponse> {
        let request = self.auth_request(tonic::Request::new(ListGatewayRoutesRequest {}));
        let response = self
            .inner
            .list_gateway_routes(request)
            .await
            .context("gRPC ListGatewayRoutes failed")?
            .into_inner();
        Ok(response)
    }

    /// Enable a plugin on a gateway route.
    pub async fn enable_gateway_plugin(
        &mut self,
        app_name: &str,
        plugin_name: &str,
        config_json: &str,
    ) -> anyhow::Result<()> {
        let request = self.auth_request(tonic::Request::new(EnableGatewayPluginRequest {
            app_name: app_name.to_string(),
            plugin_name: plugin_name.to_string(),
            config_json: config_json.to_string(),
        }));
        self.inner
            .enable_gateway_plugin(request)
            .await
            .context(format!(
                "gRPC EnableGatewayPlugin failed for '{app_name}': plugin={plugin_name}"
            ))?;
        Ok(())
    }

    /// Disable a plugin on a gateway route.
    pub async fn disable_gateway_plugin(
        &mut self,
        app_name: &str,
        plugin_name: &str,
    ) -> anyhow::Result<()> {
        let request = self.auth_request(tonic::Request::new(DisableGatewayPluginRequest {
            app_name: app_name.to_string(),
            plugin_name: plugin_name.to_string(),
        }));
        self.inner
            .disable_gateway_plugin(request)
            .await
            .context(format!(
                "gRPC DisableGatewayPlugin failed for '{app_name}': plugin={plugin_name}"
            ))?;
        Ok(())
    }

    /// Get cache statistics for a gateway route.
    pub async fn get_gateway_cache_stats(
        &mut self,
        app_name: &str,
    ) -> anyhow::Result<GetGatewayCacheStatsResponse> {
        let request = self.auth_request(tonic::Request::new(GetGatewayCacheStatsRequest {
            app_name: app_name.to_string(),
        }));
        let response = self
            .inner
            .get_gateway_cache_stats(request)
            .await
            .context(format!("gRPC GetGatewayCacheStats failed for '{app_name}'"))?
            .into_inner();
        Ok(response)
    }

    /// Purge the cache for a gateway route.
    pub async fn purge_gateway_cache(
        &mut self,
        app_name: &str,
    ) -> anyhow::Result<()> {
        let request = self.auth_request(tonic::Request::new(PurgeGatewayCacheRequest {
            app_name: app_name.to_string(),
        }));
        self.inner
            .purge_gateway_cache(request)
            .await
            .context(format!("gRPC PurgeGatewayCache failed for '{app_name}'"))?;
        Ok(())
    }

    /// Get Prometheus metrics from APISIX.
    pub async fn get_gateway_metrics(
        &mut self,
    ) -> anyhow::Result<GetGatewayMetricsResponse> {
        let request = self.auth_request(tonic::Request::new(GetGatewayMetricsRequest {}));
        let response = self
            .inner
            .get_gateway_metrics(request)
            .await
            .context("gRPC GetGatewayMetrics failed")?
            .into_inner();
        Ok(response)
    }

    // ── Security ──────────────────────────────────────────────────

    /// Get security engine status.
    pub async fn get_security_status(
        &mut self,
    ) -> anyhow::Result<GetSecurityStatusResponse> {
        let request = self.auth_request(tonic::Request::new(GetSecurityStatusRequest {}));
        let response = self
            .inner
            .get_security_status(request)
            .await
            .context("gRPC GetSecurityStatus failed")?
            .into_inner();
        Ok(response)
    }

    /// Get list of active security decisions (banned IPs).
    pub async fn get_security_decisions(
        &mut self,
    ) -> anyhow::Result<GetSecurityDecisionsResponse> {
        let request = self.auth_request(tonic::Request::new(GetSecurityDecisionsRequest {}));
        let response = self
            .inner
            .get_security_decisions(request)
            .await
            .context("gRPC GetSecurityDecisions failed")?
            .into_inner();
        Ok(response)
    }
}
