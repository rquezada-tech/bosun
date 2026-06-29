//! APISIX API Gateway integration via its Admin API.
//!
//! Uses APISIX's Admin API (default http://localhost:9180/apisix/admin) to
//! manage routes, plugins (rate-limit, proxy-cache, JWT auth, etc.), cache,
//! and Prometheus metrics for deployed applications.
//!
//! Route IDs use the pattern `bosun-{app_name}` so they can be individually
//! managed and removed.

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Default base URL for the APISIX Admin API.
const DEFAULT_ADMIN_URL: &str = "http://localhost:9180/apisix/admin";

/// Client for APISIX's Admin API.
///
/// Manages routes, plugins, caching, and observability for deployed
/// applications. Each app gets a route with `id = "bosun-{app_name}"`.
#[derive(Debug, Clone)]
pub struct GatewayClient {
    admin_url: String,
    client: Client,
}

/// An APISIX upstream node definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpstreamNode {
    nodes: std::collections::BTreeMap<String, u32>,
    #[serde(rename = "type", default = "default_lb_type")]
    lb_type: String,
}

fn default_lb_type() -> String {
    "roundrobin".to_string()
}

/// An APISIX route definition for the Admin API.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApisixRoute {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream: Option<UpstreamNode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plugins: Option<serde_json::Map<String, JsonValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u32>,
}

/// Response wrapper from APISIX Admin API.
#[derive(Debug, Clone, Deserialize)]
struct ApisixResponse<T> {
    #[serde(default)]
    value: Option<T>,
    #[serde(default)]
    node: Option<ApisixNode<T>>,
}

/// Node wrapper in APISIX response (sometimes nested under "node").
#[derive(Debug, Clone, Deserialize)]
struct ApisixNode<T> {
    #[serde(default)]
    value: Option<T>,
    #[serde(default)]
    nodes: Option<Vec<ApisixNodeItem>>,
}

/// Individual node item (for route listings).
#[derive(Debug, Clone, Deserialize)]
struct ApisixNodeItem {
    #[serde(default)]
    value: Option<ApisixRoute>,
}

impl GatewayClient {
    /// Build the route ID for an app.
    fn route_id(app_name: &str) -> String {
        format!("bosun-{}", app_name)
    }

    /// Create a new GatewayClient with the default Admin API URL.
    ///
    /// Returns None if APISIX is not reachable.
    pub async fn connect() -> Option<Self> {
        Self::with_admin_url(DEFAULT_ADMIN_URL).await
    }

    /// Create a new GatewayClient with a custom Admin API URL.
    ///
    /// Returns None if APISIX is not reachable.
    pub async fn with_admin_url(admin_url: impl Into<String>) -> Option<Self> {
        let admin_url = admin_url.into();
        let client = Client::new();

        // Ping the APISIX Admin API to verify reachability
        let url = format!("{}/routes", admin_url);
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!("APISIX Admin API reachable at {}", admin_url);
                Some(Self { admin_url, client })
            }
            Ok(resp) => {
                tracing::warn!(
                    "APISIX Admin API at {} returned {}: {}",
                    admin_url,
                    resp.status(),
                    resp.text().await.unwrap_or_default()
                );
                None
            }
            Err(e) => {
                tracing::warn!(
                    "APISIX Admin API unreachable at {}: {}. Gateway features disabled.",
                    admin_url,
                    e
                );
                None
            }
        }
    }

    /// Get the APISIX version and uptime.
    pub async fn get_status(&self) -> anyhow::Result<GatewayStatusInfo> {
        // Try to get a simple status by checking if the API responds
        let url = format!("{}/routes", self.admin_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to reach APISIX: {e}"))?;

        if !resp.status().is_success() {
            anyhow::bail!("APISIX returned {}", resp.status());
        }

        // APISIX doesn't expose a simple version endpoint via Admin API,
        // but we can check if it's reachable and report it as enabled.
        Ok(GatewayStatusInfo {
            enabled: true,
            version: "APISIX (Admin API reachable)".to_string(),
            uptime: "running".to_string(),
        })
    }

    /// Configure a reverse proxy route for an application.
    ///
    /// Creates a route in APISIX that proxies requests matching
    /// `domain` to `localhost:port`.
    pub async fn configure_route(
        &self,
        app_name: &str,
        domain: &str,
        port: u16,
        upstream_path: Option<&str>,
    ) -> anyhow::Result<()> {
        let route_id = Self::route_id(app_name);
        let uri = upstream_path.unwrap_or("/*").to_string();

        let mut nodes = std::collections::BTreeMap::new();
        nodes.insert(format!("localhost:{}", port), 1);

        let route = ApisixRoute {
            id: Some(route_id.clone()),
            name: Some(app_name.to_string()),
            uri: Some(uri),
            host: Some(domain.to_string()),
            upstream: Some(UpstreamNode {
                nodes,
                lb_type: "roundrobin".to_string(),
            }),
            plugins: None,
            status: Some(1),
        };

        let url = format!("{}/routes/{}", self.admin_url, route_id);

        tracing::info!(
            "Configuring APISIX route for {} ({} -> localhost:{})",
            app_name,
            domain,
            port
        );

        let resp = self
            .client
            .put(&url)
            .json(&route)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to configure APISIX route for {app_name}: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "APISIX returned {status} when configuring route for {app_name}: {body}"
            );
        }

        tracing::info!("APISIX route configured for {}", app_name);
        Ok(())
    }

    /// Remove a route for an application.
    ///
    /// Idempotent: succeeds silently if the route doesn't exist.
    pub async fn remove_route(&self, app_name: &str) -> anyhow::Result<()> {
        let route_id = Self::route_id(app_name);
        let url = format!("{}/routes/{}", self.admin_url, route_id);

        tracing::info!("Removing APISIX route for {} (id={})", app_name, route_id);

        let resp = self
            .client
            .delete(&url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to remove APISIX route for {app_name}: {e}"))?;

        let status = resp.status();
        if status.is_success() {
            tracing::info!("APISIX route removed for {}", app_name);
        } else if status == reqwest::StatusCode::NOT_FOUND {
            tracing::info!(
                "APISIX route for {} not found (already removed or never configured)",
                app_name
            );
        } else {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "APISIX returned {status} when removing route for {app_name}: {body}"
            );
        }

        Ok(())
    }

    /// Add or update a plugin on an existing route.
    ///
    /// `config_json` should be a JSON string with the plugin configuration,
    /// e.g. `{"count": 100, "time_window": 60}` for rate-limit.
    pub async fn enable_plugin(
        &self,
        app_name: &str,
        plugin_name: &str,
        config_json: &str,
    ) -> anyhow::Result<()> {
        let route_id = Self::route_id(app_name);

        // Parse the config JSON
        let config: JsonValue = serde_json::from_str(config_json)
            .map_err(|e| anyhow::anyhow!("Invalid plugin config JSON: {e}"))?;

        tracing::info!(
            "Enabling plugin '{}' on route '{}'",
            plugin_name,
            route_id
        );

        // Build a PATCH request that adds the plugin to the route's plugins map
        // The key path in APISIX PATCH is: /plugins/{plugin_name}
        let patch_url = format!("{}/routes/{}/plugins/{}", self.admin_url, route_id, plugin_name);

        let resp = self
            .client
            .put(&patch_url)
            .json(&config)
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to enable plugin '{plugin_name}' on route '{app_name}': {e}"
                )
            })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "APISIX returned {status} when enabling plugin '{plugin_name}' on '{app_name}': {body}"
            );
        }

        tracing::info!(
            "Plugin '{}' enabled on route '{}'",
            plugin_name,
            app_name
        );
        Ok(())
    }

    /// Remove a plugin from a route.
    pub async fn disable_plugin(
        &self,
        app_name: &str,
        plugin_name: &str,
    ) -> anyhow::Result<()> {
        let route_id = Self::route_id(app_name);

        tracing::info!(
            "Disabling plugin '{}' on route '{}'",
            plugin_name,
            route_id
        );

        let url = format!(
            "{}/routes/{}/plugins/{}",
            self.admin_url, route_id, plugin_name
        );

        let resp = self
            .client
            .delete(&url)
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to disable plugin '{plugin_name}' on route '{app_name}': {e}"
                )
            })?;

        let status = resp.status();
        if status.is_success() {
            tracing::info!("Plugin '{}' disabled on route '{}'", plugin_name, app_name);
        } else if status == reqwest::StatusCode::NOT_FOUND {
            tracing::info!(
                "Plugin '{}' not found on route '{}' (already disabled)",
                plugin_name,
                app_name
            );
        } else {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "APISIX returned {status} when disabling plugin '{plugin_name}' on '{app_name}': {body}"
            );
        }

        Ok(())
    }

    /// Enable proxy-cache plugin on a route.
    ///
    /// `ttl_secs`: cache TTL in seconds.
    /// `strategy`: cache strategy (e.g., "disk", "memory").
    pub async fn enable_cache(
        &self,
        app_name: &str,
        ttl_secs: u64,
        strategy: &str,
    ) -> anyhow::Result<()> {
        let config = serde_json::json!({
            "cache_strategy": strategy,
            "cache_ttl": ttl_secs,
            "cache_http_status": [200, 301, 302],
            "cache_method": ["GET", "HEAD"],
        });
        self.enable_plugin(app_name, "proxy-cache", &config.to_string())
            .await
    }

    /// Disable proxy-cache plugin on a route.
    pub async fn disable_cache(&self, app_name: &str) -> anyhow::Result<()> {
        self.disable_plugin(app_name, "proxy-cache").await
    }

    /// Get cache statistics for an app.
    ///
    /// Returns (hits, misses, size_bytes).
    pub async fn get_cache_stats(
        &self,
        app_name: &str,
    ) -> anyhow::Result<CacheStats> {
        let route_id = Self::route_id(app_name);

        // APISIX doesn't have a direct per-route cache stats endpoint.
        // We fetch the route and check if proxy-cache plugin is configured,
        // then try to get Prometheus metrics filtered for this route.
        let url = format!("{}/routes/{}", self.admin_url, route_id);

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get route for cache stats: {e}"))?;

        if !resp.status().is_success() {
            anyhow::bail!("APISIX returned {} when getting route", resp.status());
        }

        let body = resp.text().await?;

        // Try to parse hits from Prometheus-style metrics if available
        // First, extract if proxy-cache is configured
        let has_cache = body.contains("proxy-cache");

        if !has_cache {
            return Ok(CacheStats {
                app_name: app_name.to_string(),
                hits: 0,
                misses: 0,
                size_bytes: 0,
            });
        }

        // Try to get Prometheus metrics for cache info
        let metrics_url = format!("{}/../prometheus/metrics", self.admin_url);
        match self.client.get(&metrics_url).send().await {
            Ok(metrics_resp) if metrics_resp.status().is_success() => {
                let metrics_text = metrics_resp.text().await.unwrap_or_default();

                let hits = Self::parse_prometheus_metric(&metrics_text, "apisix_cache_hit_count", Some(route_id.as_str()));
                let misses = Self::parse_prometheus_metric(&metrics_text, "apisix_cache_miss_count", Some(route_id.as_str()));
                let size = Self::parse_prometheus_metric(&metrics_text, "apisix_cache_size_bytes", Some(route_id.as_str()));

                Ok(CacheStats {
                    app_name: app_name.to_string(),
                    hits,
                    misses,
                    size_bytes: size,
                })
            }
            _ => {
                Ok(CacheStats {
                    app_name: app_name.to_string(),
                    hits: 0,
                    misses: 0,
                    size_bytes: 0,
                })
            }
        }
    }

    /// Get Prometheus metrics from APISIX.
    pub async fn get_metrics(&self) -> anyhow::Result<String> {
        let metrics_url = format!("{}/../prometheus/metrics", self.admin_url);

        let resp = self
            .client
            .get(&metrics_url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to fetch APISIX metrics: {e}"))?;

        if !resp.status().is_success() {
            anyhow::bail!("APISIX returned {} for metrics", resp.status());
        }

        resp.text()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read metrics response: {e}"))
    }

    /// List all bosun-managed routes in APISIX.
    pub async fn list_routes(&self) -> anyhow::Result<Vec<RouteInfo>> {
        let url = format!("{}/routes", self.admin_url);

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list APISIX routes: {e}"))?;

        if !resp.status().is_success() {
            anyhow::bail!("APISIX returned {} when listing routes", resp.status());
        }

        let body_text = resp.text().await?;
        let body: JsonValue = serde_json::from_str(&body_text)
            .map_err(|e| anyhow::anyhow!("Failed to parse routes response: {e}"))?;

        let mut routes = Vec::new();

        // APISIX returns routes in different formats; handle both
        if let Some(list) = body.get("list") {
            // Format: { "list": [ { "value": { "id": "..." } } ] }
            if let Some(items) = list.as_array() {
                for item in items {
                    if let Some(route_value) = item.get("value") {
                        if let Some(id) = route_value.get("id").and_then(|v| v.as_str()) {
                            if id.starts_with("bosun-") {
                                let app_name = id.strip_prefix("bosun-").unwrap_or(id);
                                let domain = route_value
                                    .get("host")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("-")
                                    .to_string();
                                let port = route_value
                                    .get("upstream")
                                    .and_then(|u| u.get("nodes"))
                                    .and_then(|n| n.as_object())
                                    .and_then(|nodes| nodes.keys().next())
                                    .and_then(|addr| {
                                        addr.split(':').nth(1)
                                    })
                                    .and_then(|p| p.parse::<u32>().ok())
                                    .unwrap_or(0);
                                let uri = route_value
                                    .get("uri")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("/*")
                                    .to_string();
                                let plugins: Vec<String> = route_value
                                    .get("plugins")
                                    .and_then(|p| p.as_object())
                                    .map(|obj| obj.keys().cloned().collect())
                                    .unwrap_or_default();

                                routes.push(RouteInfo {
                                    name: app_name.to_string(),
                                    domain,
                                    port,
                                    plugins,
                                    uri,
                                });
                            }
                        }
                    }
                }
            }
        } else if let Some(node) = body.get("node") {
            // Format: { "node": { "nodes": [ { "value": { "id": "..." } } ] } }
            if let Some(nodes) = node.get("nodes").and_then(|n| n.as_array()) {
                for item in nodes {
                    if let Some(route_value) = item.get("value") {
                        if let Some(id) = route_value.get("id").and_then(|v| v.as_str()) {
                            if id.starts_with("bosun-") {
                                let app_name = id.strip_prefix("bosun-").unwrap_or(id);
                                let domain = route_value
                                    .get("host")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("-")
                                    .to_string();
                                let port = route_value
                                    .get("upstream")
                                    .and_then(|u| u.get("nodes"))
                                    .and_then(|n| n.as_object())
                                    .and_then(|nodes| nodes.keys().next())
                                    .and_then(|addr| {
                                        addr.split(':').nth(1)
                                    })
                                    .and_then(|p| p.parse::<u32>().ok())
                                    .unwrap_or(0);
                                let uri = route_value
                                    .get("uri")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("/*")
                                    .to_string();
                                let plugins: Vec<String> = route_value
                                    .get("plugins")
                                    .and_then(|p| p.as_object())
                                    .map(|obj| obj.keys().cloned().collect())
                                    .unwrap_or_default();

                                routes.push(RouteInfo {
                                    name: app_name.to_string(),
                                    domain,
                                    port,
                                    plugins,
                                    uri,
                                });
                            }
                        }
                    }
                }
            }
        }

        Ok(routes)
    }

    /// Parse a Prometheus metric value from the metrics text.
    fn parse_prometheus_metric(text: &str, metric_name: &str, filter: Option<&str>) -> u64 {
        for line in text.lines() {
            if line.starts_with(metric_name) {
                if let Some(f) = filter {
                    if !line.contains(f) {
                        continue;
                    }
                }
                if let Some(val_str) = line.split_whitespace().last() {
                    if let Ok(val) = val_str.parse::<f64>() {
                        return val as u64;
                    }
                }
            }
        }
        0
    }
}

/// Information about the gateway status.
#[derive(Debug, Clone)]
pub struct GatewayStatusInfo {
    pub enabled: bool,
    pub version: String,
    pub uptime: String,
}

/// Cache statistics for a route.
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub app_name: String,
    pub hits: u64,
    pub misses: u64,
    pub size_bytes: u64,
}

/// Information about a configured route.
#[derive(Debug, Clone)]
pub struct RouteInfo {
    pub name: String,
    pub domain: String,
    pub port: u32,
    pub plugins: Vec<String>,
    pub uri: String,
}
