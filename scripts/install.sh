#!/usr/bin/env bash
#
# bosun — one-command VPS install script
# Bootstraps a fresh Ubuntu/Debian VPS with Docker, Rust, the bosun-daemon,
# TLS certificates, and a systemd service. Idempotent: safe to run multiple times.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/rquezada-tech/bosun/main/scripts/install.sh | sudo bash
#
set -euo pipefail

# ── Colors ────────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

info()  { echo -e "${GREEN}[INFO]${NC}  $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
err()   { echo -e "${RED}[ERROR]${NC} $*" >&2; }
header(){ echo -e "\n${CYAN}═══ $* ═══${NC}\n"; }

# ── Root check ────────────────────────────────────────────────────────────────
if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
    err "This script must be run as root. Use: sudo bash install.sh"
    exit 1
fi

# ── Configuration (override via env vars) ─────────────────────────────────────
BOSUN_VERSION="${BOSUN_VERSION:-main}"
BOSUN_REPO="${BOSUN_REPO:-https://github.com/rquezada-tech/bosun.git}"
BOSUN_USER="${BOSUN_USER:-bosun}"
BOSUN_BIN_DIR="${BOSUN_BIN_DIR:-/usr/local/bin}"
BOSUN_CONFIG_DIR="${BOSUN_CONFIG_DIR:-/etc/bosun}"
BOSUN_DATA_DIR="${BOSUN_DATA_DIR:-/var/lib/bosun}"
BOSUN_CACHE_DIR="${BOSUN_CACHE_DIR:-/var/cache/bosun}"
BOSUN_LISTEN_ADDR="${BOSUN_LISTEN_ADDR:-0.0.0.0:9090}"
BOSUN_RUST_LOG="${BOSUN_RUST_LOG:-bosun_daemon=info}"

CERT_FILE="${BOSUN_CONFIG_DIR}/server.crt"
KEY_FILE="${BOSUN_CONFIG_DIR}/server.key"
BUILD_DIR="$(mktemp -d /tmp/bosun-build.XXXXXX)"
trap 'rm -rf "$BUILD_DIR"' EXIT

# ── Step 1: OS detection ──────────────────────────────────────────────────────
header "Step 1/10: Detecting operating system"

if [ -f /etc/os-release ]; then
    # shellcheck source=/dev/null
    . /etc/os-release
else
    err "Cannot detect OS. /etc/os-release not found."
    exit 1
fi

case "$ID" in
    ubuntu|debian)
        info "Detected $NAME $VERSION_ID — supported."
        ;;
    *)
        err "Unsupported OS: $ID. This script only supports Ubuntu and Debian."
        err "Patches for other distributions are welcome at: $BOSUN_REPO"
        exit 1
        ;;
esac

# ── Step 2: Install Docker Engine ─────────────────────────────────────────────
header "Step 2/10: Installing Docker Engine"

if command -v docker &>/dev/null && docker info &>/dev/null 2>&1; then
    info "Docker Engine is already installed and running."
    DOCKER_VERSION="$(docker --version 2>/dev/null || echo 'unknown')"
    info "  Version: $DOCKER_VERSION"
else
    info "Installing Docker Engine from official Docker repository..."

    # Remove any old packages
    for pkg in docker.io docker-doc docker-compose docker-compose-v2 podman-docker containerd runc; do
        apt-get remove -y "$pkg" &>/dev/null || true
    done

    # Install prerequisites
    apt-get update -qq
    apt-get install -y -qq ca-certificates curl gnupg lsb-release

    # Add Docker's official GPG key
    install -m 0755 -d /etc/apt/keyrings
    if [ ! -f /etc/apt/keyrings/docker.asc ]; then
        curl -fsSL https://download.docker.com/linux/"$ID"/gpg -o /etc/apt/keyrings/docker.asc
        chmod a+r /etc/apt/keyrings/docker.asc
    fi

    # Add the repository
    echo \
        "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] \
        https://download.docker.com/linux/$ID $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
        | tee /etc/apt/sources.list.d/docker.list > /dev/null

    apt-get update -qq
    apt-get install -y -qq docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin

    # Verify Docker is working
    if ! docker info &>/dev/null 2>&1; then
        err "Docker Engine installed but cannot connect. Is the Docker daemon running?"
        err "Try: sudo systemctl start docker"
        exit 1
    fi

    info "Docker Engine installed successfully."
fi

# Ensure current user can access Docker (if we created a bosun user already)
# We'll handle group membership in the user-creation step below.

# ── Step 3: Install Rust toolchain ────────────────────────────────────────────
header "Step 3/10: Installing Rust toolchain"

if command -v cargo &>/dev/null && rustup --version &>/dev/null 2>&1; then
    info "Rust toolchain is already installed."
    RUST_VERSION="$(rustc --version 2>/dev/null || echo 'unknown')"
    info "  Version: $RUST_VERSION"
else
    info "Installing Rust via rustup..."
    export RUSTUP_INIT_SKIP_PATH_CHECK=yes
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable

    # Source cargo env for this session
    # shellcheck source=/dev/null
    if [ -f "$HOME/.cargo/env" ]; then
        . "$HOME/.cargo/env"
    fi

    info "Rust installed successfully."
fi

# Ensure cargo is in PATH
if ! command -v cargo &>/dev/null; then
    # shellcheck source=/dev/null
    [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
    export PATH="$HOME/.cargo/bin:$PATH"
fi

# ── Step 4: Clone / update bosun repo ─────────────────────────────────────────
header "Step 4/10: Fetching bosun source"

if [ -d "$BUILD_DIR/.git" ] && git -C "$BUILD_DIR" remote get-url origin &>/dev/null 2>&1; then
    info "Updating existing clone..."
    git -C "$BUILD_DIR" fetch origin
    git -C "$BUILD_DIR" checkout "$BOSUN_VERSION"
    git -C "$BUILD_DIR" pull origin "$BOSUN_VERSION" || true
else
    info "Cloning bosun repository (branch: $BOSUN_VERSION)..."
    git clone --depth 1 --branch "$BOSUN_VERSION" "$BOSUN_REPO" "$BUILD_DIR" || {
        # Fallback: clone main and checkout
        warn "Branch '$BOSUN_VERSION' not found, falling back to main."
        rm -rf "$BUILD_DIR"
        git clone --depth 1 "$BOSUN_REPO" "$BUILD_DIR"
    }
fi

# ── Step 5: Build bosun-daemon ────────────────────────────────────────────────
header "Step 5/10: Building bosun-daemon (release)"

cd "$BUILD_DIR"

info "This may take a few minutes on the first run..."
cargo build --release --bin bosun-daemon 2>&1

if [ ! -f "$BUILD_DIR/target/release/bosun-daemon" ]; then
    err "Build failed. Check the output above for errors."
    exit 1
fi

# Install the binary
install -m 0755 "$BUILD_DIR/target/release/bosun-daemon" "$BOSUN_BIN_DIR/bosun-daemon"
info "Installed bosun-daemon to $BOSUN_BIN_DIR/bosun-daemon"

# ── Step 6: Create /etc/bosun/ directory ──────────────────────────────────────
header "Step 6/10: Setting up configuration directory"

mkdir -p "$BOSUN_CONFIG_DIR"
chmod 0750 "$BOSUN_CONFIG_DIR"

# ── Step 7: Generate self-signed TLS cert (if no certs provided) ──────────────
header "Step 7/10: Setting up TLS certificates"

if [ -f "$CERT_FILE" ] && [ -f "$KEY_FILE" ]; then
    info "TLS certificates already present:"
    info "  Cert: $CERT_FILE"
    info "  Key:  $KEY_FILE"

    # Verify cert matches key
    if openssl x509 -noout -modulus -in "$CERT_FILE" 2>/dev/null | openssl md5 | \
       diff - <(openssl rsa -noout -modulus -in "$KEY_FILE" 2>/dev/null | openssl md5) &>/dev/null; then
        info "Certificate and key match — using existing."
    else
        warn "Existing certificate and key do not match. Regenerating..."
        FORCE_REGENERATE=true
    fi
else
    FORCE_REGENERATE=true
fi

if [ "${FORCE_REGENERATE:-false}" = "true" ]; then
    info "Generating self-signed TLS certificate..."

    # Detect server hostname or IP for the cert
    SERVER_NAME="${SERVER_NAME:-$(hostname -f 2>/dev/null || hostname)}"
    info "  Server name: $SERVER_NAME"

    openssl req -x509 -nodes -days 365 -newkey rsa:4096 \
        -keyout "$KEY_FILE" \
        -out "$CERT_FILE" \
        -subj "/CN=${SERVER_NAME}/O=Bosun PaaS/C=US" \
        -addext "subjectAltName=DNS:${SERVER_NAME},DNS:localhost,IP:127.0.0.1" 2>/dev/null

    chmod 0600 "$KEY_FILE"
    chmod 0644 "$CERT_FILE"

    info "Self-signed certificate generated."
    warn "This is a self-signed certificate. For production, replace with a real cert"
    warn "(e.g., Let's Encrypt via certbot) at:"
    warn "  $CERT_FILE"
    warn "  $KEY_FILE"
fi

# ── Step 8: Create systemd service ────────────────────────────────────────────
header "Step 8/10: Creating systemd service"

# Create bosun system user if it doesn't exist
if ! id -u "$BOSUN_USER" &>/dev/null; then
    info "Creating system user: $BOSUN_USER"
    useradd --system --no-create-home --shell /usr/sbin/nologin \
        --home-dir "$BOSUN_DATA_DIR" "$BOSUN_USER"

    # Add bosun user to docker group so it can manage containers
    usermod -aG docker "$BOSUN_USER"
fi

# Create data directories
mkdir -p "$BOSUN_DATA_DIR" "$BOSUN_CACHE_DIR"
chown -R "${BOSUN_USER}:${BOSUN_USER}" "$BOSUN_DATA_DIR" "$BOSUN_CACHE_DIR" "$BOSUN_CONFIG_DIR"

# Write systemd unit file
SERVICE_FILE="/etc/systemd/system/bosun-daemon.service"

if [ -f "$SERVICE_FILE" ]; then
    info "systemd service already exists. Updating..."
else
    info "Creating systemd service..."
fi

cat > "$SERVICE_FILE" << SYSTEMDEOF
[Unit]
Description=Bosun PaaS Daemon
Documentation=https://github.com/rquezada-tech/bosun
After=network-online.target docker.service
Wants=network-online.target docker.service
Requires=docker.service

[Service]
Type=simple
User=${BOSUN_USER}
Group=${BOSUN_USER}
ExecStart=${BOSUN_BIN_DIR}/bosun-daemon \\
    --listen ${BOSUN_LISTEN_ADDR} \\
    --data-dir ${BOSUN_DATA_DIR} \\
    --cert ${CERT_FILE} \\
    --key ${KEY_FILE}
Restart=always
RestartSec=5
Environment=RUST_LOG=${BOSUN_RUST_LOG}

# Sandboxing / hardening
ProtectSystem=strict
ProtectHome=yes
NoNewPrivileges=yes
PrivateTmp=yes
ReadWritePaths=${BOSUN_DATA_DIR} ${BOSUN_CACHE_DIR} ${BOSUN_CONFIG_DIR}
ReadOnlyPaths=/etc/ssl/certs

# Limits
LimitNOFILE=65536
LimitNPROC=4096

[Install]
WantedBy=multi-user.target
SYSTEMDEOF

chmod 0644 "$SERVICE_FILE"

# Reload systemd
systemctl daemon-reload

# ── Step 9: Enable and start the service ──────────────────────────────────────
header "Step 9/10: Enabling and starting bosun-daemon"

systemctl enable bosun-daemon.service

if systemctl is-active --quiet bosun-daemon.service; then
    info "bosun-daemon is already running. Restarting..."
    systemctl restart bosun-daemon.service
else
    info "Starting bosun-daemon..."
    systemctl start bosun-daemon.service
fi

# Give it a moment to start
sleep 2

if systemctl is-active --quiet bosun-daemon.service; then
    info "bosun-daemon is running!"
else
    warn "bosun-daemon may not have started correctly."
    warn "Check logs: journalctl -u bosun-daemon -f"
fi

# ── Step 10: Open firewall port ───────────────────────────────────────────────
header "Step 10/10: Configuring firewall"

BOSUN_PORT="${BOSUN_LISTEN_ADDR##*:}"

if command -v ufw &>/dev/null && ufw status | grep -q "Status: active"; then
    if ufw status | grep -q "$BOSUN_PORT/tcp"; then
        info "Firewall port $BOSUN_PORT/tcp is already open."
    else
        info "Opening firewall port $BOSUN_PORT/tcp..."
        ufw allow "$BOSUN_PORT/tcp" comment "Bosun gRPC API"
        info "Port $BOSUN_PORT/tcp opened."
    fi
else
    info "ufw not active or not installed. Skipping firewall configuration."
    warn "If you use a different firewall, ensure port $BOSUN_PORT/tcp is open."
fi

# ── Success ───────────────────────────────────────────────────────────────────
echo ""
echo -e "${GREEN}╔══════════════════════════════════════════════════════════════╗${NC}"
echo -e "${GREEN}║                                                              ║${NC}"
echo -e "${GREEN}║   Bosun installed successfully! 🎉                           ║${NC}"
echo -e "${GREEN}║                                                              ║${NC}"
echo -e "${GREEN}╚══════════════════════════════════════════════════════════════╝${NC}"
echo ""
echo -e "  ${CYAN}Daemon status:${NC}  systemctl status bosun-daemon"
echo -e "  ${CYAN}View logs:${NC}      journalctl -u bosun-daemon -f"
echo -e "  ${CYAN}Config dir:${NC}     $BOSUN_CONFIG_DIR"
echo -e "  ${CYAN}Data dir:${NC}       $BOSUN_DATA_DIR"
echo -e "  ${CYAN}Service file:${NC}   $SERVICE_FILE"
echo ""
echo -e "  ${CYAN}Next steps:${NC}"
echo -e "  1. Install the bosun CLI on your local machine:"
echo -e "     cargo install --git $BOSUN_REPO bosun"
echo ""
echo -e "  2. Connect to your daemon:"
echo -e "     export BOSUN_DAEMON=https://$(hostname -I 2>/dev/null | awk '{print $1}' || echo 'YOUR_SERVER_IP'):$BOSUN_PORT"
echo ""
echo -e "  3. Deploy your first app:"
echo -e "     bosun deploy ./my-app --domain my-app.example.com"
echo ""
echo -e "  ${YELLOW}Note:${NC} A self-signed TLS certificate was generated."
echo -e "  ${YELLOW}For trusted TLS, replace the certs in $BOSUN_CONFIG_DIR.${NC}"
echo ""
