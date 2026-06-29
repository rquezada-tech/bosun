# Bosun

> 🌐 **Spanish version:** [README.md](./README.md) — [Ver este README en Español](./README.md)

<p align="center">
  <img src="https://raw.githubusercontent.com/rquezada-tech/bosun/main/logo_bosun.png" alt="Bosun Logo" width="340">
</p>

> *Deploy Docker apps, monitor metrics, and manage SSL — all from your terminal. Zero dashboard. Pure CLI.*

<!-- Badges -->
<div align="center">

![Status](https://img.shields.io/badge/Status-Alpha-f97316?style=flat-square&labelColor=374151)
![Version](https://img.shields.io/badge/Version-0.1.0-2563eb?style=flat-square&labelColor=374151)
![Paradigm](https://img.shields.io/badge/Paradigm-CLI_First-22c55e?style=flat-square&labelColor=374151)
![RAM](https://img.shields.io/badge/RAM-15MB_daemon-22c55e?style=flat-square&labelColor=374151)
![Stack](https://img.shields.io/badge/Stack-Rust_%2B_gRPC_%2B_SQLite-0ea5e9?style=flat-square&labelColor=374151)
![License](https://img.shields.io/badge/License-GPLv3+-2f855a?style=flat-square&labelColor=374151)

</div>

## What is Bosun?

Bosun is a PaaS that runs entirely in your terminal. No browser. No React dashboard. No hundreds of megabytes of RAM wasted on a UI you look at twice a month. Just a tiny Rust daemon on your server and a single CLI binary on your machine.

**Bosun replaces CapRover, Dokku, and Coolify with a single ~15 MB RAM Rust binary. No Node.js. No MongoDB. No external runtime. Just Docker Engine.**

## Quick Install

```bash
curl -fsSL https://raw.githubusercontent.com/rquezada-tech/bosun/main/scripts/install.sh | sudo bash
```

This installs: Docker Engine + Caddy + bosun-daemon + systemd + TLS + firewall.

See the [Spanish README](./README.md) for full documentation including capabilities, architecture, comparison with other projects, and contribution guidelines.

## License

GPLv3+. See [LICENSE](LICENSE).

---

*Bosun: the PaaS that weighs less than the apps it hosts.*
