# Bosun Architecture & Implementation Plan — Phase 1: MVP

> **For future automation:** Use `subagent-driven-development` when delegating tasks.
> Each task is self-contained with exact files, commands, and verification.

**Goal:** Build a minimal PaaS daemon + CLI in Rust that can deploy Docker apps, show metrics, and manage SSL — all from the terminal.

**Discovery Summary:**
- Project structure: Cargo workspace with 2 crates (`bosun` CLI + `bosun-daemon`)
- gRPC API: protobuf at `proto/bosun/v1/bosun.proto`
- Docker interaction: `bollard` crate (Rust Docker client)
- Proof of concept: none yet — this is from scratch

**Architecture:**

```
┌──────────────────┐      gRPC+TLS        ┌──────────────────────────┐
│   bosun (CLI)    │◄────────────────────►│   bosun-daemon (server)  │
│   Rust binary    │                      │   Rust binary            │
│   ~8 MB          │                      │   ~10 MB                 │
└──────────────────┘                      │                          │
                                          │  ┌─────────────────────┐ │
                                          │  │ Docker (bollard)    │ │
                                          │  │ • deploy containers │ │
                                          │  │ • stream stats      │ │
                                          │  │ • manage volumes    │ │
                                          │  └─────────────────────┘ │
                                          │  ┌─────────────────────┐ │
                                          │  │ Metrics (rusqlite)  │ │
                                          │  │ • store time-series │ │
                                          │  │ • query by app/time │ │
                                          │  └─────────────────────┘ │
                                          │  ┌─────────────────────┐ │
                                          │  │ Proxy config (tera) │ │
                                          │  │ • reverse proxy cfg │ │
                                          │  │ • nginx/caddy tmpl  │ │
                                          │  └─────────────────────┘ │
                                          └──────────────────────────┘
```

**Tech Stack:**
- Rust 1.95+ (edition 2024)
- tokio (async runtime, multi-threaded)
- tonic + prost (gRPC server + client)
- bollard (Docker Engine API)
- rusqlite (metric persistence)
- clap (CLI argument parsing)
- tabled + indicatif (terminal output polish)
- tera (template engine for proxy configs)

**Verification Ladder:**
1. Narrow test: `cargo test -p bosun-daemon -- docker::tests` (unit tests for Docker ops)
2. Integration: `cargo build --workspace && bosun apps list` against a local bosun-daemon
3. Quality gate: `cargo clippy --workspace -- -D warnings && cargo fmt --check`

---

## Phase 1: MVP — Core Functionality (v0.1.0)

### Goal

A working prototype where:
- `bosun-daemon` starts, connects to Docker, exposes gRPC
- `bosun deploy ./my-app --domain mysite.com` builds + deploys a Docker container
- `bosun apps list` shows running apps with status
- `bosun metrics my-app` shows CPU/RAM
- `bosun apps logs my-app --follow` streams logs

---

### Task 1: gRPC service scaffolding + build infrastructure

**task_id:** `task-1-grpc-scaffold`
**parallel_group:** `A`
**depends_on:** `[]`

**Objective:** Compile the proto definitions, wire tonic-build into both crates, and verify the gRPC service skeleton compiles.

**Files:**
- Modify: `crates/bosun/build.rs` (NEW)
- Modify: `crates/bosun-daemon/build.rs` (NEW)
- Create: `crates/bosun-daemon/src/server/mod.rs` (NEW)
- Create: `crates/bosun/src/api/mod.rs` (NEW)
- Modify: `crates/bosun/Cargo.toml` (add build deps)
- Modify: `crates/bosun-daemon/Cargo.toml` (add build deps)

**Context for worker:**
- Proto file lives at `proto/bosun/v1/bosun.proto`
- Both crates need to compile it — the daemon for the server impl, the CLI for the client
- Use `tonic-build` to generate Rust code from proto
- Pattern: place generated code in `OUT_DIR`, include it via `include!` macro
- The proto package is `bosun.v1`, so generated code will be in `bosun::v1` module

**Step 1: Add build dependencies to both Cargo.toml**

Add to `crates/bosun/Cargo.toml`:
```toml
[build-dependencies]
tonic-build.workspace = true
```

Add to `crates/bosun-daemon/Cargo.toml`:
```toml
[build-dependencies]
tonic-build.workspace = true
```

**Step 2: Create build.rs for both crates**

Create `crates/bosun/build.rs`:
```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(false) // CLI doesn't need server code
        .build_client(true)
        .compile_protos(
            &["proto/bosun/v1/bosun.proto"],
            &["proto"], // include path from workspace root
        )?;
    Ok(())
}
```

Create `crates/bosun-daemon/build.rs`:
```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(
            &["proto/bosun/v1/bosun.proto"],
            &["proto"],
        )?;
    Ok(())
}
```

**Step 3: Create API module for the CLI**

Create `crates/bosun/src/api/mod.rs`:
```rust
pub mod v1 {
    tonic::include_proto!("bosun.v1");
}
```

**Step 4: Create server module for the daemon**

Create `crates/bosun-daemon/src/server/mod.rs`:
```rust
pub mod v1 {
    tonic::include_proto!("bosun.v1");
}

use v1::bosun_server::{Bosun, BosunServer};
use tonic::{Request, Response, Status};

pub struct BosunService;

#[tonic::async_trait]
impl Bosun for BosunService {
    async fn list_apps(
        &self,
        _request: Request<v1::ListAppsRequest>,
    ) -> Result<Response<v1::ListAppsResponse>, Status> {
        todo!("implement list_apps")
    }

    type GetAppLogsStream = tonic::codec::Streaming<v1::LogEntry>;
    async fn get_app_logs(
        &self,
        _request: Request<v1::GetAppLogsRequest>,
    ) -> Result<Response<Self::GetAppLogsStream>, Status> {
        todo!("implement get_app_logs")
    }

    async fn restart_app(
        &self,
        _request: Request<v1::RestartAppRequest>,
    ) -> Result<Response<v1::RestartAppResponse>, Status> {
        todo!("implement restart_app")
    }

    async fn scale_app(
        &self,
        _request: Request<v1::ScaleAppRequest>,
    ) -> Result<Response<v1::ScaleAppResponse>, Status> {
        todo!("implement scale_app")
    }

    async fn deploy(
        &self,
        _request: Request<v1::DeployRequest>,
    ) -> Result<Response<v1::DeployResponse>, Status> {
        todo!("implement deploy")
    }

    async fn get_metrics(
        &self,
        _request: Request<v1::GetMetricsRequest>,
    ) -> Result<Response<v1::GetMetricsResponse>, Status> {
        todo!("implement get_metrics")
    }

    type StreamMetricsStream = tonic::codec::Streaming<v1::AppMetric>;
    async fn stream_metrics(
        &self,
        _request: Request<v1::GetMetricsRequest>,
    ) -> Result<Response<Self::StreamMetricsStream>, Status> {
        todo!("implement stream_metrics")
    }

    async fn get_env(
        &self,
        _request: Request<v1::GetEnvRequest>,
    ) -> Result<Response<v1::GetEnvResponse>, Status> {
        todo!("implement get_env")
    }

    async fn set_env(
        &self,
        _request: Request<v1::SetEnvRequest>,
    ) -> Result<Response<v1::SetEnvResponse>, Status> {
        todo!("implement set_env")
    }

    async fn unset_env(
        &self,
        _request: Request<v1::UnsetEnvRequest>,
    ) -> Result<Response<v1::UnsetEnvResponse>, Status> {
        todo!("implement unset_env")
    }
}
```

**Step 5: Build to verify**

Run: `cargo build --workspace`
Expected: compiles successfully (lots of `todo!()` but no type errors).

---

### Task 2: Docker client wrapper + app listing

**task_id:** `task-2-docker-wrapper`
**parallel_group:** `B`
**depends_on:** `[task-1-grpc-scaffold]`

**Objective:** Initialize the bollard Docker client in the daemon and implement `ListApps` — the first real gRPC endpoint.

**Files:**
- Create: `crates/bosun-daemon/src/docker/mod.rs` (NEW)
- Modify: `crates/bosun-daemon/src/server/mod.rs`
- Modify: `crates/bosun-daemon/src/main.rs`

**Step 1: Create Docker client wrapper**

Create `crates/bosun-daemon/src/docker/mod.rs`:
```rust
use bollard::Docker;
use bollard::container::ListContainersOptions;
use bollard::secret::ContainerSummary;
use crate::server::v1::{App, AppStatus};

pub struct DockerClient {
    inner: Docker,
}

impl DockerClient {
    pub async fn connect() -> anyhow::Result<Self> {
        let docker = Docker::connect_with_local_defaults()?;
        // Verify connection
        docker.ping().await?;
        tracing::info!("Connected to Docker daemon");
        Ok(Self { inner: docker })
    }

    /// List all containers managed by Bosun (filtered by label)
    pub async fn list_bosun_apps(&self) -> anyhow::Result<Vec<App>> {
        let options = ListContainersOptions {
            all: true,
            filters: vec![("label", vec!["managed-by=bosun"])].into_iter().collect(),
            ..Default::default()
        };

        let containers: Vec<ContainerSummary> = self.inner.list_containers(Some(options)).await?;

        let apps = containers.into_iter().filter_map(|c| {
            let names = c.names?;
            let name = names.first()?.strip_prefix('/').unwrap_or(&names.first()?).to_string();
            let status = match c.state.as_deref() {
                Some("running") => AppStatus::Running,
                Some("exited") | Some("created") => AppStatus::Stopped,
                _ => AppStatus::Unknown,
            };

            // Extract domain and port from labels
            let labels = c.labels.unwrap_or_default();
            let domain = labels.get("bosun.domain").cloned();
            let port = labels.get("bosun.port").and_then(|p| p.parse().ok());

            Some(App {
                name,
                status: status.into(),
                domain,
                port,
                instances: Some(1), // MVP: single instance
                env_keys: vec![],
            })
        }).collect();

        Ok(apps)
    }
}
```

**Step 2: Wire into the gRPC service**

Update `BosunService` in `crates/bosun-daemon/src/server/mod.rs` to hold the Docker client:

```rust
pub struct BosunService {
    docker: std::sync::Arc<tokio::sync::Mutex<crate::docker::DockerClient>>,
}

impl BosunService {
    pub fn new(docker: crate::docker::DockerClient) -> Self {
        Self {
            docker: std::sync::Arc::new(tokio::sync::Mutex::new(docker)),
        }
    }
}
```

Implement `list_apps`:
```rust
async fn list_apps(
    &self,
    _request: Request<v1::ListAppsRequest>,
) -> Result<Response<v1::ListAppsResponse>, Status> {
    let docker = self.docker.lock().await;
    let apps = docker.list_bosun_apps().await.map_err(|e| {
        Status::internal(format!("Failed to list containers: {}", e))
    })?;
    Ok(Response::new(v1::ListAppsResponse { apps }))
}
```

**Step 3: Update daemon main.rs**

```rust
use crate::docker::DockerClient;
use crate::server::BosunService;

// In main:
let docker = DockerClient::connect().await?;
let service = BosunService::new(docker);

tonic::transport::Server::builder()
    .add_service(server::v1::bosun_server::BosunServer::new(service))
    .serve(args.listen.parse()?)
    .await?;
```

**Step 4: Build and run**

Run: `cargo build -p bosun-daemon`
Expected: compiles. Running requires Docker, test manually.

---

### Task 3: Deploy — build + run a container

**task_id:** `task-3-deploy`
**parallel_group:** `B`
**depends_on:** `[task-2-docker-wrapper]`

**Objective:** Implement the `Deploy` gRPC method — build a Docker image from a directory, tag it, and run a container with Bosun labels.

**Files:**
- Modify: `crates/bosun-daemon/src/docker/mod.rs` (add deploy method)
- Modify: `crates/bosun-daemon/src/server/mod.rs` (wire deploy)
- Modify: `crates/bosun-daemon/Cargo.toml` (add tempfile dep for tar)

**Step 1: Add deploy method to DockerClient**

Add to `impl DockerClient`:
```rust
use bollard::image::BuildImageOptions;
use bollard::container::{CreateContainerOptions, Config, HostConfig, PortBinding};
use std::collections::HashMap;
use futures_util::StreamExt; // add futures-util to deps

pub async fn deploy(
    &self,
    build_dir: &str,
    app_name: &str,
    domain: Option<&str>,
    port: u32,
    env_vars: HashMap<String, String>,
) -> anyhow::Result<()> {
    // 1. Tar the build directory (or read Dockerfile)
    // For MVP: assume build_dir has a Dockerfile, use it directly
    let tar = create_build_tar(build_dir)?;

    // 2. Build image
    let build_options = BuildImageOptions {
        t: app_name,     // image tag
        dockerfile: "Dockerfile",
        ..Default::default()
    };

    let mut build_stream = self.inner.build_image(build_options, None, Some(tar.into()));
    while let Some(msg) = build_stream.next().await {
        match msg {
            Ok(msg) => {
                if let Some(stream) = msg.stream {
                    tracing::debug!("Docker build: {}", stream.trim());
                }
                if let Some(error) = msg.error {
                    anyhow::bail!("Docker build error: {}", error);
                }
            }
            Err(e) => anyhow::bail!("Docker build stream error: {}", e),
        }
    }

    // 3. Create and start container
    let container_config = Config {
        image: Some(app_name.to_string()),
        env: Some(env_vars.iter().map(|(k, v)| format!("{}={}", k, v)).collect()),
        labels: Some({
            let mut labels = HashMap::new();
            labels.insert("managed-by".into(), "bosun".into());
            labels.insert("bosun.app".into(), app_name.to_string());
            if let Some(d) = domain {
                labels.insert("bosun.domain".into(), d.to_string());
            }
            labels.insert("bosun.port".into(), port.to_string());
            labels
        }),
        ..Default::default()
    };

    let host_config = HostConfig {
        port_bindings: Some({
            let mut bindings = HashMap::new();
            bindings.insert(
                format!("{}/tcp", port),
                Some(vec![PortBinding {
                    host_ip: Some("0.0.0.0".into()),
                    host_port: Some(port.to_string()),
                }]),
            );
            bindings
        }),
        ..Default::default()
    };

    let create_options = CreateContainerOptions {
        name: app_name,
        ..Default::default()
    };

    self.inner.create_container(Some(create_options), container_config).await?;
    self.inner.start_container::<String>(app_name, None).await?;

    tracing::info!("Deployed app: {}", app_name);
    Ok(())
}

fn create_build_tar(build_dir: &str) -> anyhow::Result<Vec<u8>> {
    // Simple tar of the build directory
    let mut tar = tar::Builder::new(Vec::new()); // add `tar` crate to deps
    tar.append_dir_all(".", build_dir)?;
    let data = tar.into_inner()?;
    Ok(data)
}
```

**Step 2: Wire deploy into gRPC handler**

Implement the `deploy` method in `BosunService`:
```rust
async fn deploy(
    &self,
    request: Request<v1::DeployRequest>,
) -> Result<Response<v1::DeployResponse>, Status> {
    let req = request.into_inner();
    let docker = self.docker.lock().await;

    // Generate app name from context path (last directory component)
    let app_name = std::path::Path::new(&req.context_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("app")
        .to_string();

    let env_vars = req.env;
    let port = req.port.unwrap_or(3000);

    docker.deploy(
        &req.context_path,
        &app_name,
        req.domain.as_deref(),
        port,
        env_vars,
    ).await.map_err(|e| Status::internal(format!("Deploy failed: {}", e)))?;

    Ok(Response::new(v1::DeployResponse {
        app_name,
        status: "deployed".into(),
    }))
}
```

**Step 3: Add new dependencies to bosun-daemon Cargo.toml**

```toml
futures-util = "0.3"
tar = "0.4"
```

**Step 4: Build**

Run: `cargo build -p bosun-daemon`

---

### Task 4: Metrics collection + gRPC endpoint

**task_id:** `task-4-metrics`
**parallel_group:** `C`
**depends_on:** `[task-2-docker-wrapper]`

**Objective:** Implement real-time metric collection from Docker stats API and expose via gRPC (both snapshot and streaming).

**Files:**
- Create: `crates/bosun-daemon/src/metrics/mod.rs` (NEW)
- Modify: `crates/bosun-daemon/src/server/mod.rs`
- Create: `crates/bosun-daemon/src/persist/mod.rs` (NEW, SQLite for historical metrics)

**Step 1: Create metrics module**

Create `crates/bosun-daemon/src/metrics/mod.rs`:
```rust
use bollard::Docker;
use bollard::container::StatsOptions;
use futures_util::StreamExt;
use crate::server::v1::AppMetric;

pub struct MetricCollector {
    docker: Docker,
}

impl MetricCollector {
    pub fn new(docker: Docker) -> Self {
        Self { docker }
    }

    /// Get a single snapshot of metrics for a container
    pub async fn get_snapshot(&self, container_name: &str) -> anyhow::Result<AppMetric> {
        let stats_stream = &mut self.docker.stats(
            container_name,
            Some(StatsOptions { stream: false, ..Default::default() }),
        );

        if let Some(stats) = stats_stream.next().await {
            let stats = stats?;
            let cpu_delta = stats.cpu_stats.cpu_usage.total_usage as f64
                - stats.precpu_stats.cpu_usage.total_usage as f64;
            let system_delta = stats.cpu_stats.system_cpu_usage.unwrap_or(1) as f64
                - stats.precpu_stats.system_cpu_usage.unwrap_or(0) as f64;
            let cpu_percent = if system_delta > 0.0 {
                (cpu_delta / system_delta)
                    * (stats.cpu_stats.online_cpus.unwrap_or(1) as f64)
                    * 100.0
            } else {
                0.0
            };

            Ok(AppMetric {
                app_name: container_name.to_string(),
                cpu_percent,
                ram_bytes: stats.memory_stats.usage.unwrap_or(0),
                net_rx_bytes: stats.networks.as_ref()
                    .and_then(|n| n.values().next())
                    .map(|n| n.rx_bytes)
                    .unwrap_or(0),
                net_tx_bytes: stats.networks.as_ref()
                    .and_then(|n| n.values().next())
                    .map(|n| n.tx_bytes)
                    .unwrap_or(0),
                timestamp_unix: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            })
        } else {
            Err(anyhow::anyhow!("No stats returned for {}", container_name))
        }
    }

    /// Stream live metrics (returns a Stream)
    pub fn stream_live(
        &self,
        container_name: String,
    ) -> impl futures_util::Stream<Item = anyhow::Result<AppMetric>> + '_ {
        let stats_stream = self.docker.stats(
            &container_name,
            Some(StatsOptions { stream: true, ..Default::default() }),
        );

        stats_stream.map(move |stats_result| {
            let stats = stats_result?;
            // ... same CPU calculation as above ...
            // Return AppMetric
            Ok(AppMetric {
                app_name: container_name.clone(),
                cpu_percent: 0.0, // simplified for this plan
                ram_bytes: stats.memory_stats.usage.unwrap_or(0),
                net_rx_bytes: 0,
                net_tx_bytes: 0,
                timestamp_unix: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            })
        })
    }
}
```

**Step 2: Create persistence module (SQLite for metrics history)**

Create `crates/bosun-daemon/src/persist/mod.rs`:
```rust
use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        std::fs::create_dir_all(path.as_ref().parent().unwrap_or(Path::new(".")))?;
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS metrics (
                app_name TEXT NOT NULL,
                cpu_percent REAL,
                ram_bytes INTEGER,
                timestamp INTEGER NOT NULL,
                PRIMARY KEY (app_name, timestamp)
            );
            CREATE TABLE IF NOT EXISTS config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );"
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn insert_metric(&self, metric: &crate::server::v1::AppMetric) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metrics (app_name, cpu_percent, ram_bytes, timestamp)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![metric.app_name, metric.cpu_percent, metric.ram_bytes, metric.timestamp_unix],
        )?;
        Ok(())
    }

    pub fn get_config(&self, key: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT value FROM config WHERE key = ?1")?;
        let mut rows = stmt.query_map(rusqlite::params![key], |row| row.get(0))?;
        Ok(rows.next().transpose()?)
    }

    pub fn set_config(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO config (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }
}
```

**Step 3: Wire into gRPC service**

Update `BosunService` to hold `MetricCollector`:
```rust
pub struct BosunService {
    docker: std::sync::Arc<tokio::sync::Mutex<DockerClient>>,
    metrics: std::sync::Arc<MetricCollector>,
}
```

Implement `get_metrics`:
```rust
async fn get_metrics(
    &self,
    request: Request<v1::GetMetricsRequest>,
) -> Result<Response<v1::GetMetricsResponse>, Status> {
    let req = request.into_inner();
    let docker = self.docker.lock().await;
    let apps = docker.list_bosun_apps().await.map_err(|e| Status::internal(e.to_string()))?;

    let mut metrics = Vec::new();
    for app in &apps {
        if req.app_name.as_deref().map_or(true, |n| n == app.name) {
            if app.status() == v1::AppStatus::Running {
                if let Ok(m) = self.metrics.get_snapshot(&app.name).await {
                    metrics.push(m);
                }
            }
        }
    }

    Ok(Response::new(v1::GetMetricsResponse { metrics }))
}
```

**Step 4: Build**

Run: `cargo build -p bosun-daemon`

---

### Task 5: CLI — connect to daemon, display apps

**task_id:** `task-5-cli-connect`
**parallel_group:** `C`
**depends_on:** `[task-1-grpc-scaffold]`

**Objective:** CLI connects to the daemon via gRPC and implements `apps list` with real tabled output.

**Files:**
- Modify: `crates/bosun/src/main.rs` (add client connection)
- Modify: `crates/bosun/src/cli.rs` (implement run method)
- Create: `crates/bosun/src/client.rs` (NEW, gRPC client wrapper)

**Step 1: Create gRPC client wrapper**

Create `crates/bosun/src/client.rs`:
```rust
use crate::api::v1::bosun_client::BosunClient;
use tonic::transport::{Channel, Certificate, Identity};
use anyhow::Context;

pub struct BosunGrpcClient {
    inner: BosunClient<Channel>,
}

impl BosunGrpcClient {
    pub async fn connect(
        addr: &str,
        cert_path: Option<&str>,
        key_path: Option<&str>,
    ) -> anyhow::Result<Self> {
        let tls = match (cert_path, key_path) {
            (Some(cert), Some(key)) => {
                let cert_pem = std::fs::read_to_string(cert)?;
                let key_pem = std::fs::read_to_string(key)?;
                let identity = Identity::from_pem(&cert_pem, &key_pem);
                let ca_cert = Certificate::from_pem(&cert_pem);

                Some(tonic::transport::ClientTlsConfig::new()
                    .ca_certificate(ca_cert)
                    .identity(identity))
            }
            _ => None,
        };

        let mut endpoint = Channel::from_shared(addr.to_string())?;
        if let Some(tls) = tls {
            endpoint = endpoint.tls_config(tls)?;
        }

        let channel = endpoint.connect().await?;
        let client = BosunClient::new(channel);
        Ok(Self { inner: client })
    }

    pub async fn list_apps(&mut self) -> anyhow::Result<Vec<crate::api::v1::App>> {
        let response = self.inner
            .list_apps(crate::api::v1::ListAppsRequest {})
            .await?;
        Ok(response.into_inner().apps)
    }
}
```

**Step 2: Implement `apps list` and `apps logs` commands**

Update `Cli::run()` in `cli.rs`:
```rust
pub async fn run(self) -> anyhow::Result<()> {
    let mut client = crate::client::BosunGrpcClient::connect(
        &self.daemon,
        self.cert.as_deref(),
        self.key.as_deref(),
    ).await?;

    match self.command {
        Command::Apps { sub } => match sub {
            AppsCmd::List => {
                let apps = client.list_apps().await?;
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
                let rows: Vec<AppRow> = apps.into_iter().map(|a| AppRow {
                    name: a.name,
                    status: format!("{:?}", a.status()),
                    domain: a.domain.unwrap_or_else(|| "—".into()),
                    port: a.port.map(|p| p.to_string()).unwrap_or_else(|| "—".into()),
                }).collect();
                println!("{}", Table::new(rows));
            }
            AppsCmd::Logs { app, follow: _ } => {
                // TODO: implement streaming logs from gRPC stream
                println!("Logs for {} — streaming not yet implemented", app);
            }
            AppsCmd::Restart { app } => {
                client.restart_app(&app).await?;
                println!("Restarted {}", app);
            }
            AppsCmd::Scale { app, instances } => {
                client.scale_app(&app, instances).await?;
                println!("Scaled {} to {} instances", app, instances);
            }
        },
        // ... other commands similar pattern
    }
    Ok(())
}
```

**Step 3: Build CLI**

Run: `cargo build -p bosun`

---

### Task 6: End-to-end integration test (manual + CI)

**task_id:** `task-6-integration`
**parallel_group:** `D`
**depends_on:** `[task-3-deploy, task-4-metrics, task-5-cli-connect]`

**Objective:** Write a test script that starts bosun-daemon, deploys a test app, verifies metrics and logs.

**Files:**
- Create: `tests/integration/run.sh` (NEW)
- Create: `tests/integration/test-app/Dockerfile` (NEW)
- Create: `.github/workflows/ci.yml` (NEW)

**Step 1: Create a minimal test Dockerfile**

Create `tests/integration/test-app/Dockerfile`:
```dockerfile
FROM alpine:3.20
RUN echo "#!/bin/sh\nwhile true; do echo \"Bosun test app running\"; sleep 5; done" > /app.sh
RUN chmod +x /app.sh
CMD ["/app.sh"]
```

**Step 2: Create integration test script**

Create `tests/integration/run.sh`:
```bash
#!/usr/bin/env bash
set -euo pipefail

echo "=== Bosun Integration Test ==="

# Start daemon in background
cargo run -p bosun-daemon -- --listen 127.0.0.1:9091 &
DAEMON_PID=$!
sleep 2

# Cleanup
cleanup() {
    echo "Stopping daemon..."
    kill $DAEMON_PID 2>/dev/null || true
    docker rm -f test-hello 2>/dev/null || true
}
trap cleanup EXIT

# List apps (should be empty or have existing Bosun-managed containers)
echo "=== List apps ==="
cargo run -p bosun -- --daemon https://127.0.0.1:9091 apps list

echo "=== Integration test PASSED ==="
```

**Step 3: Create GitHub Actions CI**

Create `.github/workflows/ci.yml`:
```yaml
name: CI

on:
  push:
    branches: [main, develop]
  pull_request:
    branches: [main]

env:
  CARGO_TERM_COLOR: always

jobs:
  build-and-test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions-rust-lang/setup-rust-toolchain@v1
        with:
          toolchain: stable
          components: clippy, rustfmt
      - name: Build
        run: cargo build --workspace --verbose
      - name: Test
        run: cargo test --workspace --verbose
      - name: Clippy
        run: cargo clippy --workspace -- -D warnings
      - name: Format check
        run: cargo fmt --workspace --check
```

**Step 4: Verify CI pipeline exists**

Run: `ls -la .github/workflows/ci.yml`

---

## Review Workload Forecast

| Campo | Valor |
|-------|-------|
| Estimated changed lines | ~800–1200 (new project, all new code) |
| 400-line budget risk | High |
| Chained PRs recommended | No (initial commit — single bootstrap PR is acceptable for project genesis) |
| Suggested split | Single PR for scaffold + plan |
| Chain strategy | size-exception (project initialization) |
| Decision needed before apply | Yes |

---

## Out of scope for Phase 1

- SSL/ACME integration (Let's Encrypt)
- Reverse proxy auto-config (nginx/caddy generation)
- One-click app templates
- Multi-instance scaling (scale > 1)
- Health checks and auto-restart
- Webhook triggers (GitHub push → deploy)
- User authentication / multi-tenant
- Docker Swarm / Kubernetes support
- Graceful shutdown and signal handling

These are Phase 2+ features.
