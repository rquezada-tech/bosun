//! Metric collection from Docker containers.
//!
//! Provides snapshot and streaming metrics using the Docker stats API.

use bollard::container::StatsOptions;
use bollard::Docker;
use crate::server::v1::AppMetric;
use futures_util::StreamExt;

/// Collector that gathers container metrics from Docker.
pub struct MetricCollector {
    docker: Docker,
}

impl MetricCollector {
    /// Create a new collector connected to the Docker daemon.
    pub fn new(docker: Docker) -> Self {
        Self { docker }
    }

    /// Get a single snapshot of metrics for a container.
    pub async fn get_snapshot(&self, container_name: &str) -> anyhow::Result<AppMetric> {
        let options = StatsOptions {
            stream: false,
            one_shot: true,
        };

        let mut stream = self.docker.stats(container_name, Some(options));
        match stream.next().await {
            Some(Ok(stats)) => {
                Ok(Self::stats_to_metric(container_name, &stats))
            }
            Some(Err(e)) => Err(anyhow::anyhow!("Stats error for {}: {}", container_name, e)),
            None => Err(anyhow::anyhow!("No stats received for {}", container_name)),
        }
    }

    /// Stream live metrics for a container.
    pub fn stream_live(
        &self,
        container_name: &str,
    ) -> impl futures_util::Stream<Item = anyhow::Result<AppMetric>> + '_ {
        let options = StatsOptions {
            stream: true,
            one_shot: false,
        };

        let name = container_name.to_string();
        self.docker
            .stats(container_name, Some(options))
            .map(move |result| match result {
                Ok(stats) => Ok(Self::stats_to_metric(&name, &stats)),
                Err(e) => Err(anyhow::anyhow!("Stats stream error for {}: {}", name, e)),
            })
    }

    /// Convert a Docker Stats struct into an AppMetric.
    fn stats_to_metric(container_name: &str, stats: &bollard::container::Stats) -> AppMetric {
        // CPU calculation: (cpu_delta / system_delta) * online_cpus * 100
        let cpu_percent = if let (Some(system_cpu), Some(online_cpus)) =
            (stats.cpu_stats.system_cpu_usage, stats.cpu_stats.online_cpus)
        {
            let precpu = &stats.precpu_stats;
            let cpu_delta = stats.cpu_stats.cpu_usage.total_usage.saturating_sub(
                precpu.cpu_usage.total_usage,
            ) as f64;
            let system_delta = system_cpu.saturating_sub(
                precpu.system_cpu_usage.unwrap_or(0),
            ) as f64;

            if system_delta > 0.0 && cpu_delta > 0.0 {
                (cpu_delta / system_delta) * online_cpus as f64 * 100.0
            } else {
                0.0
            }
        } else {
            0.0
        };

        // RAM usage in bytes
        let ram_bytes = stats.memory_stats.usage.unwrap_or(0);

        // Network: aggregate all interfaces
        let (net_rx_bytes, net_tx_bytes) = if let Some(ref networks) = stats.networks {
            let rx: u64 = networks.values().map(|n| n.rx_bytes).sum();
            let tx: u64 = networks.values().map(|n| n.tx_bytes).sum();
            (rx, tx)
        } else if let Some(ref net) = stats.network {
            (net.rx_bytes, net.tx_bytes)
        } else {
            (0, 0)
        };

        let timestamp_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        AppMetric {
            app_name: container_name.to_string(),
            cpu_percent,
            ram_bytes,
            net_rx_bytes,
            net_tx_bytes,
            timestamp_unix,
        }
    }
}
