<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://img.shields.io/badge/bosun-⚓-3BB1DC?style=for-the-badge">
    <img alt="bosun" src="https://img.shields.io/badge/bosun-⚓-3BB1DC?style=for-the-badge">
  </picture>
</p>

<p align="center">
  <strong>Minimal PaaS orchestration. Zero dashboard. Pure terminal.</strong>
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPLv3+-blue.svg" alt="License: GPLv3+"></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/rust-stable%201.95+-orange.svg" alt="Rust: stable 1.95+"></a>
  <a href="#status"><img src="https://img.shields.io/badge/status-alpha-red.svg" alt="Status: Alpha"></a>
</p>

---

## What is Bosun?

Bosun is a PaaS that runs entirely in your terminal. No browser. No React dashboard. No hundreds of megabytes of RAM wasted on a UI you look at twice a month. Just a tiny Rust daemon on your server and a single CLI binary on your machine.

You get **deployments, metrics, log streaming, SSL certificates, and reverse proxy configuration** — everything CapRover or Dokku give you — at **less than 15 MB of RAM** for the daemon.

> Think of it as `htop` for your PaaS. Or Dokku rewritten in Rust with real observability.

```
$ bosun deploy ./my-api --domain api.acme.com --ssl
Building… ━━━━━━━━━━━━ 100%   Deploying api… ✓   SSL enabled ✓

$ bosun apps list
┌──────────┬──────────┬────────┬─────────────┬──────────┐
│ APP      │ STATUS   │ CPU    │ RAM         │ UPTIME   │
├──────────┼──────────┼────────┼─────────────┼──────────┤
│ api      │ running  │ 2.1%   │ 48 MB       │ 14d 3h   │
│ frontend │ running  │ 0.8%   │ 32 MB       │ 14d 3h   │
│ worker   │ running  │ 5.4%   │ 128 MB      │ 2h 15m   │
│ redis    │ stopped  │ —      │ —           │ —        │
└──────────┴──────────┴────────┴─────────────┴──────────┘

$ bosun metrics api --live
api  cpu: ████░░░░░░ 38%   ram: ██████░░░░ 62%   req/s: 142

$ bosun apps logs worker --follow
2026-06-28T10:23:14  job-1428 completed in 32ms
2026-06-28T10:23:15  job-1429 completed in 28ms

$ bosun apps templates
┌───────────┬──────────────────────────────────────┬───────────┬──────┐
│ NAME      │ DESCRIPTION                          │ CATEGORY  │ PORT │
├───────────┼──────────────────────────────────────┼───────────┼──────┤
│ redis     │ Redis 7 (Alpine) — in-memory ...     │ cache     │ 6379 │
│ postgres  │ PostgreSQL 16 (Alpine) — relati...   │ database  │ 5432 │
│ mysql     │ MySQL 8.4 — relational database      │ database  │ 3306 │
│ mongo     │ MongoDB 7 — document database        │ database  │27017 │
│ nginx     │ Nginx (Alpine) — web server / ...    │ proxy     │ 80   │
└───────────┴──────────────────────────────────────┴───────────┴──────┘

$ bosun apps create redis
🚀 Creating 'redis' from template 'redis' (port: 6379)...
✔ Created 'redis' from template 'redis' successfully (status: deployed)
```

---

## Table of Contents

- [Features](#features)
- [Quick Start](#quick-start)
- [Architecture](#architecture)
- [Why Bosun?](#why-bosun)
- [Roadmap](#roadmap)
- [Contributing](#contributing)
- [License](#license)

---

## Features

### Implemented

- [x] gRPC API between CLI and daemon (protobuf-defined, streaming support)
- [x] Docker container discovery by label (`managed-by=bosun`)
- [x] GPLv3+ licensed — free software, always

### In Progress (MVP)

- [ ] `bosun deploy` — build and run containers from a Dockerfile or compose file
- [ ] `bosun apps list|logs|restart|scale` — full application lifecycle
- [x] `bosun apps create <template>` and `bosun apps templates` — deploy one-click apps (Redis, Postgres, MySQL, MongoDB, Nginx)
- [ ] `bosun metrics` — real-time CPU, RAM, network per container
- [ ] `bosun env` — environment variable management
- [ ] `bosun config` — daemon configuration from the CLI
- [x] `bosun deploy --ssl` — automatic Let's Encrypt HTTPS via Caddy (requires Caddy installed)
- [ ] CI pipeline (GitHub Actions: build, test, clippy, fmt)

### Planned

- [ ] Reverse proxy auto-configuration (Caddy integration)
- [ ] Health checks and auto-restart
- [ ] Webhook triggers (deploy on `git push`)
- [ ] Docker Swarm multi-node support
- [ ] Persistent volume management

---

## Quick Start

> ⚠️ Bosun is in early development. These instructions show the intended workflow — not everything works yet.

### Prerequisites

- **Server:** Linux or macOS with [Docker Engine](https://docs.docker.com/engine/install/) 20.10+
- **Client:** macOS or Linux with the `bosun` binary
- **Network:** The daemon port (default `9090`) accessible from your client
- **Caddy (optional):** The install script installs [Caddy](https://caddyserver.com/) automatically for reverse proxy and Let's Encrypt SSL

### Install (one command)

The fastest way to get a fresh Ubuntu/Debian VPS running Bosun:

```bash
curl -fsSL https://raw.githubusercontent.com/rquezada-tech/bosun/main/scripts/install.sh | sudo bash
```

This single command:
- Installs Docker Engine (if not present)
- Installs Caddy reverse proxy (port 80/443, automatic Let's Encrypt)
- Installs the Rust toolchain
- Clones and builds `bosun-daemon` from source
- Creates `/etc/bosun/` with a self-signed TLS certificate
- Sets up a `systemd` service (auto-start, auto-restart)
- Opens port `9090/tcp` in `ufw`

After it finishes, install the CLI on your **local machine**:

```bash
cargo install --git https://github.com/rquezada-tech/bosun.git bosun
```

> **Note:** The install script is idempotent — safe to re-run. It skips steps that are already done.

### Install from source (for developers)

```bash
git clone https://github.com/rquezada-tech/bosun.git
cd bosun

# Build everything
cargo build --workspace

# Run the daemon directly
cargo run --bin bosun-daemon -- --listen 0.0.0.0:9090 --data-dir /var/lib/bosun
```

Pre-built binaries will be available once we hit v0.1.0.

### Start the daemon

On your server:

```bash
# If installed via the one-command script:
systemctl start bosun-daemon

# Or run directly:
bosun-daemon --listen 0.0.0.0:9090 --data-dir /var/lib/bosun --cert /etc/bosun/server.crt --key /etc/bosun/server.key
```

### Connect and deploy

On your local machine:

```bash
export BOSUN_DAEMON=https://my-server:9090

# Deploy your first app
bosun deploy ./my-node-app --domain api.my-site.com

# Deploy with automatic HTTPS (requires Caddy)
bosun deploy ./my-node-app --domain api.my-site.com --ssl

# Check it's running
bosun apps list

# Deploy a one-click app (Redis, Postgres, MySQL, MongoDB, Nginx)
bosun apps templates              # see available templates
bosun apps create redis           # spin up Redis in seconds

# Watch live metrics
bosun metrics my-node-app --live
```

### Security

For production, always use TLS:

```bash
bosun-daemon --listen 0.0.0.0:9090 --cert /etc/bosun/server.crt --key /etc/bosun/server.key
bosun --cert ~/.bosun/client.crt --key ~/.bosun/client.key apps list
```

---

## Architecture

```
┌──────────────────────┐         gRPC + TLS          ┌────────────────────────────┐
│      bosun (CLI)     │ ◄─────────────────────────► │   bosun-daemon (server)    │
│      Rust binary     │                             │   Rust binary              │
│      ~8 MB           │                             │   ~10 MB / ~15 MB RAM      │
│                      │                             │                            │
│  • clap argument     │                             │  ┌──────────────────────┐  │
│    parsing           │                             │  │ Docker (bollard)     │  │
│  • tabled output     │                             │  │ • build & run        │  │
│  • indicatif bars    │                             │  │ • stats & logs       │  │
│  • tonic gRPC client │                             │  │ • volumes & networks │  │
│                      │                             │  └──────────────────────┘  │
│  Zero runtime deps   │                             │  ┌──────────────────────┐  │
│  beyond glibc +      │                             │  │ Metrics (rusqlite)   │  │
│  system TLS          │                             │  │ • time-series store  │  │
│                      │                             │  │ • per-app queries    │  │
│                      │                             │  └──────────────────────┘  │
│                      │                             │  ┌──────────────────────┐  │
│                      │                             │  │ Proxy config (tera)  │  │
│                      │                             │  │ • Caddy config gen   │  │
│                      │                             │  │ • hot-reload         │  │
│                      │                             │  └──────────────────────┘  │
└──────────────────────┘                             └───────────┬────────────────┘
                                                                 │
                                                    ┌────────────▼────────────────┐
                                                    │   Caddy (reverse proxy)    │
                                                    │   • automatic TLS (LE)     │
                                                    │   • HTTP → HTTPS redirect  │
                                                    │   • route → Docker apps    │
                                                    └────────────────────────────┘
```

### Design decisions

| Decision | Rationale |
|---|---|
| **gRPC instead of REST** | Streaming logs and live metrics natively. Strongly typed contracts via protobuf. Smaller wire format than JSON. |
| **SQLite for persistence** | Zero setup. No separate DB process. Embedded, fast, reliable. Perfect for single-node PaaS. |
| **bollard for Docker** | Mature Rust Docker client. Talks directly to `/var/run/docker.sock` — no daemon, no socket proxy. |
| **CLI-only, no web UI** | The browser is the heaviest part of any PaaS. A CLI is scriptable, pipeable, automatable. Less code, fewer bugs, lower attack surface. |
| **Single binary per side** | No runtime dependencies. Copy `bosun-daemon` to your server and run it. That's it. |

### Resource usage (estimated vs CapRover)

| Resource | CapRover (typical) | Bosun (estimated) | Reduction |
|---|---|---|---|
| Daemon RAM | 300–500 MB | **15–30 MB** | ~95% |
| Disk (deps) | ~400 MB | **~15 MB** | ~96% |
| CPU idle | 1–3% | **<0.1%** | ~97% |
| Web dashboard RAM | 80–150 MB | **0 MB** | 100% |
| External runtimes | Node, MongoDB | **None** (only Docker) | All eliminated |

---

## Why Bosun?

### The problem with current PaaS tools

- **CapRover** is great, but it needs Node.js, MongoDB, and a React dashboard. On a 1 GB VPS, CapRover alone eats 30–50% of your RAM before you deploy a single app.
- **Dokku** is minimal (Bash), but Bash at 12k+ lines is fragile. No real metrics. No streaming logs over the network. Hard to extend.
- **Coolify** requires PHP and Next.js. Powerful but heavy.
- **Kamal** is CLI-first, but tied to Ruby and Rails conventions.

### Bosun's answer

> A PaaS should use fewer resources than the apps it hosts.

If your API needs 100 MB of RAM and your PaaS needs 500 MB, the PaaS is the expensive part. Bosun flips this: **the daemon uses less RAM than a single idle Node.js process.**

For the price of a $5/month VPS, you get:
- Automated Docker deployments
- Real-time metrics
- Streaming logs
- SSL certificates (automatic via Caddy + Let's Encrypt)
- Reverse proxy routing (Caddy)

No Kubernetes. No cloud vendor lock-in. No dashboard you don't need.

---

## Roadmap

| Version | Scope | Target |
|---|---|---|
| **v0.1.0** | MVP: deploy, apps list/logs, metrics, env vars | Q3 2026 |
| **v0.2.0** | SSL (Let's Encrypt), reverse proxy auto-config | Q4 2026 |
| **v0.3.0** | One-click apps, webhook triggers, health checks | Q1 2027 |
| **v0.4.0** | Docker Swarm support, multi-node | Q2 2027 |
| **v1.0.0** | Stable API, backwards compatibility guarantees | TBD |

Roadmap details and task-level breakdowns live in [`docs/plans/`](docs/plans/).

---

## Contributing

Bosun is a community project. We welcome contributions of all kinds — code, documentation, bug reports, feature ideas, and feedback from real-world deployments.

### Getting started

1. **Read the architecture plan:** [`docs/plans/2026-06-28-mvp-architecture.md`](docs/plans/2026-06-28-mvp-architecture.md) — it explains the project structure, design decisions, and what each module does.
2. **Set up your environment:**

   ```bash
   git clone https://github.com/rquezada-tech/bosun.git
   cd bosun
   cargo build --workspace          # compiles both crates
   cargo test --workspace           # runs all tests
   cargo clippy --workspace -- -D warnings  # lint
   ```

3. **Pick an issue:** Check the GitHub Issues for tasks tagged `good first issue` or `help wanted`. The [MVP plan](docs/plans/2026-06-28-mvp-architecture.md) has bite-sized tasks with exact file paths and code examples.

### Pull request workflow

1. **Fork** the repository
2. **Create a branch:** `git checkout -b feat/my-feature` (use `feat/`, `fix/`, `docs/`, or `chore/` prefix)
3. **Write tests first** — we practice TDD for all feature work
4. **Keep PRs small:** ideally under 400 lines changed. If your feature is larger, break it into stacked PRs.
5. **Run the quality gates before pushing:**

   ```bash
   cargo fmt --all -- --check
   cargo clippy --workspace -- -D warnings
   cargo test --workspace
   ```

6. **Open a PR** against the `develop` branch with a clear description of what it does and why.
7. **Wait for review.** At least one maintainer must approve before merge.

### Where to get help

- **GitHub Issues:** Bug reports, feature requests, questions
- **Discussions:** Coming soon — architectural RFCs and community support

### Code of Conduct

We follow the [Contributor Covenant Code of Conduct](https://www.contributor-covenant.org/version/2/1/code_of_conduct/). Be kind. Be constructive. Assume good faith.

### Contributors

<!-- ALL-CONTRIBUTORS-LIST:START - Do not remove or modify this section -->
<!-- ALL-CONTRIBUTORS-LIST:END -->

This project follows the [all-contributors](https://github.com/all-contributors/all-contributors) specification. Contributions of any kind are welcome.

---

## License

Bosun is free software: you can redistribute it and/or modify it under the terms of the **GNU General Public License** as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version.

See [LICENSE](LICENSE) for the full text.

> **Why GPLv3+?** Bosun replaces proprietary and source-available PaaS tools. Copyleft ensures every improvement — from us or the community — stays free for everyone. If you deploy Bosun in production, your users deserve the same freedoms you got.

---

<p align="center">
  <sub>Built with Rust. Driven by the community. Licensed for freedom.</sub>
</p>
