#!/usr/bin/env bash
#
# bosun — one-command VPS install script
# Bootstraps a fresh Ubuntu/Debian VPS with Docker, Caddy reverse proxy,
# Rust, the bosun-daemon, TLS certificates, and a systemd service.
# Idempotent: safe to run multiple times.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/rquezada-tech/bosun/main/scripts/install.sh | sudo bash
#   # With Swarm: WITH_SWARM=true sudo bash install.sh
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
BOSUN_WEBHOOK_LISTEN="${BOSUN_WEBHOOK_LISTEN:-0.0.0.0:9091}"
BOSUN_RUST_LOG="${BOSUN_RUST_LOG:-bosun_daemon=info}"
WITH_GATEWAY="${WITH_GATEWAY:-false}"  # Set to true to install APISIX API Gateway
WITH_CROWDSEC="${WITH_CROWDSEC:-false}"  # Set to true to install CrowdSec IDS/IPS
WITH_SWARM="${WITH_SWARM:-false}"  # Set to true to initialize Docker Swarm during install
AS_CONTROLLER="false"  # Set via --as-controller flag to configure as multi-cloud controller
# If WITH_CROWDSEC is not set, install fail2ban as lightweight fallback

# ── Parse command-line flags ───────────────────────────────────────────────────
for arg in "$@"; do
    case "$arg" in
        --as-controller)
            AS_CONTROLLER="true"
            info "Controller mode requested: will configure as multi-cloud orchestration controller"
            ;;
        --with-swarm)
            WITH_SWARM="true"
            ;;
        --with-gateway)
            WITH_GATEWAY="true"
            ;;
        --with-crowdsec)
            WITH_CROWDSEC="true"
            ;;
        --help|-h)
            echo "Usage: sudo bash install.sh [options]"
            echo ""
            echo "Options:"
            echo "  --as-controller   Configure as a multi-cloud orchestration controller"
            echo "  --with-swarm      Initialize Docker Swarm"
            echo "  --with-gateway    Install APISIX API Gateway"
            echo "  --with-crowdsec   Install CrowdSec IDS/IPS (default: fail2ban)"
            echo ""
            echo "Environment variables:"
            echo "  BOSUN_VERSION, BOSUN_REPO, BOSUN_USER, BOSUN_LISTEN_ADDR, ..."
            exit 0
            ;;
    esac
done

CERT_FILE="${BOSUN_CONFIG_DIR}/server.crt"
KEY_FILE="${BOSUN_CONFIG_DIR}/server.key"
WEBHOOK_SECRET_FILE="${BOSUN_CONFIG_DIR}/webhook-secret"
JWT_SECRET_FILE="${BOSUN_CONFIG_DIR}/jwt-secret"
MCP_API_KEY_FILE="${BOSUN_CONFIG_DIR}/mcp-api-key"
BUILD_DIR="$(mktemp -d /tmp/bosun-build.XXXXXX)"
trap 'rm -rf "$BUILD_DIR"' EXIT

# ── Step 1: OS detection ──────────────────────────────────────────────────────
header "Step 1/13: Detecting operating system"

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
header "Step 2/13: Installing Docker Engine"

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

# ── Step 3: Docker Swarm initialization (optional, with --with-swarm) ────────
header "Step 3/15: Docker Swarm (optional)"

if [ "$WITH_SWARM" = "true" ]; then
    info "Docker Swarm requested via --with-swarm"

    # Check if already in Swarm mode
    if docker info --format '{{.Swarm.LocalNodeState}}' 2>/dev/null | grep -q "active"; then
        info "Docker is already in Swarm mode."
    else
        info "Initializing Docker Swarm..."
        if docker swarm init 2>/dev/null; then
            info "Docker Swarm initialized. This node is now a Swarm manager."

            # Show join token
            WORKER_TOKEN="$(docker swarm join-token -q worker 2>/dev/null || echo '')"
            MANAGER_TOKEN="$(docker swarm join-token -q manager 2>/dev/null || echo '')"

            echo -e "  ${GREEN}╔══════════════════════════════════════════════════════════════╗${NC}"
            echo -e "  ${GREEN}║  Docker Swarm initialized!                                  ║${NC}"
            echo -e "  ${GREEN}╚══════════════════════════════════════════════════════════════╝${NC}"
            echo ""
            echo -e "  To join worker nodes to this cluster:"
            echo -e "    docker swarm join --token ${WORKER_TOKEN} <MANAGER_IP>:2377"
            echo ""
            if [ -n "$MANAGER_TOKEN" ]; then
                echo -e "  To join manager nodes (for HA):"
                echo -e "    docker swarm join --token ${MANAGER_TOKEN} <MANAGER_IP>:2377"
                echo ""
            fi
        else
            warn "Docker Swarm init failed. Continuing without Swarm."
            warn "You can initialize later: docker swarm init"
        fi
    fi
else
    info "Docker Swarm not requested. Skipping."
    info "  To enable later: re-run with WITH_SWARM=true or:"
    info "    docker swarm init"
    info "    # Or use: bosun cluster init"
fi

# ── Step 3: Install Caddy reverse proxy ────────────────────────────────────────
header "Step 3/13: Installing Caddy reverse proxy"

if command -v caddy &>/dev/null; then
    CADDY_VERSION="$(caddy version 2>/dev/null || echo 'unknown')"
    info "Caddy is already installed."
    info "  Version: $CADDY_VERSION"
else
    info "Installing Caddy from official repository..."

    # Install prerequisites
    apt-get install -y -qq debian-archive-keyring curl

    # Add Caddy GPG key
    if [ ! -f /usr/share/keyrings/caddy-archive-keyring.gpg ]; then
        curl -fsSL https://dl.cloudsmith.io/public/caddy/stable/gpg.key \
            | gpg --dearmor -o /usr/share/keyrings/caddy-archive-keyring.gpg
    fi

    # Add Caddy repository
    if [ ! -f /etc/apt/sources.list.d/caddy-stable.list ]; then
        curl -fsSL https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt \
            | tee /etc/apt/sources.list.d/caddy-stable.list > /dev/null
    fi

    apt-get update -qq
    apt-get install -y -qq caddy

    # Enable and start Caddy
    systemctl enable caddy.service
    if systemctl is-active --quiet caddy.service; then
        systemctl restart caddy.service
    else
        systemctl start caddy.service
    fi

    # Verify Caddy is running
    sleep 2
    if systemctl is-active --quiet caddy.service; then
        info "Caddy is running on port 80/443."
    else
        warn "Caddy installed but may not be running. Check: systemctl status caddy"
    fi

    info "Caddy installed successfully."
fi

# ── Step 4: Install APISIX API Gateway (optional, with --with-gateway) ───────
header "Step 4/14: APISIX API Gateway (optional)"

if [ "$WITH_GATEWAY" = "true" ]; then
    info "APISIX API Gateway requested. Installing via Docker..."

    # Check if bosun Docker network exists, create if not
    if ! docker network inspect bosun &>/dev/null 2>&1; then
        info "Creating bosun Docker network..."
        docker network create bosun
    fi

    # Check if APISIX is already running
    if docker ps --format '{{.Names}}' | grep -q '^apisix$'; then
        info "APISIX container is already running."
        APISIX_VERSION="$(docker inspect apisix --format '{{.Config.Image}}' 2>/dev/null || echo 'unknown')"
        info "  Image: $APISIX_VERSION"
    else
        # Remove stopped container if it exists
        docker rm -f apisix &>/dev/null || true

        info "Starting APISIX container..."
        docker run -d \
            --name apisix \
            --network bosun \
            --restart unless-stopped \
            -p 9080:9080 \
            -p 9180:9180 \
            -e APISIX_STAND_ALONE=true \
            apache/apisix:latest

        # Wait for APISIX to become healthy
        info "Waiting for APISIX to start..."
        for i in $(seq 1 30); do
            if curl -s http://localhost:9180/apisix/admin/routes > /dev/null 2>&1; then
                info "APISIX is ready (Admin API on port 9180, Proxy on port 9080)"
                break
            fi
            sleep 2
        done
    fi

    # Create a systemd unit for the APISIX container (ensures it starts on boot)
    APISIX_SERVICE_FILE="/etc/systemd/system/apisix.service"
    if [ ! -f "$APISIX_SERVICE_FILE" ]; then
        info "Creating systemd unit for APISIX container..."
        cat > "$APISIX_SERVICE_FILE" << 'SYSTEMDEOF'
[Unit]
Description=APISIX API Gateway (Docker)
Documentation=https://apisix.apache.org/docs/
After=docker.service network-online.target
Wants=docker.service network-online.target
Requires=docker.service

[Service]
Type=simple
Restart=always
RestartSec=10
ExecStartPre=-/usr/bin/docker rm -f apisix
ExecStart=/usr/bin/docker run --rm --name apisix \
    --network bosun \
    -p 9080:9080 \
    -p 9180:9180 \
    -e APISIX_STAND_ALONE=true \
    apache/apisix:latest
ExecStop=/usr/bin/docker stop apisix

[Install]
WantedBy=multi-user.target
SYSTEMDEOF
        systemctl daemon-reload
        systemctl enable apisix.service
        info "APISIX systemd service created and enabled."
    fi
else
    info "APISIX API Gateway not requested. Skipping."
    info "  To enable later: re-run with WITH_GATEWAY=true or:"
    info "    docker run -d --name apisix --network bosun \\"
    info "      -p 9080:9080 -p 9180:9180 -e APISIX_STAND_ALONE=true apache/apisix:latest"
fi

# ── Step 5: Install Rust toolchain ────────────────────────────────────────────
header "Step 5/14: Installing Rust toolchain"

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

# ── Step 6: Clone / update bosun repo ─────────────────────────────────────────
header "Step 6/14: Fetching bosun source"

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

# ── Step 7: Build bosun-daemon ────────────────────────────────────────────────
header "Step 7/14: Building bosun-daemon (release)"

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

# ── Step 8: Create /etc/bosun/ directory and install catalog ──────────────────
header "Step 8/14: Setting up configuration directory and app catalog"

mkdir -p "$BOSUN_CONFIG_DIR"
chmod 0750 "$BOSUN_CONFIG_DIR"

# Copy template catalog to /etc/bosun/catalog/
CATALOG_SRC="${BUILD_DIR}/templates/catalog"
CATALOG_DST="${BOSUN_CONFIG_DIR}/catalog"
if [ -d "$CATALOG_SRC" ]; then
    info "Installing app template catalog to $CATALOG_DST"
    rm -rf "$CATALOG_DST"
    cp -r "$CATALOG_SRC" "$CATALOG_DST"
    chown -R "${BOSUN_USER}:${BOSUN_USER}" "$CATALOG_DST" 2>/dev/null || true
    info "App catalog installed ($(find "$CATALOG_DST" -name '*.toml' | wc -l) templates)"
else
    warn "Template catalog not found at $CATALOG_SRC. Templates will be unavailable."
fi

# ── Step 9: Generate self-signed TLS cert (if no certs provided) ──────────────
header "Step 9/14: Setting up TLS certificates"

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
    info "Generating self-signed TLS certificate for gRPC mTLS..."
    info "(This is for daemon communication. Edge SSL for deployed apps is handled by Caddy.)"

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

    info "Self-signed certificate generated for gRPC mTLS."
    warn "This is a self-signed certificate for daemon-to-CLI communication."
    warn "For edge SSL on deployed apps, install Caddy and use: bosun deploy --ssl --domain myapp.example.com"
    warn "Caddy will automatically provision Let's Encrypt certificates for your public domains."
    warn ""
    warn "Self-signed certs:"
    warn "  $CERT_FILE"
    warn "  $KEY_FILE"
fi

# ── Generate controller-specific mTLS certs ──────────────────────────────
if [ "$AS_CONTROLLER" = "true" ]; then
    header "Controller mode: Generating mTLS certificates for inter-node communication"

    CONTROLLER_CA_KEY="${BOSUN_CONFIG_DIR}/controller-ca.key"
    CONTROLLER_CA_CERT="${BOSUN_CONFIG_DIR}/controller-ca.crt"
    CONTROLLER_CLIENT_CERT="${BOSUN_CONFIG_DIR}/controller-client.crt"
    CONTROLLER_CLIENT_KEY="${BOSUN_CONFIG_DIR}/controller-client.key"

    if [ -f "$CONTROLLER_CA_KEY" ] && [ -f "$CONTROLLER_CA_CERT" ]; then
        info "Controller CA certificates already exist."
    else
        info "Generating controller CA (Certificate Authority)..."
        # Generate a CA key and self-signed certificate for mTLS between controller and worker nodes
        openssl req -x509 -nodes -days 3650 -newkey rsa:4096 \
            -keyout "$CONTROLLER_CA_KEY" \
            -out "$CONTROLLER_CA_CERT" \
            -subj "/CN=Bosun Controller CA/O=Bosun PaaS/C=US" 2>/dev/null

        chmod 0600 "$CONTROLLER_CA_KEY"
        chmod 0644 "$CONTROLLER_CA_CERT"
        info "Controller CA generated: $CONTROLLER_CA_CERT"
    fi

    # Generate a client certificate signed by the CA for inter-node gRPC
    if [ -f "$CONTROLLER_CLIENT_KEY" ] && [ -f "$CONTROLLER_CLIENT_CERT" ]; then
        info "Controller client certificates already exist."
    else
        info "Generating controller client certificate (signed by CA)..."
        # Generate client key
        openssl genrsa -out "$CONTROLLER_CLIENT_KEY" 4096 2>/dev/null

        # Generate CSR
        openssl req -new -key "$CONTROLLER_CLIENT_KEY" \
            -out /tmp/bosun-controller-client.csr \
            -subj "/CN=bosun-controller/O=Bosun PaaS/C=US" 2>/dev/null

        # Sign with CA (including SANs for localhost and typical VPS hostnames)
        cat > /tmp/bosun-controller-ext.cnf << 'EOF'
[v3_req]
subjectAltName = DNS:localhost,DNS:controller,DNS:$(hostname -f 2>/dev/null || hostname),IP:127.0.0.1
keyUsage = digitalSignature,keyEncipherment
extendedKeyUsage = clientAuth,serverAuth
EOF

        openssl x509 -req -days 3650 \
            -in /tmp/bosun-controller-client.csr \
            -CA "$CONTROLLER_CA_CERT" \
            -CAkey "$CONTROLLER_CA_KEY" \
            -CAcreateserial \
            -out "$CONTROLLER_CLIENT_CERT" \
            -extensions v3_req \
            -extfile /tmp/bosun-controller-ext.cnf 2>/dev/null

        chmod 0600 "$CONTROLLER_CLIENT_KEY"
        chmod 0644 "$CONTROLLER_CLIENT_CERT"
        rm -f /tmp/bosun-controller-client.csr /tmp/bosun-controller-ext.cnf /tmp/bosun-controller.srl
        info "Controller client certificate generated: $CONTROLLER_CLIENT_CERT"
    fi

    warn "Controller mTLS certificates:"
    warn "  CA Cert:    $CONTROLLER_CA_CERT"
    warn "  CA Key:     $CONTROLLER_CA_KEY"
    warn "  Client Cert: $CONTROLLER_CLIENT_CERT"
    warn "  Client Key:  $CONTROLLER_CLIENT_KEY"
    warn ""
    warn "Share the CA cert ($CONTROLLER_CA_CERT) with worker nodes."
    warn "Worker nodes need their own client certs signed by this CA."
fi

# ── Step 10: Generate mTLS certificates for cross-VPS routing ─────────────
header "Step 10/16: Generating mTLS certificates for cross-VPS routing"

CA_KEY="${BOSUN_CONFIG_DIR}/ca.key"
CA_CRT="${BOSUN_CONFIG_DIR}/ca.crt"
NODE_KEY="${BOSUN_CONFIG_DIR}/node.key"
NODE_CSR="${BOSUN_CONFIG_DIR}/node.csr"
NODE_CRT="${BOSUN_CONFIG_DIR}/node.crt"

# Only generate if gateway is enabled and certs don't exist
if [ "$WITH_GATEWAY" = "true" ]; then
    if [ -f "$CA_CRT" ] && [ -f "$NODE_CRT" ]; then
        info "mTLS certificates already present."
        info "  CA cert: $CA_CRT"
        info "  Node cert: $NODE_CRT"
    else
        info "Generating CA and node certificates for mTLS..."

        # Generate CA private key and self-signed CA certificate
        openssl genrsa -out "$CA_KEY" 4096 2>/dev/null
        chmod 0600 "$CA_KEY"

        openssl req -x509 -new -nodes \
            -key "$CA_KEY" \
            -sha256 -days 3650 \
            -out "$CA_CRT" \
            -subj "/CN=Bosun CA/O=Bosun PaaS/C=US" 2>/dev/null
        chmod 0644 "$CA_CRT"

        info "CA certificate generated: $CA_CRT"

        # Generate node private key and CSR
        openssl genrsa -out "$NODE_KEY" 2048 2>/dev/null
        chmod 0600 "$NODE_KEY"

        SERVER_NAME="${SERVER_NAME:-$(hostname -f 2>/dev/null || hostname)}"
        openssl req -new \
            -key "$NODE_KEY" \
            -out "$NODE_CSR" \
            -subj "/CN=${SERVER_NAME}/O=Bosun Node/C=US" 2>/dev/null

        # Sign the node CSR with the CA
        openssl x509 -req \
            -in "$NODE_CSR" \
            -CA "$CA_CRT" \
            -CAkey "$CA_KEY" \
            -CAcreateserial \
            -out "$NODE_CRT" \
            -days 365 -sha256 2>/dev/null
        chmod 0644 "$NODE_CRT"

        # Clean up CSR
        rm -f "$NODE_CSR"

        chown "${BOSUN_USER}:${BOSUN_USER}" "$CA_KEY" "$CA_CRT" "$NODE_KEY" "$NODE_CRT" 2>/dev/null || true

        info "Node certificate generated: $NODE_CRT"
        info ""

        echo -e "  ${YELLOW}╔══════════════════════════════════════════════════════════════╗${NC}"
        echo -e "  ${YELLOW}║  mTLS certificates generated for cross-VPS routing           ║${NC}"
        echo -e "  ${YELLOW}║                                                              ║${NC}"
        echo -e "  ${YELLOW}║  To add a peer node on another VPS:                          ║${NC}"
        echo -e "  ${YELLOW}║  1. Copy the CA cert to the peer:                            ║${NC}"
        echo -e "  ${YELLOW}║     scp $CA_CRT user@peer:/etc/bosun/ca.crt                  ║${NC}"
        echo -e "  ${YELLOW}║                                                              ║${NC}"
        echo -e "  ${YELLOW}║  2. On the peer, generate its own node cert (same CA):       ║${NC}"
        echo -e "  ${YELLOW}║     (re-run this script with WITH_GATEWAY=true)              ║${NC}"
        echo -e "  ${YELLOW}║                                                              ║${NC}"
        echo -e "  ${YELLOW}║  3. Add the peer to this node's gateway:                     ║${NC}"
        echo -e "  ${YELLOW}║     bosun gateway peer add <name> <peer_addr> $CA_CRT        ║${NC}"
        echo -e "  ${YELLOW}╚══════════════════════════════════════════════════════════════╝${NC}"
        echo ""
    fi
else
    info "APISIX gateway not enabled (WITH_GATEWAY=false). Skipping mTLS certs."
    info "  To enable cross-VPS routing later:"
    info "    1. Re-run install with WITH_GATEWAY=true"
    info "    2. Or generate certs manually with openssl"
fi

# ── Step 11: Generate webhook secret ────────────────────────────────────────────
header "Step 11/16: Generating webhook secret"

if [ -f "$WEBHOOK_SECRET_FILE" ]; then
    info "Webhook secret already exists at $WEBHOOK_SECRET_FILE"
else
    info "Generating random webhook secret..."
    openssl rand -hex 32 > "$WEBHOOK_SECRET_FILE"
    chmod 0600 "$WEBHOOK_SECRET_FILE"
    chown "${BOSUN_USER}:${BOSUN_USER}" "$WEBHOOK_SECRET_FILE" 2>/dev/null || true
    info "Webhook secret generated and saved to $WEBHOOK_SECRET_FILE"
fi

# ── Step 12: Generate JWT secret and admin password ──────────────────────────
header "Step 12/16: Generating JWT authentication secret"

if [ -f "$JWT_SECRET_FILE" ]; then
    info "JWT secret already exists at $JWT_SECRET_FILE"
else
    info "Generating random JWT secret..."
    openssl rand -hex 32 > "$JWT_SECRET_FILE"
    chmod 0600 "$JWT_SECRET_FILE"
    chown "${BOSUN_USER}:${BOSUN_USER}" "$JWT_SECRET_FILE" 2>/dev/null || true
    info "JWT secret generated and saved to $JWT_SECRET_FILE"
fi

# Generate default admin password if not set
if [ -z "${BOSUN_ADMIN_PASSWORD:-}" ]; then
    BOSUN_ADMIN_PASSWORD="$(openssl rand -base64 12)"
    ADMIN_PASSWORD_GENERATED="true"
else
    ADMIN_PASSWORD_GENERATED="false"
fi

# Generate MCP API key for LLM agent authentication
header "Step 12b/16: Generating MCP API key"
if [ -f "$MCP_API_KEY_FILE" ]; then
    info "MCP API key already exists at $MCP_API_KEY_FILE"
else
    info "Generating random MCP API key..."
    openssl rand -hex 32 > "$MCP_API_KEY_FILE"
    chmod 0600 "$MCP_API_KEY_FILE"
    chown "${BOSUN_USER}:${BOSUN_USER}" "$MCP_API_KEY_FILE" 2>/dev/null || true
    info "MCP API key generated and saved to $MCP_API_KEY_FILE"
fi

# ── Step 13: Create systemd service ────────────────────────────────────────────
header "Step 13/16: Creating systemd service"

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
After=network-online.target docker.service${WITH_GATEWAY:+ apisix.service}
Wants=network-online.target docker.service${WITH_GATEWAY:+ apisix.service}
Requires=docker.service

[Service]
Type=simple
User=${BOSUN_USER}
Group=${BOSUN_USER}
ExecStart=${BOSUN_BIN_DIR}/bosun-daemon \\
    --listen ${BOSUN_LISTEN_ADDR} \\
    --data-dir ${BOSUN_DATA_DIR} \\
    --templates-dir ${BOSUN_CONFIG_DIR}/catalog \\
    --cert ${CERT_FILE} \\
    --key ${KEY_FILE} \\
    --jwt-secret \$(cat ${JWT_SECRET_FILE}) \\
    --webhook-listen ${BOSUN_WEBHOOK_LISTEN} \\
    --webhook-secret \\$(cat ${WEBHOOK_SECRET_FILE}) \\
    --mcp-listen 127.0.0.1:9092 \\
    --mcp-api-key \\$(cat ${MCP_API_KEY_FILE})
Restart=always
RestartSec=5
Environment=RUST_LOG=${BOSUN_RUST_LOG}
Environment=BOSUN_ADMIN_PASSWORD=${BOSUN_ADMIN_PASSWORD}

# Sandboxing / hardening
ProtectSystem=strict
ProtectHome=yes
NoNewPrivileges=yes
PrivateTmp=yes
ReadWritePaths=${BOSUN_DATA_DIR} ${BOSUN_CACHE_DIR} ${BOSUN_CONFIG_DIR} /var/log/caddy
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

# ── Step 14: Enable and start the service ──────────────────────────────────────
header "Step 14/16: Enabling and starting bosun-daemon"

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

# ── Step 15: Install IDS/IPS (CrowdSec or Fail2Ban) ──────────────────────────
header "Step 15/16: Installing IDS/IPS security engine"

if [ "$WITH_CROWDSEC" = "true" ]; then
    info "CrowdSec requested via --with-crowdsec"
    if command -v cscli &>/dev/null; then
        info "CrowdSec is already installed."
        CS_VERSION="$(cscli version 2>/dev/null || echo 'unknown')"
        info "  Version: $CS_VERSION"
    else
        info "Installing CrowdSec from official repository..."

        # Add CrowdSec repository
        curl -s https://packagecloud.io/install/repositories/crowdsec/crowdsec/script.deb.sh | bash

        # Install CrowdSec + firewall bouncer
        apt-get install -y -qq crowdsec crowdsec-firewall-bouncer-iptables

        # Enable and start CrowdSec
        systemctl enable crowdsec.service
        systemctl start crowdsec.service

        # Configure CrowdSec to read Caddy logs
        CROWD_SEC_ACQUIS_DIR="/etc/crowdsec/acquis.d"
        mkdir -p "$CROWD_SEC_ACQUIS_DIR"

        cat > "$CROWD_SEC_ACQUIS_DIR/caddy.yaml" << 'CROWDSECEOF'
# Bosun-managed: CrowdSec reads Caddy logs
filenames:
  - /var/log/caddy/*.log
labels:
  type: caddy
  bosun_managed: "true"
CROWDSECEOF

        # Reload CrowdSec
        cscli hub update 2>/dev/null || true
        cscli hub upgrade 2>/dev/null || true
        systemctl reload crowdsec 2>/dev/null || systemctl restart crowdsec

        info "CrowdSec installed and configured to monitor Caddy logs."
    fi
else
    info "CrowdSec not requested. Installing Fail2Ban as lightweight fallback..."
    if command -v fail2ban-client &>/dev/null; then
        info "Fail2Ban is already installed."
        F2B_VERSION="$(fail2ban-client version 2>/dev/null || echo 'unknown')"
        info "  Version: $F2B_VERSION"
    else
        apt-get install -y -qq fail2ban

        # Enable and start Fail2Ban
        systemctl enable fail2ban.service
        systemctl start fail2ban.service

        # Create a default jail for Caddy HTTP auth failures
        F2B_JAIL_DIR="/etc/fail2ban/jail.d"
        mkdir -p "$F2B_JAIL_DIR"

        cat > "$F2B_JAIL_DIR/bosun-caddy.conf" << 'FAIL2BANEOF'
# Bosun-managed: Fail2Ban monitors Caddy access logs
[bosun-caddy]
enabled = true
filter = bosun-caddy
logpath = /var/log/caddy/access.log
          /var/log/caddy/*.log
maxretry = 5
findtime = 600
bantime = 3600
action = iptables-multiport[name=bosun-caddy, port="80,443", protocol=tcp]
FAIL2BANEOF

        # Create filter for HTTP auth failures
        cat > "/etc/fail2ban/filter.d/bosun-caddy.conf" << 'FAIL2BANFILTEREOF'
# Bosun-managed Fail2Ban filter for Caddy
[Definition]
failregex = ^<HOST> -.*"(GET|POST|PUT|DELETE|PATCH|HEAD|OPTIONS).*" (401|403|429) .*$
ignoreregex =
FAIL2BANFILTEREOF

        # Reload Fail2Ban
        systemctl reload fail2ban 2>/dev/null || systemctl restart fail2ban

        info "Fail2Ban installed and configured to monitor Caddy logs."
    fi
fi

info "Security engine ready — bosun-daemon will auto-detect and configure per-app."

# ── Step 16: Open firewall port ───────────────────────────────────────────────
header "Step 16/16: Configuring firewall"

BOSUN_PORT="${BOSUN_LISTEN_ADDR##*:}"
WEBHOOK_PORT="${BOSUN_WEBHOOK_LISTEN##*:}"

if command -v ufw &>/dev/null && ufw status | grep -q "Status: active"; then
    if ufw status | grep -q "$BOSUN_PORT/tcp"; then
        info "Firewall port $BOSUN_PORT/tcp is already open."
    else
        info "Opening firewall port $BOSUN_PORT/tcp..."
        ufw allow "$BOSUN_PORT/tcp" comment "Bosun gRPC API"
        info "Port $BOSUN_PORT/tcp opened."
    fi

    if ufw status | grep -q "$WEBHOOK_PORT/tcp"; then
        info "Firewall port $WEBHOOK_PORT/tcp is already open."
    else
        info "Opening firewall port $WEBHOOK_PORT/tcp..."
        ufw allow "$WEBHOOK_PORT/tcp" comment "Bosun webhook HTTP API"
        info "Port $WEBHOOK_PORT/tcp opened."
    fi
else
    info "ufw not active or not installed. Skipping firewall configuration."
    warn "If you use a different firewall, ensure ports $BOSUN_PORT/tcp and $WEBHOOK_PORT/tcp are open."
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
echo -e "  ${CYAN}Caddy status:${NC}   systemctl status caddy"
echo -e "  ${CYAN}View logs:${NC}      journalctl -u bosun-daemon -f"
echo -e "  ${CYAN}Caddy logs:${NC}     journalctl -u caddy -f"
echo -e "  ${CYAN}Config dir:${NC}     $BOSUN_CONFIG_DIR"
echo -e "  ${CYAN}Data dir:${NC}       $BOSUN_DATA_DIR"
echo -e "  ${CYAN}Service file:${NC}   $SERVICE_FILE"
echo ""

# Print admin credentials if generated
if [ "$ADMIN_PASSWORD_GENERATED" = "true" ]; then
    echo -e "  ${YELLOW}╔══════════════════════════════════════════════════════════════╗${NC}"
    echo -e "  ${YELLOW}║  DEFAULT ADMIN CREDENTIALS — SAVE THIS!                      ║${NC}"
    echo -e "  ${YELLOW}║                                                              ║${NC}"
    echo -e "  ${YELLOW}║  Username: admin                                             ║${NC}"
    echo -e "  ${YELLOW}║  Password: ${BOSUN_ADMIN_PASSWORD}                                           ║${NC}"
    echo -e "  ${YELLOW}║                                                              ║${NC}"
    echo -e "  ${YELLOW}║  This password will NOT be shown again.                      ║${NC}"
    echo -e "  ${YELLOW}║  Change it immediately after your first login.               ║${NC}"
    echo -e "  ${YELLOW}╚══════════════════════════════════════════════════════════════╝${NC}"
fi

echo ""
echo -e "  ${CYAN}Next steps:${NC}"
echo -e "  1. Install the bosun CLI on your local machine:"
echo -e "     cargo install --git $BOSUN_REPO bosun"
echo ""
echo -e "  2. Authenticate with the daemon:"
echo -e "     export BOSUN_DAEMON=https://$(hostname -I 2>/dev/null | awk '{print $1}' || echo 'YOUR_SERVER_IP'):$BOSUN_PORT"
echo -e "     bosun login admin"
echo -e "     # Enter the admin password shown above"
echo ""
echo -e "  3. Deploy your first app:"
echo -e "     bosun deploy ./my-app --domain my-app.example.com"
echo -e "     # With SSL (requires Caddy):"
echo -e "     bosun deploy ./my-app --domain my-app.example.com --ssl"
echo -e "     # With deploy strategy (direct, rolling, blue-green):"
echo -e "     bosun deploy ./my-app --domain my-app.example.com --strategy rolling"
echo ""
echo -e "  4. Create additional users:"
echo -e "     bosun create-user myuser --password s3cret --role user"
echo ""
echo -e \"  5. Configure git push auto-deploy via webhook:\"
echo -e \"     curl -X POST https://YOUR_SERVER_IP:$WEBHOOK_PORT/hooks/my-app \\\\\"
echo -e \"       -H 'X-Bosun-Secret: YOUR_WEBHOOK_SECRET' \\\\\"
echo -e \"       -H 'X-Bosun-Strategy: rolling' \\\\\"
echo -e \"       -H 'Content-Type: application/json' \\\\\"
echo -e \"       -d '{\\\"ref\\\":\\\"refs/heads/main\\\"}'\"
echo \"\"

if [ \"$WITH_SWARM\" = \"true\" ]; then
    echo -e \"  6. ${CYAN}Docker Swarm Cluster Management:${NC}\"
    echo -e \"     ${CYAN}List nodes:${NC}     bosun cluster nodes\"
    echo -e "     ${CYAN}Join worker:${NC}    bosun cluster join \${TOKEN} \${MANAGER_IP}:2377"
    echo -e \"     ${CYAN}Leave Swarm:${NC}    bosun cluster leave\"
    echo -e "     ${CYAN}Manage nodes:${NC}   docker node ls  (Docker native CLI)"
    echo \"\"
fi
echo -e "     Available strategies via X-Bosun-Strategy header:"
echo -e "       direct      — stop old, start new (simplest)"
echo -e "       rolling     — rolling update via Docker (default for webhooks)"
echo -e "       blue-green  — maintain two environments, switch traffic"
echo ""
echo -e "  ${YELLOW}Note:${NC} A self-signed TLS certificate was generated for gRPC mTLS."
echo -e "  ${YELLOW}Edge SSL (HTTPS for deployed apps) is handled by Caddy via Let's Encrypt.${NC}"

# ── Controller mode next steps ──────────────────────────────────────────
if [ "$AS_CONTROLLER" = "true" ]; then
    echo ""
    echo -e "  ${CYAN}╔══════════════════════════════════════════════════════════════╗${NC}"
    echo -e "  ${CYAN}║  CONTROLLER MODE — Multi-Cloud Orchestration                 ║${NC}"
    echo -e "  ${CYAN}╚══════════════════════════════════════════════════════════════╝${NC}"
    echo ""
    echo -e "  ${CYAN}This node is configured as a multi-cloud controller.${NC}"
    echo -e "  ${CYAN}You can now manage multiple bosun-daemon nodes across VPS/clouds.${NC}"
    echo ""
    echo -e "  ${CYAN}1. Install bosun-daemon on each worker VPS:${NC}"
    echo -e "     curl -fsSL https://raw.githubusercontent.com/rquezada-tech/bosun/main/scripts/install.sh | sudo bash"
    echo ""
    echo -e "  ${CYAN}2. On the controller, register each worker node:${NC}"
    echo -e "     bosun cluster add-node --name vps-2 --addr https://<WORKER_IP>:9090 --label cloud=aws --label region=us-east-1"
    echo -e "     bosun cluster add-node --name vps-3 --addr https://<WORKER_IP>:9090 --label cloud=digitalocean --label region=nyc3"
    echo ""
    echo -e "  ${CYAN}3. View your cluster:${NC}"
    echo -e "     bosun cluster nodes        # List all nodes (Swarm + Bosun managed)"
    echo -e "     bosun cluster metrics      # Aggregated cluster metrics"
    echo ""
    echo -e "  ${CYAN}4. Deploy to a specific node:${NC}"
    echo -e "     bosun deploy ./my-app --node vps-2 --domain myapp.example.com"
    echo ""
    echo -e "  ${CYAN}5. Share controller CA with worker nodes for mTLS:${NC}"
    echo -e "     scp ${BOSUN_CONFIG_DIR}/controller-ca.crt user@worker:/etc/bosun/"
    echo ""
    echo -e "  ${CYAN}Controller certs:${NC}"
    echo -e "    CA:      ${BOSUN_CONFIG_DIR}/controller-ca.crt"
    echo -e "    Client:  ${BOSUN_CONFIG_DIR}/controller-client.crt"
    echo ""
fi

echo ""
