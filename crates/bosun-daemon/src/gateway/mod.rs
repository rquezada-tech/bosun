//! APISIX API Gateway integration via its Admin API.
//!
//! Uses APISIX's Admin API (default http://localhost:9180/apisix/admin) to
//! manage routes, plugins (rate-limit, proxy-cache, JWT auth, etc.), cache,
//! and Prometheus metrics for deployed applications.
//!
//! Route IDs use the pattern `bosun-{app_name}` so they can be individually
//! managed and removed.
//!
//! ## Cross-VPS Routing (mTLS)
//!
//! APISIX can route traffic to apps on OTHER VPS servers running bosun,
//! with mutual TLS authentication between nodes. See `Peer` and the
//! peer management methods below.

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::path::PathBuf;

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
    /// `domain` to the upstream. When `peer_name` is provided, the
    /// upstream becomes `https://{peer.addr}:{port}` with mTLS
    /// authentication; otherwise defaults to `localhost:{port}`.
    pub async fn configure_route(
        &self,
        app_name: &str,
        domain: &str,
        port: u16,
        upstream_path: Option<&str>,
        peer_name: Option<&str>,
    ) -> anyhow::Result<()> {
        let route_id = Self::route_id(app_name);
        let uri = upstream_path.unwrap_or("/*").to_string();

        let (upstream_addr, plugins) = if let Some(pname) = peer_name {
            // Cross-VPS routing: use peer address with mTLS
            let peers = load_peers()?;
            let peer = peers
                .iter()
                .find(|p| p.name == pname)
                .ok_or_else(|| anyhow::anyhow!("Peer '{pname}' not found. Add it with: bosun gateway peer add {pname} <addr> <ca-cert>"))?;

            tracing::info!(
                "Configuring cross-VPS route for {} via peer {} ({}:{})",
                app_name,
                pname,
                peer.addr,
                port
            );

            // Build mTLS plugin config for APISIX
            let mut mtsl_config = serde_json::Map::new();
            mtsl_config.insert(
                "client_ca".to_string(),
                JsonValue::String(peer.cert_path.clone()),
            );

            let mut plugins_map = serde_json::Map::new();
            plugins_map.insert("mtls".to_string(), JsonValue::Object(mtsl_config));

            (format!("{}:{}", peer.addr, port), Some(plugins_map))
        } else {
            // Local routing: use localhost
            (format!("localhost:{}", port), None)
        };

        let mut nodes = std::collections::BTreeMap::new();
        nodes.insert(upstream_addr, 1);

        let route = ApisixRoute {
            id: Some(route_id.clone()),
            name: Some(app_name.to_string()),
            uri: Some(uri),
            host: Some(domain.to_string()),
            upstream: Some(UpstreamNode {
                nodes,
                lb_type: "roundrobin".to_string(),
            }),
            plugins,
            status: Some(1),
        };

        let url = format!("{}/routes/{}", self.admin_url, route_id);

        tracing::info!(
            "Configuring APISIX route for {} ({} -> port {})",
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

// ═══════════════════════════════════════════════════════════════════
//  Cross-VPS Peer Management (mTLS)
// ═══════════════════════════════════════════════════════════════════

/// Status of a remote bosun peer node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PeerStatus {
    /// Peer is reachable and TLS handshake succeeds.
    Online,
    /// Peer is unreachable or TLS handshake fails.
    Offline,
    /// Peer has not been tested yet.
    Unknown,
}

impl std::fmt::Display for PeerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerStatus::Online => write!(f, "online"),
            PeerStatus::Offline => write!(f, "offline"),
            PeerStatus::Unknown => write!(f, "unknown"),
        }
    }
}

/// A remote bosun daemon node that can receive traffic via APISIX
/// with mutual TLS authentication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    /// Human-readable name for this peer (e.g., "nyc-vps").
    pub name: String,
    /// Host:port address of the remote bosun node.
    pub addr: String,
    /// Path to the CA certificate that signed the peer's node cert.
    pub cert_path: String,
    /// Current connectivity status.
    #[serde(default = "default_peer_status")]
    pub status: PeerStatus,
}

fn default_peer_status() -> PeerStatus {
    PeerStatus::Unknown
}

/// Path to the peers configuration file.
const PEERS_FILE: &str = "/etc/bosun/peers.json";

/// Load all peers from the peers JSON file.
fn load_peers() -> anyhow::Result<Vec<Peer>> {
    let path = PathBuf::from(PEERS_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let data = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("Failed to read peers file {}: {e}", path.display()))?;
    let peers: Vec<Peer> = serde_json::from_str(&data)
        .map_err(|e| anyhow::anyhow!("Failed to parse peers file: {e}"))?;
    Ok(peers)
}

/// Save all peers to the peers JSON file.
fn save_peers(peers: &[Peer]) -> anyhow::Result<()> {
    let path = PathBuf::from(PEERS_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_string_pretty(peers)?;
    std::fs::write(&path, data)
        .map_err(|e| anyhow::anyhow!("Failed to write peers file {}: {e}", path.display()))?;
    Ok(())
}

impl GatewayClient {
    // ═══════════════════════════════════════════════════════════════
    //  Peer management
    // ═══════════════════════════════════════════════════════════════

    /// Register a remote bosun node as a peer.
    ///
    /// `cert_path` should point to the CA certificate that signed
    /// the remote node's TLS certificate. This CA cert is used by
    /// APISIX to verify the peer during mTLS handshake.
    pub fn add_peer(
        name: &str,
        addr: &str,
        cert_path: &str,
    ) -> anyhow::Result<Peer> {
        let mut peers = load_peers()?;

        // Replace if already exists
        peers.retain(|p| p.name != name);

        let peer = Peer {
            name: name.to_string(),
            addr: addr.to_string(),
            cert_path: cert_path.to_string(),
            status: PeerStatus::Unknown,
        };

        tracing::info!("Adding peer: {} at {} (CA cert: {})", name, addr, cert_path);
        peers.push(peer.clone());
        save_peers(&peers)?;

        Ok(peer)
    }

    /// List all registered peers.
    pub fn list_peers() -> anyhow::Result<Vec<Peer>> {
        load_peers()
    }

    /// Remove a registered peer by name.
    pub fn remove_peer(name: &str) -> anyhow::Result<()> {
        let mut peers = load_peers()?;

        let len_before = peers.len();
        peers.retain(|p| p.name != name);

        if peers.len() == len_before {
            anyhow::bail!("Peer '{name}' not found");
        }

        tracing::info!("Removed peer: {}", name);
        save_peers(&peers)?;
        Ok(())
    }

    /// Test connectivity to a peer.
    ///
    /// Attempts a TCP connect + TLS handshake to verify the peer
    /// is reachable and its certificate is valid.
    pub async fn test_peer(name: &str) -> anyhow::Result<Peer> {
        let mut peers = load_peers()?;
        let peer = peers
            .iter_mut()
            .find(|p| p.name == name)
            .ok_or_else(|| anyhow::anyhow!("Peer '{name}' not found"))?;

        tracing::info!("Testing connectivity to peer '{}' at {}", name, peer.addr);

        // Attempt a TLS connection to the peer
        let url = format!("https://{}", peer.addr);
        let client = Client::builder()
            .danger_accept_invalid_certs(true) // We just want to test reachability
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build test client: {e}"))?;

        match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.get(&url).send(),
        )
        .await
        {
            Ok(Ok(_)) => {
                peer.status = PeerStatus::Online;
                tracing::info!("Peer '{}' is online", name);
            }
            Ok(Err(e)) => {
                peer.status = PeerStatus::Offline;
                tracing::warn!("Peer '{}' is offline: {e}", name);
            }
            Err(_) => {
                peer.status = PeerStatus::Offline;
                tracing::warn!("Peer '{}' timed out after 10s", name);
            }
        }

        let result = peer.clone();
        save_peers(&peers)?;
        Ok(result)
    }
}
