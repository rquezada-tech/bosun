//! gRPC client wrapper for the Bosun daemon.
//!
//! Provides a typed async client that handles connection, TLS,
//! and timeout logic.

use anyhow::Context;
use crate::proto::bosun::v1::bosun_client::BosunClient as GrpcBosunClient;
use crate::proto::bosun::v1::*;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint, Identity};
use std::time::Duration;

/// Wrapper around the gRPC BosunClient with convenient methods.
pub struct BosunClient {
    inner: GrpcBosunClient<Channel>,
}

impl BosunClient {
    /// Connect to the bosun daemon at `addr`.
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

        Ok(Self {
            inner: GrpcBosunClient::new(channel),
        })
    }

    // ── App management ────────────────────────────────────────────

    /// List all deployed applications.
    pub async fn list_apps(&mut self) -> anyhow::Result<Vec<App>> {
        let response = self
            .inner
            .list_apps(ListAppsRequest {})
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
        let response = self
            .inner
            .get_app_logs(GetAppLogsRequest {
                app_name: app_name.to_string(),
                follow,
                tail_lines,
            })
            .await
            .context(format!("gRPC GetAppLogs failed for '{app_name}'"))?
            .into_inner();
        Ok(response)
    }

    /// Restart an application.
    pub async fn restart_app(&mut self, app_name: &str) -> anyhow::Result<()> {
        self.inner
            .restart_app(RestartAppRequest {
                app_name: app_name.to_string(),
            })
            .await
            .context(format!("gRPC RestartApp failed for '{app_name}'"))?;
        Ok(())
    }

    /// Scale an application to the given number of instances.
    pub async fn scale_app(&mut self, app_name: &str, instances: u32) -> anyhow::Result<()> {
        self.inner
            .scale_app(ScaleAppRequest {
                app_name: app_name.to_string(),
                instances,
            })
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
    ) -> anyhow::Result<DeployResponse> {
        let response = self
            .inner
            .deploy(DeployRequest {
                context_path: context_path.to_string(),
                domain: domain.map(|s| s.to_string()),
                enable_ssl,
                env,
                port,
            })
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
        let response = self
            .inner
            .get_metrics(GetMetricsRequest {
                app_name: app_name.map(|s| s.to_string()),
                live: false,
            })
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
        let response = self
            .inner
            .stream_metrics(GetMetricsRequest {
                app_name: app_name.map(|s| s.to_string()),
                live: true,
            })
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
        let response = self
            .inner
            .get_env(GetEnvRequest {
                app_name: app_name.to_string(),
            })
            .await
            .context(format!("gRPC GetEnv failed for '{app_name}'"))?
            .into_inner();
        Ok(response.env)
    }

    /// Set an environment variable for an app.
    pub async fn set_env(&mut self, app_name: &str, key: &str, value: &str) -> anyhow::Result<()> {
        self.inner
            .set_env(SetEnvRequest {
                app_name: app_name.to_string(),
                key: key.to_string(),
                value: value.to_string(),
            })
            .await
            .context(format!("gRPC SetEnv failed for '{app_name}': {key}={value}"))?;
        Ok(())
    }

    /// Remove an environment variable from an app.
    pub async fn unset_env(&mut self, app_name: &str, key: &str) -> anyhow::Result<()> {
        self.inner
            .unset_env(UnsetEnvRequest {
                app_name: app_name.to_string(),
                key: key.to_string(),
            })
            .await
            .context(format!("gRPC UnsetEnv failed for '{app_name}': {key}"))?;
        Ok(())
    }

    // ── Templates ──────────────────────────────────────────────────

    /// List available one-click app templates.
    pub async fn list_templates(&mut self) -> anyhow::Result<Vec<TemplateInfo>> {
        let response = self
            .inner
            .list_templates(ListTemplatesRequest {})
            .await
            .context("gRPC ListTemplates failed")?
            .into_inner();
        Ok(response.templates)
    }
}
