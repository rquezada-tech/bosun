//! Deploy strategy module.
//!
//! Implements strategy dispatch for Direct, Rolling, and BlueGreen deployments.
//! Orchestrates Docker container lifecycle and Caddy proxy updates based on
//! the chosen zero-downtime strategy.

use std::collections::HashMap;
use std::path::Path;

use crate::docker::DockerClient;
use crate::proxy::CaddyClient;

/// Supported deploy strategies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeployStrategy {
    /// Stop old, start new — brief downtime (~2-5s).
    Direct,
    /// Graceful stop, remove, start with same port.
    Rolling,
    /// Maintain two color containers, swap Caddy for zero downtime.
    BlueGreen,
}

impl DeployStrategy {
    /// Parse from a string (case-insensitive).
    /// Defaults to Direct for unknown values.
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "direct" => DeployStrategy::Direct,
            "rolling" => DeployStrategy::Rolling,
            "bluegreen" | "blue-green" | "blue_green" => DeployStrategy::BlueGreen,
            _ => {
                tracing::warn!(
                    "Unknown deploy strategy '{}', falling back to Direct",
                    s
                );
                DeployStrategy::Direct
            }
        }
    }

    /// Display name for logging.
    pub fn as_str(&self) -> &'static str {
        match self {
            DeployStrategy::Direct => "direct",
            DeployStrategy::Rolling => "rolling",
            DeployStrategy::BlueGreen => "blue-green",
        }
    }
}

/// Execute a deploy using the given strategy.
///
/// Returns the port the app is running on (for Caddy updates).
/// For Direct and Rolling, this is the requested port.
/// For BlueGreen, this is the temporary high port of the new color.
pub async fn execute_deploy(
    strategy: DeployStrategy,
    docker: &DockerClient,
    proxy: Option<&CaddyClient>,
    build_dir: &Path,
    app_name: &str,
    domain: Option<&str>,
    port: u16,
    env_vars: &HashMap<String, String>,
) -> anyhow::Result<(String, u16)> {
    tracing::info!(
        "Executing {} deploy for '{}' (domain={:?}, port={})",
        strategy.as_str(),
        app_name,
        domain,
        port
    );

    match strategy {
        DeployStrategy::Direct => {
            docker
                .deploy(build_dir, app_name, domain, port, env_vars)
                .await?;

            // Health check
            docker.wait_for_container_healthy(app_name, 30).await?;

            // Update Caddy if domain is set
            if let (Some(d), Some(p)) = (domain, proxy) {
                p.configure_app(d, port).await?;
            }

            Ok((app_name.to_string(), port))
        }

        DeployStrategy::Rolling => {
            docker
                .deploy_rolling(build_dir, app_name, domain, port, env_vars)
                .await?;

            // Health check is already done in deploy_rolling.
            // Update Caddy if domain is set.
            if let (Some(d), Some(p)) = (domain, proxy) {
                p.configure_app(d, port).await?;
                tracing::info!("Caddy updated for rolling deploy: {} -> localhost:{}", d, port);
            }

            Ok((app_name.to_string(), port))
        }

        DeployStrategy::BlueGreen => {
            let (active_color, _) = docker
                .determine_blue_green_colors(app_name)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to determine blue-green colors: {}", e))?;

            docker
                .deploy_blue_green(build_dir, app_name, domain, port, env_vars)
                .await?;

            // Health check is already done in deploy_blue_green.
            // Now determine the new active color and its port for Caddy.
            let (new_active, _) = docker
                .determine_blue_green_colors(app_name)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to determine blue-green colors: {}", e))?;

            // Get the port of the new active container
            let new_container_name = format!("{}-{}", app_name, new_active);
            let active_port = docker.get_container_port(&new_container_name, port).await;

            // Update Caddy to point to the new active container's port
            if let (Some(d), Some(p)) = (domain, proxy) {
                tracing::info!(
                    "Swapping Caddy reverse proxy: {} -> localhost:{} (new {} color)",
                    d,
                    active_port,
                    new_active
                );
                p.configure_app(d, active_port).await?;
            }

            tracing::info!(
                "Blue-green deploy complete: {} is active on port {} (was {})",
                new_active,
                active_port,
                active_color
            );

            Ok((new_container_name, active_port))
        }
    }
}

/// Execute a rollback for a blue-green deployed app.
///
/// Swaps Caddy back to the inactive (previous) color.
pub async fn execute_rollback(
    docker: &DockerClient,
    proxy: Option<&CaddyClient>,
    app_name: &str,
    domain: Option<&str>,
) -> anyhow::Result<()> {
    let (rollback_color, rollback_port) = docker.rollback_blue_green(app_name).await?;

    if let (Some(d), Some(p)) = (domain, proxy) {
        tracing::info!(
            "Rollback: swapping Caddy to {} (port {})",
            rollback_color,
            rollback_port
        );
        p.configure_app(d, rollback_port).await?;
    }

    tracing::info!(
        "Rollback complete: {} is now active on port {}",
        rollback_color,
        rollback_port
    );

    Ok(())
}

/// Execute a promote for a blue-green deployed app.
///
/// Removes the inactive color container, making the active color permanent.
pub async fn execute_promote(
    docker: &DockerClient,
    app_name: &str,
) -> anyhow::Result<String> {
    let color = docker.promote_blue_green(app_name).await?;
    tracing::info!("Promoted: {} is now the permanent deployment", color);
    Ok(color)
}
