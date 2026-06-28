#!/usr/bin/env bash
#
# bosun — uninstall script
# Stops and removes the bosun-daemon service, binaries, config, and data.
# Optionally removes Docker and Rust.
#
# Usage:
#   sudo bash uninstall.sh
#
set -euo pipefail

# ── Colors ────────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

info()  { echo -e "${GREEN}[INFO]${NC}  $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
err()   { echo -e "${RED}[ERROR]${NC} $*" >&2; }

# ── Root check ────────────────────────────────────────────────────────────────
if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
    err "This script must be run as root. Use: sudo bash uninstall.sh"
    exit 1
fi

BOSUN_USER="${BOSUN_USER:-bosun}"
BOSUN_BIN_DIR="${BOSUN_BIN_DIR:-/usr/local/bin}"
BOSUN_CONFIG_DIR="${BOSUN_CONFIG_DIR:-/etc/bosun}"
BOSUN_DATA_DIR="${BOSUN_DATA_DIR:-/var/lib/bosun}"
BOSUN_CACHE_DIR="${BOSUN_CACHE_DIR:-/var/cache/bosun}"
SERVICE_FILE="/etc/systemd/system/bosun-daemon.service"

echo ""
echo -e "${CYAN}╔══════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║        Bosun Uninstall Script           ║${NC}"
echo -e "${CYAN}╚══════════════════════════════════════════╝${NC}"
echo ""

# ── Step 1: Stop and disable systemd service ──────────────────────────────────
info "Stopping and disabling bosun-daemon service..."

if [ -f "$SERVICE_FILE" ]; then
    if systemctl is-active --quiet bosun-daemon.service 2>/dev/null; then
        systemctl stop bosun-daemon.service
        info "bosun-daemon service stopped."
    else
        info "bosun-daemon service is not running."
    fi

    if systemctl is-enabled --quiet bosun-daemon.service 2>/dev/null; then
        systemctl disable bosun-daemon.service
        info "bosun-daemon service disabled."
    fi

    rm -f "$SERVICE_FILE"
    systemctl daemon-reload
    info "Service file removed."
else
    info "No systemd service file found at $SERVICE_FILE"
fi

# ── Step 2: Remove bosun user ─────────────────────────────────────────────────
info "Removing bosun system user..."
if id -u "$BOSUN_USER" &>/dev/null; then
    userdel -r "$BOSUN_USER" 2>/dev/null || userdel "$BOSUN_USER"
    info "User '$BOSUN_USER' removed."
else
    info "User '$BOSUN_USER' does not exist."
fi

# ── Step 3: Remove binaries ───────────────────────────────────────────────────
info "Removing bosun binaries..."
REMOVED_BINARY=false
for bin in bosun-daemon bosun; do
    if [ -f "${BOSUN_BIN_DIR}/${bin}" ]; then
        rm -f "${BOSUN_BIN_DIR}/${bin}"
        info "  Removed: ${BOSUN_BIN_DIR}/${bin}"
        REMOVED_BINARY=true
    fi
done
if [ "$REMOVED_BINARY" = false ]; then
    info "  No bosun binaries found in $BOSUN_BIN_DIR"
fi

# ── Step 4: Remove config and data ────────────────────────────────────────────
info "Removing config and data directories..."
for dir in "$BOSUN_CONFIG_DIR" "$BOSUN_DATA_DIR" "$BOSUN_CACHE_DIR"; do
    if [ -d "$dir" ]; then
        rm -rf "$dir"
        info "  Removed: $dir"
    fi
done

# Remove any leftover cargo build artifacts from /tmp
rm -rf /tmp/bosun-build.* 2>/dev/null || true

# ── Step 5: Close firewall port ───────────────────────────────────────────────
if command -v ufw &>/dev/null && ufw status 2>/dev/null | grep -q "Status: active"; then
    BOSUN_RULE="$(ufw status numbered 2>/dev/null | grep -i '9090.*Bosun\|Bosun.*9090' || true)"
    if [ -n "$BOSUN_RULE" ]; then
        info "Found UFW rule for bosun port. Remove manually with:"
        info "  sudo ufw status numbered"
        info "  sudo ufw delete <rule-number>"
    else
        info "No UFW rule found for bosun port."
    fi
fi

# ── Step 6: Optional — Remove Docker ──────────────────────────────────────────
echo ""
echo -e "${YELLOW}╔══════════════════════════════════════════╗${NC}"
echo -e "${YELLOW}║        Optional Cleanup                  ║${NC}"
echo -e "${YELLOW}╚══════════════════════════════════════════╝${NC}"
echo ""

if command -v docker &>/dev/null; then
    read -r -p "$(echo -e "${YELLOW}Remove Docker Engine and all containers/images? [y/N]: ${NC}")" REMOVE_DOCKER
    if [[ "${REMOVE_DOCKER,,}" =~ ^y(es)?$ ]]; then
        info "Removing Docker Engine..."
        systemctl stop docker docker.socket 2>/dev/null || true
        apt-get purge -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin 2>/dev/null || true
        rm -rf /var/lib/docker /var/lib/containerd
        rm -f /etc/apt/sources.list.d/docker.list
        rm -f /etc/apt/keyrings/docker.asc
        info "Docker Engine removed."
    else
        info "Keeping Docker Engine."
    fi
fi

if command -v rustup &>/dev/null; then
    read -r -p "$(echo -e "${YELLOW}Remove Rust toolchain? [y/N]: ${NC}")" REMOVE_RUST
    if [[ "${REMOVE_RUST,,}" =~ ^y(es)?$ ]]; then
        info "Removing Rust toolchain..."
        rustup self uninstall -y
        rm -rf "$HOME/.cargo" "$HOME/.rustup"
        info "Rust toolchain removed."
    else
        info "Keeping Rust toolchain."
    fi
fi

# ── Done ──────────────────────────────────────────────────────────────────────
echo ""
echo -e "${GREEN}╔══════════════════════════════════════════╗${NC}"
echo -e "${GREEN}║   Bosun has been uninstalled.            ║${NC}"
echo -e "${GREEN}╚══════════════════════════════════════════╝${NC}"
echo ""
