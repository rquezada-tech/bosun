# Bosun

> 🌐 **English version:** [README.en.md](./README.en.md) — [View this README in English](./README.en.md)

<p align="center">
  <img src="https://raw.githubusercontent.com/rquezada-tech/bosun/main/logo_bosun.png" alt="Bosun Logo" width="340">
</p>

> *Deploy Docker apps, monitor metrics, and manage SSL — all from your terminal. Zero dashboard. Pure CLI.*

<!-- Badges -->
<div align="center">

![Estado](https://img.shields.io/badge/Estado-Alpha-f97316?style=flat-square&labelColor=374151)
![Versión](https://img.shields.io/badge/Versión-0.1.0-2563eb?style=flat-square&labelColor=374151)
![Paradigma](https://img.shields.io/badge/Paradigma-CLI_First-22c55e?style=flat-square&labelColor=374151)
![RAM](https://img.shields.io/badge/RAM-15MB_daemon-22c55e?style=flat-square&labelColor=374151)
![Stack](https://img.shields.io/badge/Stack-Rust_%2B_gRPC_%2B_SQLite-0ea5e9?style=flat-square&labelColor=374151)
![Licencia](https://img.shields.io/badge/Licencia-GPLv3+-2f855a?style=flat-square&labelColor=374151)

</div>

## Qué es

Bosun es un PaaS que corre enteramente en tu terminal. Sin navegador. Sin dashboard React. Sin cientos de megabytes de RAM desperdiciados en una interfaz que mirás dos veces al mes. Solo un daemon Rust diminuto en tu servidor y un solo binario CLI en tu máquina.

**Bosun reemplaza CapRover, Dokku y Coolify con un solo binario Rust de ~15 MB de RAM. Sin Node.js. Sin MongoDB. Sin runtime externo. Solo Docker Engine.**

Está diseñado para:
- **VPS de $5/mes** — el daemon usa menos RAM que un solo proceso idle de Node.js
- **Automatización total** — cada comando es scripteable, pipeable, integrable en CI/CD
- **Zero-touch security** — CrowdSec/Fail2Ban se configuran solos en cada deploy
- **Gobernanza de APIs** — API Gateway integrado (APISIX) con rate-limiting, caching y observabilidad
- **Despliegues sin downtime** — rolling updates y blue-green deploy desde CLI o webhook

```
$ bosun apps list
┌──────────┬──────────┬────────┬─────────────┬──────────┐
│ APP      │ STATUS   │ CPU    │ RAM         │ UPTIME   │
├──────────┼──────────┼────────┼─────────────┼──────────┤
│ api      │ running  │ 2.1%   │ 48 MB       │ 14d 3h   │
│ frontend │ running  │ 0.8%   │ 32 MB       │ 14d 3h   │
│ redis    │ running  │ 0.3%   │ 12 MB       │ 3d 7h    │
│ postgres │ running  │ 1.2%   │ 256 MB      │ 3d 7h    │
└──────────┴──────────┴────────┴─────────────┴──────────┘

$ bosun deploy ./my-api --domain api.acme.com --ssl --strategy blue-green
Building… ━━━━━━━━━━━━ 100%   Deploying api (blue-green)… ✓   SSL via Caddy… ✓
Security: CrowdSec monitoring api logs ✓

$ bosun security status
Engine: CrowdSec 1.6  │  47 attacks blocked today
Active bans: 12       │  Last: SQLi from 45.xxx (2m ago)

$ bosun gateway cache enable api --ttl 60s
Cache enabled for api  │  TTL: 60s  │  Strategy: disk
```

## Capacidades

| Categoría | Capacidad | Estado |
|---|---|---|
| **Deploy** | Dockerfile + Docker Compose | ✅ |
| **Deploy** | Rolling update (sin downtime) | ✅ |
| **Deploy** | Blue-green deploy + rollback instantáneo | ✅ |
| **Deploy** | Pre/post hooks (`--pre`, `--post`, `bosun.hooks.toml`) | ✅ |
| **Catálogo** | 42 one-click apps con versiones | ✅ |
| **Catálogo** | `bosun apps create redis --version 7-alpine` | ✅ |
| **SSL** | Let's Encrypt automático vía Caddy | ✅ |
| **Proxy** | Caddy reverse proxy con hot-reload | ✅ |
| **Gateway** | APISIX API Gateway (rate-limit, cache, JWT auth, Prometheus) | ✅ |
| **Observabilidad** | Métricas en vivo (CPU/RAM/network), logs streaming | ✅ |
| **Seguridad** | CrowdSec + Fail2Ban zero-config en cada deploy | ✅ |
| **Seguridad** | Pentesting CLI (ports, SSL, headers, secrets, CVEs) | ✅ |
| **Auth** | JWT multi-tenant (admin/user roles, login/logout) | ✅ |
| **Backups** | `bosun backup create/list/restore` (volúmenes + config) | ✅ |
| **CI/CD** | Webhooks (git push → redeploy automático) | ✅ |
| **Dashboard** | TUI interactiva (`bosun dashboard`, ratatui) | ✅ |
| **Instalación** | Un solo comando: `curl \| sudo bash` | ✅ |

### Próximos

- [ ] Docker Swarm multi-node — deploy en clusters
- [ ] One-click app store comunitario — compartir templates
- [ ] Métricas históricas con retención configurable
- [ ] Alertas (Slack, email, webhook) basadas en thresholds

### No planeados (van contra la filosofía de ser ligero)

- Dashboard web (React/Next.js)
- Base de datos externa (MongoDB, PostgreSQL para el PaaS mismo)
- Más contenedores que bosun-daemon + opcionales (Caddy, APISIX, CrowdSec)
- Kubernetes o abstracciones multi-cloud

## Instalación rápida

```bash
# En un VPS Ubuntu/Debian limpio:
curl -fsSL https://raw.githubusercontent.com/rquezada-tech/bosun/main/scripts/install.sh | sudo bash

# Con API Gateway:
curl -fsSL https://raw.githubusercontent.com/rquezada-tech/bosun/main/scripts/install.sh | sudo bash -s -- --with-gateway

# Con seguridad automática:
curl -fsSL https://raw.githubusercontent.com/rquezada-tech/bosun/main/scripts/install.sh | sudo bash -s -- --with-crowdsec
```

Esto instala: Docker Engine + Caddy + bosun-daemon + systemd + TLS + firewall.

### Desde source (desarrolladores)

```bash
git clone https://github.com/rquezada-tech/bosun.git
cd bosun
cargo install --path crates/bosun
cargo install --path crates/bosun-daemon
```

## Uso

```bash
# Conectarse al daemon
export BOSUN_DAEMON=https://mi-server:9090
bosun login                    # auth JWT

# Desplegar
bosun deploy ./app --domain api.acme.com --ssl
bosun deploy ./app --strategy blue-green --pre "npm test" --post "npm run migrate"
bosun apps create redis --version 7-alpine
bosun apps create postgres

# Monitorear
bosun apps list                # tabla con estado, CPU, RAM, uptime
bosun metrics api --live       # htop para una app
bosun apps logs api --follow   # logs en vivo

# Gestionar
bosun apps restart api
bosun backup create api
bosun backup restore abc123

# Seguridad
bosun security status          # CrowdSec/Fail2Ban: ataques bloqueados
bosun security scan api        # pentest: puertos, SSL, headers
bosun security report api      # reporte HTML

# API Gateway
bosun gateway status           # APISIX: rutas, plugins, métricas
bosun gateway cache enable api --ttl 60s
bosun gateway plugin add api rate-limit --config '{"count":100}'

# Dashboard
bosun dashboard                # TUI interactiva con todo en vivo
```

## Arquitectura

```
┌──────────────────────┐         gRPC + TLS          ┌────────────────────────────┐
│      bosun (CLI)     │ ◄─────────────────────────► │   bosun-daemon (server)    │
│      Rust binary     │                             │   Rust binary              │
│      ~8 MB           │                             │   ~10 MB / ~15 MB RAM      │
└──────────────────────┘                             │                            │
                                                     │  ┌──────────────────────┐  │
                                                     │  │ Docker (bollard)     │  │
                                                     │  │ • build, run, stats  │  │
                                                     │  │ • rolling/blue-green │  │
                                                     │  └──────────────────────┘  │
                                                     │  ┌──────────────────────┐  │
                                                     │  │ Caddy (reverse proxy)│  │
                                                     │  │ • SSL auto (LE)      │  │
                                                     │  │ • route hot-reload   │  │
                                                     │  └──────────────────────┘  │
                                                     │  ┌──────────────────────┐  │
                                                     │  │ APISIX (gateway)     │  │
                                                     │  │ • rate-limit, cache  │  │
                                                     │  │ • JWT, Prometheus    │  │
                                                     │  └──────────────────────┘  │
                                                     │  ┌──────────────────────┐  │
                                                     │  │ CrowdSec (IPS)       │  │
                                                     │  │ • log monitoring     │  │
                                                     │  │ • auto-ban           │  │
                                                     │  └──────────────────────┘  │
                                                     │  ┌──────────────────────┐  │
                                                     │  │ SQLite (persist)     │  │
                                                     │  │ • metrics, users     │  │
                                                     │  │ • backups, config    │  │
                                                     │  └──────────────────────┘  │
                                                     └────────────────────────────┘
```

> **Filosofía:** Bosun es dos binarios Rust + Docker Engine. No usa Node.js, MongoDB, Redis, ni ningún runtime externo. Cada línea de código debe justificar su existencia. Un VPS de $5/mes debería poder correr Bosun + todas tus apps.

## Diferencia con otros proyectos

| **Característica**       | **Bosun** | **CapRover** | **Dokku** | **Coolify** | **Kamal** |
|---------------------------|-----------|-------------|-----------|-------------|-----------|
| **Dashboard**             | TUI (ratatui) | Web (React) | Ninguno | Web (Next.js) | Ninguno |
| **RAM del PaaS**          | ~15 MB    | ~500 MB     | ~50 MB    | ~300 MB     | ~50 MB    |
| **Dependencias externas** | 0         | Node + MongoDB | Bash    | PHP + Next   | Ruby      |
| **SSL automático**        | ✅ Caddy  | ✅ Let's Encrypt | ✅       | ✅           | ✅        |
| **Rolling update**        | ✅        | ❌          | ❌        | ❌          | ✅        |
| **Blue-green deploy**     | ✅        | ❌          | ❌        | ❌          | ❌        |
| **Catálogo apps**         | 42 apps   | 100+ apps   | Plugins  | 50+ apps    | ❌        |
| **API Gateway integrado** | ✅ APISIX | ❌          | ❌        | ❌          | ❌        |
| **Seguridad automática**  | ✅ CrowdSec | ❌        | ❌        | ❌          | ❌        |
| **Pentesting CLI**        | ✅        | ❌          | ❌        | ❌          | ❌        |
| **Auth multi-tenant**     | ✅ JWT    | ✅          | ❌        | ✅          | ❌        |
| **Backup/Restore**        | ✅        | ✅          | ❌        | ✅          | ❌        |
| **Instalación**           | `curl \| bash` | Script   | `apt-get` | `docker run` | Ruby gem |
| **Lenguaje**              | Rust      | TypeScript  | Bash      | PHP         | Ruby      |

**Bosun es el único PaaS con API Gateway, seguridad automática, pentesting CLI, y dashboard TUI — todo en menos de 15 MB de RAM.**

## Estructura del proyecto

```
bosun/
├── crates/
│   ├── bosun/               # CLI (cliente gRPC)
│   │   └── src/
│   │       ├── main.rs      # Entry point
│   │       ├── cli.rs       # Argumentos y handlers (apps, deploy, metrics, backup...)
│   │       ├── client.rs    # Cliente gRPC con auth token
│   │       └── proto.rs     # Código generado protobuf
│   └── bosun-daemon/        # Daemon (servidor gRPC)
│       └── src/
│           ├── main.rs      # Entry point, TLS, gRPC server, webhook server
│           ├── server/      # Handlers gRPC (Bosun trait)
│           ├── docker/      # Cliente Docker (bollard): deploy, metrics, logs
│           ├── templates/   # Catálogo TOML de 42 apps
│           ├── auth/        # JWT auth, interceptor, roles
│           ├── backup/      # Backup/Restore de volúmenes
│           ├── security/    # CrowdSec/Fail2Ban + pentesting
│           ├── gateway/     # APISIX Admin API client
│           ├── metrics/     # Recolección de métricas Docker
│           ├── health/      # Health checker + auto-restart
│           ├── webhook/     # HTTP server para git push → redeploy
│           ├── hooks/       # Pre/post deploy hooks
│           ├── deploy/      # Estrategias (direct, rolling, blue-green)
│           ├── proxy/       # Caddy Admin API client
│           ├── persist/     # SQLite (users, metrics, config, apps)
│           └── templates/   # Motor de catálogo TOML
├── templates/catalog/       # 42 archivos .toml de apps
├── proto/bosun/v1/          # Definiciones protobuf gRPC
├── scripts/
│   ├── install.sh           # Bootstrap VPS (un comando)
│   ├── uninstall.sh         # Limpieza completa
│   └── bosun-daemon.service # Unit systemd
├── docs/plans/              # Planes de arquitectura e implementación
└── .github/workflows/       # CI/CD (7 jobs)
```

## Política de contribución

> **Este proyecto es privado en desarrollo activo. Eventualmente será público bajo la misma política.**

Bosun da la bienvenida a contribuciones de la comunidad. El proyecto sigue un **modelo de contribución abierta** con los siguientes principios:

### ¿Cómo contribuir?

**1. Código (Rust)**
- Abre un issue primero para discutir cambios mayores
- Los PRs pequeños son mejores: resuelven un problema específico
- `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test` deben pasar
- Respeta la filosofía del proyecto: simple, sin dependencias externas innecesarias

**2. Catálogo de apps**
- Agrega templates en `templates/catalog/` siguiendo el formato TOML
- Cada template debe tener: nombre, descripción, categoría, al menos una versión con imagen Docker
- Las imágenes deben ser oficiales o de maintainers verificados (Docker Hub)

**3. Reporte de errores**
- Usa issues para reportar bugs con pasos para reproducir
- Incluye logs del daemon (`journalctl -u bosun-daemon -n 100`)
- Para bugs de deploy: incluye el Dockerfile o template usado

**4. Mejoras y features**
- Discutimos en issues antes de implementar
- Se valora especialmente todo lo que reduzca el uso de RAM o simplifique el stack

### Lo que NO vamos a aceptar

- Features que requieran servicios externos o conexión a Internet (salvo Let's Encrypt y webhooks)
- Código que agregue dependencias de runtime pesadas (Node.js, Python, Ruby)
- Un dashboard web — el dashboard TUI es suficiente y deliberado
- Cambios que rompan la compatibilidad hacia atrás de la API gRPC

### Proceso de PR

```
1. Fork del repositorio
2. Crea una rama: git checkout -b feat/mi-mejora
3. Haz tus cambios y commitea: git commit -m "feat: descripción clara"
4. Push a tu fork: git push origin feat/mi-mejora
5. Abre un Pull Request con descripción clara de qué y por qué
```

### Normas de conducta

- Sé respetuoso con otros colaboradores
- Las discusiones técnicas se resuelven con datos, no con opinión
- Prioriza la utilidad práctica sobre la elegancia teórica

---

## Licencia

GPLv3+. Ver [LICENSE](LICENSE).

> **¿Por qué GPLv3+?** Bosun reemplaza herramientas PaaS privativas y source-available. El copyleft asegura que cada mejora — nuestra o de la comunidad — permanezca libre para todos. Si desplegás Bosun en producción, tus usuarios merecen las mismas libertades que vos recibiste.

---

*Bosun: el PaaS que no pesa más que las apps que hostea.*
