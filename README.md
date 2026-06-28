# bosun ⚓

**Minimal PaaS orchestration daemon + CLI in Rust.**  
Deploy Docker apps, monitor metrics, and manage SSL — all from your terminal.

```
$ bosun apps list
┌──────────┬──────────┬────────┬─────────────┬──────────┐
│ APP      │ STATUS   │ CPU    │ RAM         │ UPTIME   │
├──────────┼──────────┼────────┼─────────────┼──────────┤
│ api      │ running  │ 2.1%   │ 48 MB       │ 14d 3h   │
│ frontend │ running  │ 0.8%   │ 32 MB       │ 14d 3h   │
│ worker   │ running  │ 5.4%   │ 128 MB      │ 2h 15m   │
│ redis    │ stopped  │ —      │ —           │ —        │
└──────────┴──────────┴────────┴─────────────┴──────────┘

$ bosun deploy ./mi-app --domain api.misitio.com
Building... ━━━━━━━━━━━━ 100%
Deploying api... done ✓
Enabling SSL for api.misitio.com... ✓

$ bosun metrics api --live
api  cpu: ████░░░░░░ 38%   ram: ██████░░░░ 62%   req/s: 142
```

## Architecture

```
Local CLI (bosun) ─── gRPC/TLS ───► Daemon (bosun-daemon)
                                         │
                          ┌──────────────┼──────────────┐
                          │              │              │
                      Docker API     Nginx/Caddy     Metrics DB
                      (bollard)     (config gen)    (SQLite)
```

## Crates

| Crate | Purpose | Binary |
|-------|---------|--------|
| `bosun` | CLI — user-facing terminal commands | `bosun` |
| `bosun-daemon` | Daemon — server-side orchestrator | `bosun-daemon` |

## Why Bosun?

- **~15 MB RAM** for the daemon vs 300–500 MB for CapRover/Node
- **No browser needed** — everything is CLI, scriptable, automatable
- **Single Rust binary** — no runtime dependencies beyond Docker Engine
- **Zero-dashboard philosophy** — `htop` but for your PaaS

## Status

🚧 Under heavy construction. Private alpha. Not ready for use.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
