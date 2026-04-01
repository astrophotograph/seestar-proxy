#!/usr/bin/env bash
#
# colo-relay.sh — Set up a UDP relay on a colo/VPS box to forward
# WireGuard traffic to a seestar-proxy over Tailscale.
#
# Architecture:
#   Phone (WireGuard) ──UDP:51820──▶ Colo (public IP)
#                                       │
#                                    Tailscale
#                                       │
#                                       ▼
#                                    Proxy (seestar-proxy --wireguard)
#                                       │
#                                    local network
#                                       │
#                                       ▼
#                                    Seestar telescope
#
# The colo box is a dumb relay — all traffic is encrypted end-to-end
# between the phone and the proxy. The colo cannot decrypt anything.
#
# Prerequisites:
#   - Tailscale installed and running on the colo box
#   - Tailscale installed and running on the proxy machine
#   - seestar-proxy running with --wireguard on the proxy machine
#
# Usage:
#   # Show what would be done:
#   ./colo-relay.sh --proxy-ip 100.64.1.50 --dry-run
#
#   # Set up the relay:
#   sudo ./colo-relay.sh --proxy-ip 100.64.1.50
#
#   # Use socat instead of iptables (no root needed):
#   ./colo-relay.sh --proxy-ip 100.64.1.50 --socat
#
#   # Tear down:
#   sudo ./colo-relay.sh --proxy-ip 100.64.1.50 --teardown
#
#   # Show status:
#   ./colo-relay.sh --status
#
set -euo pipefail

PROXY_IP=""
WG_PORT=51820
MODE="iptables"
DRY_RUN=false
TEARDOWN=false
STATUS=false

usage() {
    cat <<EOF
Usage: $(basename "$0") [OPTIONS]

Set up a UDP relay to forward WireGuard traffic to a seestar-proxy.

Options:
  --proxy-ip IP    Tailscale IP of the proxy machine (required)
  --port PORT      WireGuard UDP port (default: 51820)
  --socat          Use socat instead of iptables (no root needed)
  --dry-run        Show commands without executing
  --teardown       Remove the relay rules
  --status         Show current relay status
  -h, --help       Show this help

Examples:
  # Set up relay (iptables, needs root):
  sudo ./colo-relay.sh --proxy-ip 100.64.1.50

  # Set up relay (socat, no root):
  ./colo-relay.sh --proxy-ip 100.64.1.50 --socat

  # Then configure seestar-proxy:
  seestar-proxy --upstream 10.0.0.1 --wireguard --wg-endpoint <colo-public-ip>:51820
EOF
}

while [[ $# -gt 0 ]]; do
    case $1 in
        --proxy-ip) PROXY_IP="$2"; shift 2 ;;
        --port)     WG_PORT="$2"; shift 2 ;;
        --socat)    MODE="socat"; shift ;;
        --dry-run)  DRY_RUN=true; shift ;;
        --teardown) TEARDOWN=true; shift ;;
        --status)   STATUS=true; shift ;;
        -h|--help)  usage; exit 0 ;;
        *) echo "Unknown option: $1"; usage; exit 1 ;;
    esac
done

run() {
    if $DRY_RUN; then
        echo "[dry-run] $*"
    else
        echo "  $ $*"
        eval "$@"
    fi
}

if $STATUS; then
    echo "=== UDP Relay Status ==="
    echo ""
    echo "Listening on UDP $WG_PORT:"
    ss -ulnp | grep ":${WG_PORT} " || echo "  (nothing listening)"
    echo ""
    echo "iptables NAT rules:"
    sudo iptables -t nat -L PREROUTING -n 2>/dev/null | grep "${WG_PORT}" || echo "  (no rules)"
    echo ""
    echo "socat processes:"
    pgrep -a socat 2>/dev/null | grep "${WG_PORT}" || echo "  (none running)"
    echo ""
    echo "IP forwarding:"
    cat /proc/sys/net/ipv4/ip_forward
    exit 0
fi

if [[ -z "$PROXY_IP" ]]; then
    echo "Error: --proxy-ip is required"
    echo ""
    usage
    exit 1
fi

if $TEARDOWN; then
    echo "Tearing down UDP relay..."
    if [[ "$MODE" == "socat" ]]; then
        run "pkill -f 'socat.*UDP-LISTEN:${WG_PORT}' || true"
    else
        run "iptables -t nat -D PREROUTING -p udp --dport ${WG_PORT} -j DNAT --to-destination ${PROXY_IP}:${WG_PORT} 2>/dev/null || true"
        run "iptables -t nat -D POSTROUTING -p udp -d ${PROXY_IP} --dport ${WG_PORT} -j MASQUERADE 2>/dev/null || true"
    fi
    echo "Done."
    exit 0
fi

echo "=== Seestar Proxy UDP Relay ==="
echo ""
echo "  Mode:       $MODE"
echo "  Listen:     0.0.0.0:$WG_PORT (UDP)"
echo "  Forward to: $PROXY_IP:$WG_PORT (via Tailscale)"
echo ""

if [[ "$MODE" == "socat" ]]; then
    # socat mode — runs in foreground, no root needed.
    if ! command -v socat &>/dev/null; then
        echo "Error: socat not installed. Install it:"
        echo "  apt install socat"
        exit 1
    fi

    echo "Starting socat relay (Ctrl+C to stop)..."
    run "socat -v UDP-LISTEN:${WG_PORT},fork,reuseaddr UDP:${PROXY_IP}:${WG_PORT}"
else
    # iptables mode — needs root, runs in kernel (most efficient).
    if [[ $EUID -ne 0 ]]; then
        echo "Error: iptables mode requires root. Use sudo or --socat."
        exit 1
    fi

    echo "Setting up iptables relay..."

    # Enable IP forwarding.
    run "sysctl -w net.ipv4.ip_forward=1"

    # DNAT incoming WireGuard UDP to the proxy's Tailscale IP.
    run "iptables -t nat -A PREROUTING -p udp --dport ${WG_PORT} -j DNAT --to-destination ${PROXY_IP}:${WG_PORT}"

    # Masquerade outgoing packets so the proxy sees the colo's Tailscale IP
    # as the source (needed for return traffic routing).
    run "iptables -t nat -A POSTROUTING -p udp -d ${PROXY_IP} --dport ${WG_PORT} -j MASQUERADE"

    echo ""
    echo "Relay active. Verify with:"
    echo "  sudo iptables -t nat -L -n | grep ${WG_PORT}"
    echo ""
    echo "To make persistent across reboots, save with:"
    echo "  iptables-save > /etc/iptables/rules.v4"
    echo ""
    echo "To tear down:"
    echo "  sudo $(basename "$0") --proxy-ip ${PROXY_IP} --teardown"
fi

echo ""
echo "Next steps:"
echo "  1. On the proxy machine, run:"
echo "     seestar-proxy --upstream <seestar-ip> --wireguard --wg-endpoint $(hostname -I 2>/dev/null | awk '{print $1}' || echo '<colo-public-ip>'):${WG_PORT}"
echo "  2. Scan the QR code with the WireGuard app on your phone"
echo "  3. Open the Seestar app — it should discover the telescope"
