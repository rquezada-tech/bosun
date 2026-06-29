//! Multi-node cluster controller for managing remote bosun-daemon nodes.
//!
//! The `ClusterController` stores managed nodes in SQLite via the existing
//! `Store` and communicates with remote nodes via gRPC using `BosunClient`.
//! This enables multi-cloud orchestration: deploy apps across VPS nodes,
//! collect aggregated metrics, and manage the fleet from a single controller.

use std::collections::HashMap;
use std::sync::Arc;

use crate::persist::Store;
use crate::server::v1::{
    self, AddNodeRequest, AddNodeResponse, ClusterMetricsResponse, DeployToNodeRequest,
    DeployToNodeResponse, ListNodeResponse, NodeInfo, RemoveNodeRequest, RemoveNodeResponse,
};

/// The online/offline status of a managed node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeStatus {
    Online,
    Offline,
}

impl NodeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeStatus::Online => "online",
            NodeStatus::Offline => "offline",
        }
    }
}

/// A managed remote bosun-daemon node.
#[derive(Debug, Clone)]
pub struct Node {
    /// Human-readable name (e.g., "vps-2", "aws-us-east")
    pub name: String,
    /// gRPC endpoint address (e.g., "https://10.0.0.5:9090")
    pub addr: String,
    /// Current status (online/offline)
    pub status: NodeStatus,
    /// Labels for cloud provider, region, etc.
    pub labels: HashMap<String, String>,
}

impl Node {
    /// Convert to protobuf NodeInfo.
    pub fn to_proto(&self) -> NodeInfo {
        NodeInfo {
            name: self.name.clone(),
            addr: self.addr.clone(),
            status: self.status.as_str().to_string(),
            labels: self.labels.clone(),
            app_count: 0,   // filled in lazily
            cpu_percent: 0.0,
        }
    }
}

/// Multi-cloud orchestration controller.
///
/// Manages a fleet of bosun-daemon nodes across different VPS/cloud providers.
/// Nodes are persisted in SQLite so the cluster survives restarts.
///
/// In production, the controller connects to remote nodes via `BosunClient`.
/// For the MVP, we connect lazily and cache the channel.
pub struct ClusterController {
    store: Arc<Store>,
}

impl ClusterController {
    /// Create a new cluster controller backed by the given store.
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    // ── Node management ───────────────────────────────────────────────

    /// Register a remote bosun node.
    pub fn add_node(
        &self,
        name: &str,
        addr: &str,
        labels: HashMap<String, String>,
    ) -> anyhow::Result<AddNodeResponse> {
        tracing::info!("Adding node: name={name}, addr={addr}, labels={labels:?}");

        // Check if node already exists
        if let Some(existing) = self.get_node(name)? {
            anyhow::bail!(
                "Node '{name}' already exists with addr '{}'. Remove it first with 'bosun cluster remove --name {name}'.",
                existing.addr
            );
        }

        self.store.upsert_node(name, addr, &labels)?;

        tracing::info!("Node '{name}' added successfully");
        Ok(AddNodeResponse {
            name: name.to_string(),
            status: "added".to_string(),
        })
    }

    /// Remove a managed node.
    pub fn remove_node(&self, name: &str) -> anyhow::Result<RemoveNodeResponse> {
        tracing::info!("Removing node: {name}");

        // Verify the node exists
        self.get_node(name)?.ok_or_else(|| {
            anyhow::anyhow!("Node '{name}' not found. Use 'bosun cluster nodes' to list registered nodes.")
        })?;

        self.store.delete_node(name)?;

        tracing::info!("Node '{name}' removed successfully");
        Ok(RemoveNodeResponse {
            name: name.to_string(),
            status: "removed".to_string(),
        })
    }

    /// List all registered nodes with their status.
    pub fn list_nodes(&self) -> anyhow::Result<Vec<Node>> {
        let records = self.store.list_nodes()?;
        let mut nodes = Vec::with_capacity(records.len());

        for rec in records {
            let labels: HashMap<String, String> =
                serde_json::from_str(&rec.labels_json).unwrap_or_default();

            // For MVP, all registered nodes are considered online if they exist.
            // In production, we'd do a health check gRPC ping.
            let status = NodeStatus::Online;

            nodes.push(Node {
                name: rec.name,
                addr: rec.addr,
                status,
                labels,
            });
        }

        // Sort by name for deterministic output
        nodes.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(nodes)
    }

    /// Get a single node by name.
    pub fn get_node(&self, name: &str) -> anyhow::Result<Option<Node>> {
        match self.store.get_node(name)? {
            Some(rec) => {
                let labels: HashMap<String, String> =
                    serde_json::from_str(&rec.labels_json).unwrap_or_default();
                Ok(Some(Node {
                    name: rec.name,
                    addr: rec.addr,
                    status: NodeStatus::Online,
                    labels,
                }))
            }
            None => Ok(None),
        }
    }

    // ── Metrics ───────────────────────────────────────────────────────

    /// Fetch metrics from a single remote node.
    ///
    /// For the MVP, this returns the list of apps on the local daemon since
    /// full cross-node gRPC is Phase 2. The API is designed for future
    /// expansion to connect to remote nodes via `BosunClient`.
    pub async fn get_node_metrics(
        &self,
        node_name: &str,
        docker: &crate::docker::DockerClient,
        metrics: &crate::metrics::MetricCollector,
    ) -> anyhow::Result<Vec<v1::AppMetric>> {
        // Verify the node exists
        self.get_node(node_name)?
            .ok_or_else(|| anyhow::anyhow!("Node '{node_name}' not found"))?;

        // For MVP, collect metrics from local Docker (the node itself is local).
        // Future: connect to remote node via gRPC BosunClient.
        let apps = docker.list_bosun_apps().await?;

        let mut metrics_list = Vec::new();
        for app in &apps {
            if app.status() == v1::AppStatus::Running {
                match metrics.get_snapshot(&app.name).await {
                    Ok(m) => metrics_list.push(m),
                    Err(e) => {
                        tracing::warn!(
                            "Failed to get metrics for app '{}' on node '{}': {}",
                            app.name,
                            node_name,
                            e
                        );
                    }
                }
            }
        }

        Ok(metrics_list)
    }

    /// Aggregate metrics from all registered nodes.
    ///
    /// For the MVP, this collects metrics from the local daemon for each
    /// known node entry. Future versions will connect to remote nodes.
    pub async fn collect_cluster_metrics(
        &self,
        docker: &crate::docker::DockerClient,
        metrics: &crate::metrics::MetricCollector,
    ) -> anyhow::Result<Vec<NodeInfo>> {
        let nodes = self.list_nodes()?;
        let mut node_infos = Vec::with_capacity(nodes.len());

        for node in &nodes {
            let mut info = node.to_proto();

            // Collect metrics for this node
            match self.get_node_metrics(&node.name, docker, metrics).await {
                Ok(metrics_list) => {
                    info.app_count = metrics_list.len() as u32;
                    if !metrics_list.is_empty() {
                        let total_cpu: f64 =
                            metrics_list.iter().map(|m| m.cpu_percent).sum();
                        let total_ram: u64 =
                            metrics_list.iter().map(|m| m.ram_bytes).sum();
                        info.cpu_percent = total_cpu / metrics_list.len() as f64;
                        // For display, include total RAM in labels
                        let ram_mb = total_ram as f64 / 1_048_576.0;
                        info.labels
                            .insert("total_ram_mb".to_string(), format!("{:.1}", ram_mb));
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to collect metrics for node '{}': {}",
                        node.name,
                        e
                    );
                    info.status = "error".to_string();
                }
            }

            node_infos.push(info);
        }

        Ok(node_infos)
    }

    /// Delegate a deploy request to a specific node.
    ///
    /// For the MVP, this is a no-op placeholder since cross-node gRPC
    /// deployment requires the remote node's gRPC client, which is the
    /// Phase 2 goal. The API contract is established now.
    pub async fn deploy_to_node(
        &self,
        node_name: &str,
        _request: &DeployRequest,
    ) -> anyhow::Result<DeployToNodeResponse> {
        // Verify the node exists
        self.get_node(node_name)?
            .ok_or_else(|| anyhow::anyhow!("Node '{node_name}' not found"))?;

        // MVP: cross-node deploy not yet implemented.
        // Future: connect to remote node via BosunClient and forward the deploy.
        tracing::info!(
            "Deploy to node '{}' — cross-node deploy placeholder (MVP limitation)",
            node_name
        );

        Ok(DeployToNodeResponse {
            node_name: node_name.to_string(),
            status: "deploy_delegated_placeholder".to_string(),
            message: format!(
                "Cross-node deploy to '{}' not yet implemented (MVP). \
                 Deploy directly on that node for now.",
                node_name
            ),
        })
    }
}

// Re-export DeployRequest from v1 for convenience
use v1::DeployRequest;
