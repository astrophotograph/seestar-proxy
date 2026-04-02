#!/usr/bin/env bash
#
# pi-hotspot-setup.sh — Turn a Raspberry Pi into a Seestar proxy hotspot.
#
# Architecture:
#   Phone/Laptop ──WiFi──▶ Pi (hotspot on wlan0)
#                              │
#                          seestar-proxy
#                              │
#                       eth0 / wlan1 / etc.
#                              │
#                              ▼
#                        Seestar telescope
#
# The Pi runs a WiFi hotspot so multiple devices can connect and share
# one Seestar telescope. The proxy handles discovery, multiplexing,
# and optionally WireGuard remote access.
#
# Prerequisites:
#   - Raspberry Pi with Raspberry Pi OS (or any Debian-based distro)
#   - At least one WiFi interface (built-in wlan0)
#   - Upstream connection to Seestar already working (ethernet, USB WiFi, etc.)
#
# Usage:
#   # Show what would be done:
#   sudo ./pi-hotspot-setup.sh --seestar-ip 192.168.42.41 --dry-run
#
#   # Full setup:
#   sudo ./pi-hotspot-setup.sh --seestar-ip 192.168.42.41
#
#   # Custom SSID and password:
#   sudo ./pi-hotspot-setup.sh --seestar-ip 192.168.42.41 --ssid MySeestar --password mypass123
#
#   # Show status:
#   sudo ./pi-hotspot-setup.sh --status
#
#   # Tear down:
#   sudo ./pi-hotspot-setup.sh --teardown
#
set -euo pipefail

# --- Defaults ---
SEESTAR_IP=""
SSID="SeestarProxy"
WIFI_PASS="seestar123"
HOTSPOT_IFACE="wlan0"
UPSTREAM_IFACE="eth0"
HOTSPOT_IP="192.168.4.1"
HOTSPOT_SUBNET="192.168.4.0/24"
DHCP_RANGE_START="192.168.4.2"
DHCP_RANGE_END="192.168.4.20"
DHCP_LEASE="24h"
DASHBOARD_PORT=4090
BINARY_PATH="/usr/local/bin/seestar-proxy"
CONFIG_DIR="/etc/seestar-proxy"
GITHUB_REPO="astrophotograph/seestar-proxy"
TRANSPARENT=false
DRY_RUN=false
TEARDOWN=false
PURGE=false
STATUS=false

# --- Colors ---
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

info()    { echo -e "${BLUE}[info]${NC} $*"; }
success() { echo -e "${GREEN}[ok]${NC} $*"; }
warn()    { echo -e "${YELLOW}[warn]${NC} $*"; }
error()   { echo -e "${RED}[error]${NC} $*" >&2; }

usage() {
    cat <<EOF
Usage: sudo $(basename "$0") [OPTIONS]

Turn a Raspberry Pi into a Seestar proxy WiFi hotspot.

Options:
  --seestar-ip IP       IP address of the Seestar telescope (required for setup)
  --ssid NAME           Hotspot SSID (default: SeestarProxy)
  --password PASS       Hotspot WPA2 password (default: seestar123)
  --hotspot-iface IF    WiFi interface for hotspot (default: wlan0)
  --upstream-iface IF   Interface with Seestar connectivity (default: eth0)
  --dashboard-port N    Web dashboard port (default: 4090, 0 to disable)
  --binary-path PATH    Install path for binary (default: /usr/local/bin/seestar-proxy)
  --dry-run             Show commands without executing
  --teardown            Remove hotspot and proxy configuration
  --purge               With --teardown: also remove binary and user
  --status              Show current status
  -h, --help            Show this help

Examples:
  # Pi connected to Seestar's WiFi on wlan1, hotspot on wlan0:
  sudo ./pi-hotspot-setup.sh --seestar-ip 192.168.42.41 --upstream-iface wlan1

  # Pi and Seestar both on home LAN via ethernet:
  sudo ./pi-hotspot-setup.sh --seestar-ip 192.168.1.50

  # Just check what's running:
  sudo ./pi-hotspot-setup.sh --status
EOF
}

# --- Argument parsing ---
while [[ $# -gt 0 ]]; do
    case $1 in
        --seestar-ip)       SEESTAR_IP="$2"; shift 2 ;;
        --ssid)             SSID="$2"; shift 2 ;;
        --password)         WIFI_PASS="$2"; shift 2 ;;
        --hotspot-iface)    HOTSPOT_IFACE="$2"; shift 2 ;;
        --upstream-iface)   UPSTREAM_IFACE="$2"; shift 2 ;;
        --dashboard-port)   DASHBOARD_PORT="$2"; shift 2 ;;
        --binary-path)      BINARY_PATH="$2"; shift 2 ;;
        --transparent)      TRANSPARENT=true; shift ;;
        --dry-run)          DRY_RUN=true; shift ;;
        --teardown)         TEARDOWN=true; shift ;;
        --purge)            PURGE=true; shift ;;
        --status)           STATUS=true; shift ;;
        -h|--help)          usage; exit 0 ;;
        *) error "Unknown option: $1"; usage; exit 1 ;;
    esac
done

run() {
    if $DRY_RUN; then
        echo -e "  ${YELLOW}[dry-run]${NC} $*"
    else
        echo "  $ $*"
        eval "$@"
    fi
}

# --- Status mode ---
if $STATUS; then
    echo "=== Seestar Proxy Hotspot Status ==="
    echo ""

    echo "Services:"
    for svc in seestar-proxy hostapd dnsmasq; do
        if systemctl is-active "$svc" &>/dev/null; then
            echo -e "  ${GREEN}●${NC} $svc: active"
        elif systemctl is-enabled "$svc" &>/dev/null 2>&1; then
            echo -e "  ${YELLOW}●${NC} $svc: enabled but not running"
        else
            echo -e "  ${RED}●${NC} $svc: inactive"
        fi
    done
    echo ""

    echo "Hotspot interface ($HOTSPOT_IFACE):"
    if ip addr show "$HOTSPOT_IFACE" &>/dev/null; then
        ip -4 addr show "$HOTSPOT_IFACE" | grep inet | awk '{print "  " $2}'
    else
        echo "  (interface not found)"
    fi
    echo ""

    echo "Connected WiFi clients:"
    if command -v hostapd_cli &>/dev/null && systemctl is-active hostapd &>/dev/null; then
        hostapd_cli -i "$HOTSPOT_IFACE" all_sta 2>/dev/null | grep -E "^[0-9a-f]{2}:" || echo "  (none)"
    else
        echo "  (hostapd not running)"
    fi
    echo ""

    echo "DHCP leases:"
    if [[ -f /var/lib/misc/dnsmasq.leases ]]; then
        cat /var/lib/misc/dnsmasq.leases | awk '{printf "  %-15s %s (%s)\n", $3, $4, $2}' || echo "  (none)"
    else
        echo "  (no lease file)"
    fi
    echo ""

    echo "IP forwarding:"
    echo "  $(cat /proc/sys/net/ipv4/ip_forward)"
    echo ""

    echo "NAT rules:"
    iptables -t nat -L POSTROUTING -n 2>/dev/null | grep MASQUERADE || echo "  (no MASQUERADE rules)"
    echo ""

    if [[ -x "$BINARY_PATH" ]]; then
        echo "Binary: $BINARY_PATH"
        echo "  $($BINARY_PATH --version 2>/dev/null || echo 'installed')"
    else
        echo "Binary: not installed"
    fi

    echo ""
    echo "Dashboard: http://${HOTSPOT_IP}:${DASHBOARD_PORT}/"
    exit 0
fi

# --- Teardown mode ---
if $TEARDOWN; then
    echo "=== Tearing down Seestar Proxy Hotspot ==="
    echo ""

    info "Stopping services..."
    run "systemctl stop seestar-proxy 2>/dev/null || true"
    run "systemctl disable seestar-proxy 2>/dev/null || true"
    run "systemctl stop hostapd 2>/dev/null || true"
    run "systemctl disable hostapd 2>/dev/null || true"
    run "systemctl stop dnsmasq 2>/dev/null || true"
    run "systemctl disable dnsmasq 2>/dev/null || true"

    info "Removing configuration files..."
    run "rm -f /etc/systemd/system/seestar-proxy.service"
    run "rm -f /etc/dnsmasq.d/seestar-hotspot.conf"
    run "rm -f /etc/hostapd/hostapd.conf"
    run "systemctl daemon-reload"

    info "Removing iptables rules..."
    run "iptables -t nat -D POSTROUTING -o ${UPSTREAM_IFACE} -j MASQUERADE 2>/dev/null || true"

    info "Removing static IP..."
    # NetworkManager
    if command -v nmcli &>/dev/null; then
        run "nmcli connection delete seestar-hotspot 2>/dev/null || true"
    fi
    # dhcpcd
    if [[ -f /etc/dhcpcd.conf ]]; then
        run "sed -i '/# seestar-proxy hotspot/,/^$/d' /etc/dhcpcd.conf 2>/dev/null || true"
    fi

    if $PURGE; then
        info "Purging binary and user..."
        run "rm -f ${BINARY_PATH}"
        run "rm -rf ${CONFIG_DIR}"
        run "userdel seestar 2>/dev/null || true"
    fi

    success "Teardown complete."
    echo "  You may want to reboot to fully restore network defaults."
    exit 0
fi

# --- Setup mode: validate inputs ---
if [[ $EUID -ne 0 ]] && ! $DRY_RUN; then
    error "This script must be run as root (sudo)."
    exit 1
fi

if [[ -z "$SEESTAR_IP" ]] && ! $TRANSPARENT; then
    error "--seestar-ip is required for setup (or use --transparent)."
    echo ""
    usage
    exit 1
fi

if [[ "$HOTSPOT_IFACE" == "$UPSTREAM_IFACE" ]]; then
    error "Hotspot interface and upstream interface cannot be the same ($HOTSPOT_IFACE)."
    exit 1
fi

if [[ "$WIFI_PASS" == "seestar123" ]]; then
    warn "Using default WiFi password. Consider --password <your-password> for security."
fi

if [[ ${#WIFI_PASS} -lt 8 ]]; then
    error "WiFi password must be at least 8 characters (WPA2 requirement)."
    exit 1
fi

# --- Banner ---
echo "=== Seestar Proxy Hotspot Setup ==="
echo ""
echo "  Seestar IP:       $SEESTAR_IP"
echo "  Hotspot SSID:     $SSID"
echo "  Hotspot interface: $HOTSPOT_IFACE"
echo "  Hotspot IP:       $HOTSPOT_IP"
echo "  Upstream iface:   $UPSTREAM_IFACE"
echo "  Dashboard port:   $DASHBOARD_PORT"
echo "  Binary path:      $BINARY_PATH"
echo ""

# --- Step 1: Check prerequisites ---
check_prerequisites() {
    info "Checking prerequisites..."

    if ! grep -qi 'debian\|raspbian\|ubuntu' /etc/os-release 2>/dev/null; then
        warn "This script is designed for Debian-based systems. Proceeding anyway."
    fi

    if ! ip link show "$HOTSPOT_IFACE" &>/dev/null; then
        error "Hotspot interface $HOTSPOT_IFACE not found."
        echo "  Available interfaces:"
        ip -br link | awk '{print "    " $1}'
        exit 1
    fi

    # Check if hotspot interface is wireless
    if [[ ! -d "/sys/class/net/${HOTSPOT_IFACE}/wireless" ]] && ! iw dev "$HOTSPOT_IFACE" info &>/dev/null 2>&1; then
        error "$HOTSPOT_IFACE does not appear to be a wireless interface."
        exit 1
    fi

    if ! ip link show "$UPSTREAM_IFACE" &>/dev/null; then
        warn "Upstream interface $UPSTREAM_IFACE not found. Continuing anyway (may be brought up later)."
    fi

    success "Prerequisites OK."
}

# --- Step 2: Install packages ---
install_packages() {
    info "Installing packages (hostapd, dnsmasq)..."
    run "apt-get update -qq"
    run "apt-get install -y -qq hostapd dnsmasq iptables"
    success "Packages installed."
}

# --- Step 3: Download and install binary ---
install_binary() {
    if [[ -x "$BINARY_PATH" ]]; then
        info "seestar-proxy already installed at $BINARY_PATH, skipping download."
        return
    fi

    info "Downloading seestar-proxy..."

    local arch
    arch=$(uname -m)
    local target deb_arch
    case "$arch" in
        armv7l|armv6l) target="armv7-unknown-linux-musleabihf"; deb_arch="armhf" ;;
        aarch64)       target="aarch64-unknown-linux-musl"; deb_arch="arm64" ;;
        x86_64)        target="x86_64-unknown-linux-musl"; deb_arch="amd64" ;;
        *)
            error "Unsupported architecture: $arch"
            exit 1
            ;;
    esac

    info "Architecture: $arch → target: $target"

    # Prefer .deb package if dpkg is available (includes systemd unit).
    if command -v dpkg &>/dev/null; then
        local version
        version=$(curl -fsSL "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" \
            | grep '"tag_name"' | sed 's/.*"v\(.*\)".*/\1/')
        if [[ -n "$version" ]]; then
            local deb_name="seestar-proxy_${version}_${deb_arch}.deb"
            local deb_url="https://github.com/${GITHUB_REPO}/releases/latest/download/${deb_name}"
            info "Installing .deb package: $deb_name"
            run "curl -fSL -o '/tmp/${deb_name}' '${deb_url}'"
            run "dpkg -i '/tmp/${deb_name}'"
            run "rm -f '/tmp/${deb_name}'"
            if ! $DRY_RUN; then
                success "Installed via .deb (includes systemd unit)"
            fi
            return
        fi
        warn "Could not determine latest version, falling back to raw binary."
    fi

    # Fallback: download raw binary.
    local binary_name="seestar-proxy-${target}"
    local download_url="https://github.com/${GITHUB_REPO}/releases/latest/download/${binary_name}"

    run "curl -fSL -o '${BINARY_PATH}' '${download_url}'"
    run "chmod +x '${BINARY_PATH}'"

    if ! $DRY_RUN; then
        success "Installed $BINARY_PATH ($(du -h "$BINARY_PATH" | cut -f1))"
    fi
}

# --- Step 4: Create service user ---
create_user() {
    if id seestar &>/dev/null; then
        info "User 'seestar' already exists."
        return
    fi

    info "Creating system user 'seestar'..."
    run "useradd --system --no-create-home --shell /usr/sbin/nologin seestar"
    success "User created."
}

# --- Step 5: Configure hostapd ---
configure_hostapd() {
    info "Configuring hostapd..."

    # Stop hostapd if running (it locks the interface)
    run "systemctl stop hostapd 2>/dev/null || true"

    local conf="/etc/hostapd/hostapd.conf"
    run "mkdir -p /etc/hostapd"

    if ! $DRY_RUN; then
        cat > "$conf" <<EOF
# Seestar Proxy hotspot — managed by pi-hotspot-setup.sh
interface=${HOTSPOT_IFACE}
driver=nl80211
ssid=${SSID}
hw_mode=g
channel=7
wmm_enabled=0
macaddr_acl=0
auth_algs=1
ignore_broadcast_ssid=0
wpa=2
wpa_passphrase=${WIFI_PASS}
wpa_key_mgmt=WPA-PSK
wpa_pairwise=TKIP
rsn_pairwise=CCMP
country_code=US
EOF
        echo "  wrote $conf"
    else
        echo -e "  ${YELLOW}[dry-run]${NC} write $conf"
    fi

    # Set regulatory domain
    if [[ -f /etc/default/crda ]]; then
        run "sed -i 's/^REGDOMAIN=.*/REGDOMAIN=US/' /etc/default/crda"
    fi

    run "systemctl unmask hostapd 2>/dev/null || true"
    run "systemctl enable hostapd"

    success "hostapd configured."
}

# --- Step 6: Configure dnsmasq ---
configure_dnsmasq() {
    info "Configuring dnsmasq..."

    run "systemctl stop dnsmasq 2>/dev/null || true"

    local conf="/etc/dnsmasq.d/seestar-hotspot.conf"

    if ! $DRY_RUN; then
        cat > "$conf" <<EOF
# Seestar Proxy hotspot DHCP — managed by pi-hotspot-setup.sh
interface=${HOTSPOT_IFACE}
bind-interfaces
dhcp-range=${DHCP_RANGE_START},${DHCP_RANGE_END},255.255.255.0,${DHCP_LEASE}
EOF
        echo "  wrote $conf"
    else
        echo -e "  ${YELLOW}[dry-run]${NC} write $conf"
    fi

    run "systemctl enable dnsmasq"

    success "dnsmasq configured."
}

# --- Step 7: Configure networking ---
configure_network() {
    info "Configuring network..."

    # Assign static IP to hotspot interface
    if command -v nmcli &>/dev/null && systemctl is-active NetworkManager &>/dev/null; then
        # NetworkManager is running — use nmcli
        info "Detected NetworkManager, using nmcli..."
        run "nmcli connection delete seestar-hotspot 2>/dev/null || true"
        run "nmcli connection add type wifi ifname ${HOTSPOT_IFACE} con-name seestar-hotspot autoconnect no"
        run "nmcli connection modify seestar-hotspot ipv4.addresses ${HOTSPOT_IP}/24"
        run "nmcli connection modify seestar-hotspot ipv4.method manual"
        run "nmcli connection up seestar-hotspot"
    elif [[ -f /etc/dhcpcd.conf ]]; then
        # dhcpcd — older Raspberry Pi OS
        info "Detected dhcpcd, configuring static IP..."
        # Remove any previous seestar config
        run "sed -i '/# seestar-proxy hotspot/,/^$/d' /etc/dhcpcd.conf"
        if ! $DRY_RUN; then
            cat >> /etc/dhcpcd.conf <<EOF

# seestar-proxy hotspot
interface ${HOTSPOT_IFACE}
    static ip_address=${HOTSPOT_IP}/24
    nohook wpa_supplicant

EOF
            echo "  appended static IP config to /etc/dhcpcd.conf"
        else
            echo -e "  ${YELLOW}[dry-run]${NC} append static IP to /etc/dhcpcd.conf"
        fi
    else
        # Fallback: use ip command directly
        warn "No network manager detected, setting IP directly..."
        run "ip addr flush dev ${HOTSPOT_IFACE}"
        run "ip addr add ${HOTSPOT_IP}/24 dev ${HOTSPOT_IFACE}"
        run "ip link set ${HOTSPOT_IFACE} up"
    fi

    # Enable IP forwarding
    run "sysctl -w net.ipv4.ip_forward=1"
    if ! $DRY_RUN; then
        if ! grep -q '^net.ipv4.ip_forward=1' /etc/sysctl.conf 2>/dev/null; then
            echo "net.ipv4.ip_forward=1" >> /etc/sysctl.conf
            echo "  persisted ip_forward=1 in /etc/sysctl.conf"
        fi
    fi

    # NAT: masquerade hotspot traffic going out the upstream interface
    run "iptables -t nat -C POSTROUTING -o ${UPSTREAM_IFACE} -j MASQUERADE 2>/dev/null || iptables -t nat -A POSTROUTING -o ${UPSTREAM_IFACE} -j MASQUERADE"

    # Transparent proxy: redirect Seestar-bound TCP traffic from hotspot clients to the proxy.
    if $TRANSPARENT; then
        info "Adding iptables REDIRECT rules for transparent proxying..."
        run "iptables -t nat -A PREROUTING -i ${HOTSPOT_IFACE} -p tcp --dport 4700 -j REDIRECT --to-port 4700"
        run "iptables -t nat -A PREROUTING -i ${HOTSPOT_IFACE} -p tcp --dport 4800 -j REDIRECT --to-port 4800"
    fi

    # Persist iptables rules if iptables-persistent is available
    if command -v netfilter-persistent &>/dev/null; then
        run "netfilter-persistent save"
    else
        warn "iptables rules are not persisted across reboots."
        echo "  Install iptables-persistent or add to rc.local:"
        echo "    iptables -t nat -A POSTROUTING -o ${UPSTREAM_IFACE} -j MASQUERADE"
    fi

    success "Network configured."
}

# --- Step 8: Configure systemd service ---
configure_systemd() {
    info "Configuring systemd service..."

    run "mkdir -p ${CONFIG_DIR}"

    # Build ExecStart arguments.
    local exec_args="--bind 0.0.0.0 --discovery --dashboard-port ${DASHBOARD_PORT}"
    if $TRANSPARENT; then
        exec_args="--transparent ${exec_args}"
        if [[ -n "$SEESTAR_IP" ]]; then
            exec_args="--upstream ${SEESTAR_IP} ${exec_args}"
        fi
    else
        exec_args="--upstream ${SEESTAR_IP} ${exec_args}"
    fi

    local service="/etc/systemd/system/seestar-proxy.service"

    # If .deb installed the unit to /lib/systemd/system, create an override
    # with our specific upstream and dashboard settings instead of replacing it.
    local deb_unit="/lib/systemd/system/seestar-proxy.service"
    if [[ -f "$deb_unit" ]] && ! $DRY_RUN; then
        local override_dir="/etc/systemd/system/seestar-proxy.service.d"
        mkdir -p "$override_dir"
        cat > "$override_dir/hotspot.conf" <<EOF
[Service]
ExecStart=
ExecStart=${BINARY_PATH} ${exec_args}
EOF
        echo "  wrote $override_dir/hotspot.conf"
        run "systemctl daemon-reload"
        run "systemctl enable seestar-proxy"
        run "systemctl start seestar-proxy"
        success "seestar-proxy service configured (override)."
        return
    fi

    if ! $DRY_RUN; then
        cat > "$service" <<EOF
[Unit]
Description=Seestar Telescope Proxy
After=network-online.target hostapd.service
Wants=network-online.target

[Service]
Type=simple
User=root
ExecStart=${BINARY_PATH} ${exec_args}
Restart=always
RestartSec=5
StandardOutput=journal
StandardError=journal
SyslogIdentifier=seestar-proxy

[Install]
WantedBy=multi-user.target
EOF
        echo "  wrote $service"
    else
        echo -e "  ${YELLOW}[dry-run]${NC} write $service"
    fi

    run "systemctl daemon-reload"
    run "systemctl enable seestar-proxy"
    run "systemctl start seestar-proxy"

    success "seestar-proxy service started."
}

# --- Main ---
check_prerequisites
install_packages
install_binary
create_user
configure_hostapd
configure_dnsmasq
configure_network
configure_systemd

# Start hostapd and dnsmasq after network is configured
run "systemctl start hostapd"
run "systemctl start dnsmasq"

echo ""
echo "=========================================="
success "Seestar Proxy Hotspot is ready!"
echo "=========================================="
echo ""
echo "  WiFi SSID:     $SSID"
echo "  WiFi Password: $WIFI_PASS"
echo "  Dashboard:     http://${HOTSPOT_IP}:${DASHBOARD_PORT}/"
echo "  Seestar IP:    $SEESTAR_IP"
echo ""
echo "  Connect your phone/laptop to '$SSID' and open the Seestar app."
echo "  The telescope should appear via discovery."
echo ""
echo "  View logs:    journalctl -u seestar-proxy -f"
echo "  Check status: sudo $(basename "$0") --status"
echo "  Tear down:    sudo $(basename "$0") --teardown"
