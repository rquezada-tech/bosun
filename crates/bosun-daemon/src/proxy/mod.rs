//! Caddy reverse proxy integration via its Admin API.
//!
//! Uses Caddy's Admin API (default http://localhost:2019) to configure
//! reverse proxy routes for deployed applications. Routes are managed
//! per-domain with `@id` tags for easy removal.
//!
//! The Caddy config structure for a single app route:
//! ```json
//! {
//!   "@id": "bosun-DOMAIN",
//!   "match": [{"host": ["DOMAIN"]}],
//!   "handle": [{"handler": "reverse_proxy", "upstreams": [{"dial": "localhost:PORT"}]}]
//! }
//! ```

use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Default base URL for the Caddy Admin API.
const DEFAULT_BASE_URL: &str = "http://localhost:2019";

/// A route handler configuration for Caddy's reverse_proxy.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReverseProxyHandler {
    handler: String,
    upstreams: Vec<Upstream>,
}

/// An upstream server definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Upstream {
    dial: String,
}

/// A match condition for a Caddy route.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Match {
    host: Vec<String>,
}

/// A Caddy route definition for the Admin API.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Route {
    /// Optional ID for referencing this route (e.g. for deletion).
    #[serde(rename = "@id", skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(rename = "match", skip_serializing_if = "Option::is_none")]
    match_condition: Option<Vec<Match>>,
    handle: Option<Vec<ReverseProxyHandler>>,
}

/// Top-level Caddy config structure (partial, only what we need).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
struct CaddyConfig {
    apps: Option<AppsConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
struct AppsConfig {
    http: Option<HttpConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
struct HttpConfig {
    servers: Option<std::collections::BTreeMap<String, ServerConfig>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
struct ServerConfig {
    routes: Option<Vec<Route>>,
}

/// Client for Caddy's Admin API.
///
/// Manages reverse proxy routes for deployed applications. Each app
/// gets a route tagged with `@id = "bosun-{domain}"` so it can be
/// individually removed later.
#[derive(Debug, Clone)]
pub struct CaddyClient {
    base_url: String,
    client: Client,
}

impl CaddyClient {
    /// Create a new CaddyClient with the default base URL.
    ///
    /// Returns an error if Caddy is not reachable.
    pub async fn new() -> anyhow::Result<Self> {
        Self::with_base_url(DEFAULT_BASE_URL).await
    }

    /// Create a new CaddyClient with a custom base URL.
    ///
    /// Returns an error if Caddy is not reachable.
    pub async fn with_base_url(base_url: impl Into<String>) -> anyhow::Result<Self> {
        let base_url = base_url.into();
        let client = Client::new();

        // Check Caddy is reachable
        let url = format!("{}/config/", base_url);
        let resp = client.get(&url).send().await.map_err(|e| {
            anyhow::anyhow!("Caddy Admin API unreachable at {base_url}: {e}")
        })?;

        if !resp.status().is_success() {
            anyhow::bail!(
                "Caddy Admin API returned {} at {base_url}",
                resp.status()
            );
        }

        tracing::info!("Caddy Admin API reachable at {base_url}");
        Ok(Self { base_url, client })
    }

    /// Build the `@id` tag for a domain's route.
    fn route_id(domain: &str) -> String {
        format!("bosun-{}", domain)
    }

    /// Configure a reverse proxy route for an application.
    ///
    /// Posts a new route to Caddy that proxies requests matching
    /// `domain` to `localhost:port`. If a route for this domain
    /// already exists (same `@id`), Caddy will replace it.
    pub async fn configure_app(&self, domain: &str, port: u16) -> anyhow::Result<()> {
        let route = Route {
            id: Some(Self::route_id(domain)),
            match_condition: Some(vec![Match {
                host: vec![domain.to_string()],
            }]),
            handle: Some(vec![ReverseProxyHandler {
                handler: "reverse_proxy".to_string(),
                upstreams: vec![Upstream {
                    dial: format!("localhost:{}", port),
                }],
            }]),
        };

        let url = format!(
            "{}/config/apps/http/servers/srv0/routes",
            self.base_url
        );

        tracing::info!(
            "Configuring Caddy route for {} -> localhost:{}",
            domain,
            port
        );

        let resp = self
            .client
            .post(&url)
            .json(&route)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to configure Caddy route for {domain}: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Caddy returned {status} when configuring route for {domain}: {body}"
            );
        }

        tracing::info!("Caddy route configured for {}", domain);
        Ok(())
    }

    /// Remove the reverse proxy route for an application.
    ///
    /// Deletes the route by its `@id` tag. If no route exists for the
    /// given domain, this succeeds silently (idempotent).
    #[allow(dead_code)]
    pub async fn remove_app(&self, domain: &str) -> anyhow::Result<()> {
        let route_id = Self::route_id(domain);
        let url = format!("{}/id/{}", self.base_url, route_id);

        tracing::info!("Removing Caddy route for {} (id={})", domain, route_id);

        let resp = self
            .client
            .delete(&url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to remove Caddy route for {domain}: {e}"))?;

        let status = resp.status();
        if status.is_success() {
            tracing::info!("Caddy route removed for {}", domain);
        } else if status == reqwest::StatusCode::NOT_FOUND {
            tracing::info!(
                "Caddy route for {} not found (already removed or never configured)",
                domain
            );
        } else {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Caddy returned {status} when removing route for {domain}: {body}"
            );
        }

        Ok(())
    }

    /// Clean up the reverse proxy route for an application (idempotent).
    ///
    /// Convenience wrapper around [`remove_app`] used during container
    /// stop/removal flows. Does not error if the route doesn't exist.
    #[allow(dead_code)]
    pub async fn cleanup_app(&self, domain: &str) -> anyhow::Result<()> {
        self.remove_app(domain).await
    }

    /// List all currently configured routes.
    ///
    /// Returns the raw JSON config from Caddy for debugging/inspection.
    #[allow(dead_code)]
    pub async fn list_routes(&self) -> anyhow::Result<String> {
        let url = format!("{}/config/", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to fetch Caddy config: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Caddy returned {status} when fetching config: {body}");
        }

        resp.text()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read Caddy response body: {e}"))
    }
}
