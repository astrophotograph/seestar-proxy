#!/usr/bin/env bash
#
# pf-redirect.sh — Use macOS pf to transparently redirect Seestar TCP
# traffic through the local proxy.
#
# Architecture:
#   Seestar App ──TCP:4700/4800──▶ pf (kernel) ──redirect──▶ seestar-proxy (127.0.0.1)
#                                                                    │
#                                                                    ▼
#                                                              Seestar telescope
#
# The app thinks it's connecting directly to the Seestar. pf intercepts
# and redirects to the proxy, which forwards upstream. The app never
# knows it's being proxied.
#
# Prerequisites:
#   - macOS (pf is built in)
#   - seestar-proxy running locally on default ports (4700, 4800)
#
# Usage:
#   # Redirect traffic to a specific Seestar IP:
#   sudo ./pf-redirect.sh --seestar-ip 192.168.42.41
#
#   # Dry run (show rules without applying):
#   sudo ./pf-redirect.sh --seestar-ip 192.168.42.41 --dry-run
#
#   # Tear down:
#   sudo ./pf-redirect.sh --teardown
#
#   # Show status:
#   ./pf-redirect.sh --status
#
set -euo pipefail

SEESTAR_IP=""
PROXY_IP="127.0.0.1"
CONTROL_PORT=4700
IMAGING_PORT=4800
ANCHOR_NAME="seestar-proxy"
DRY_RUN=false
TEARDOWN=false
STATUS=false

usage() {
    cat <<EOF
Usage: $(basename "$0") [OPTIONS]

Use macOS pf to redirect Seestar traffic through the local proxy.

Options:
  --seestar-ip IP    IP address of the Seestar telescope (required)
  --proxy-ip IP      Proxy listen address (default: 127.0.0.1)
  --control-port N   Seestar control port (default: 4700)
  --imaging-port N   Seestar imaging port (default: 4800)
  --dry-run          Show rules without applying
  --teardown         Remove redirect rules and restore pf
  --status           Show current pf redirect status
  -h, --help         Show this help

Examples:
  # Set up redirect:
  sudo ./pf-redirect.sh --seestar-ip 192.168.42.41

  # The proxy must be running separately:
  seestar-proxy --upstream 192.168.42.41

  # Tear down when done:
  sudo ./pf-redirect.sh --teardown
EOF
}

while [[ $# -gt 0 ]]; do
    case $1 in
        --seestar-ip)    SEESTAR_IP="$2"; shift 2 ;;
        --proxy-ip)      PROXY_IP="$2"; shift 2 ;;
        --control-port)  CONTROL_PORT="$2"; shift 2 ;;
        --imaging-port)  IMAGING_PORT="$2"; shift 2 ;;
        --dry-run)       DRY_RUN=true; shift ;;
        --teardown)      TEARDOWN=true; shift ;;
        --status)        STATUS=true; shift ;;
        -h|--help)       usage; exit 0 ;;
        *)               echo "Error: unknown option: $1"; usage; exit 1 ;;
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

# --- Status ---
if $STATUS; then
    echo "=== pf Redirect Status ==="
    echo ""
    echo "pf enabled:"
    sudo pfctl -s info 2>/dev/null | head -1 || echo "  (cannot query — need sudo?)"
    echo ""
    echo "Anchor '${ANCHOR_NAME}' rules:"
    sudo pfctl -a "${ANCHOR_NAME}" -s nat 2>/dev/null || echo "  (no rules or not loaded)"
    echo ""
    echo "Proxy listening:"
    lsof -nP -iTCP:4700 -sTCP:LISTEN 2>/dev/null | head -5 || echo "  (nothing on 4700)"
    lsof -nP -iTCP:4800 -sTCP:LISTEN 2>/dev/null | head -5 || echo "  (nothing on 4800)"
    exit 0
fi

# --- Teardown ---
if $TEARDOWN; then
    echo "Tearing down pf redirect..."

    # Flush the anchor rules.
    run "pfctl -a '${ANCHOR_NAME}' -F all"

    # Remove the anchor reference from the main ruleset if we added it.
    if pfctl -s nat 2>/dev/null | grep -q "${ANCHOR_NAME}"; then
        echo "Note: anchor reference remains in main pf.conf."
        echo "It's harmless (empty anchor), but you can remove it manually:"
        echo "  Remove 'rdr-anchor \"${ANCHOR_NAME}\"' from /etc/pf.conf"
    fi

    echo "Done. Redirect rules removed."
    echo ""
    echo "To fully disable pf (if it was off before):"
    echo "  sudo pfctl -d"
    exit 0
fi

# --- Setup ---
if [[ -z "$SEESTAR_IP" ]]; then
    echo "Error: --seestar-ip is required"
    echo ""
    usage
    exit 1
fi

if [[ $EUID -ne 0 ]] && ! $DRY_RUN; then
    echo "Error: pf requires root. Use sudo."
    exit 1
fi

echo "=== Seestar pf Redirect ==="
echo ""
echo "  Seestar:  $SEESTAR_IP"
echo "  Proxy:    $PROXY_IP (ports $CONTROL_PORT, $IMAGING_PORT)"
echo "  Anchor:   $ANCHOR_NAME"
echo ""

# Build the anchor rules.
RULES=$(cat <<EOF
rdr pass on lo0 proto tcp from any to ${SEESTAR_IP} port ${CONTROL_PORT} -> ${PROXY_IP} port ${CONTROL_PORT}
rdr pass on lo0 proto tcp from any to ${SEESTAR_IP} port ${IMAGING_PORT} -> ${PROXY_IP} port ${IMAGING_PORT}
rdr pass on en0 proto tcp from any to ${SEESTAR_IP} port ${CONTROL_PORT} -> ${PROXY_IP} port ${CONTROL_PORT}
rdr pass on en0 proto tcp from any to ${SEESTAR_IP} port ${IMAGING_PORT} -> ${PROXY_IP} port ${IMAGING_PORT}
EOF
)

echo "Rules:"
echo "$RULES"
echo ""

if $DRY_RUN; then
    echo "[dry-run] Would load these rules into anchor '${ANCHOR_NAME}'"
    exit 0
fi

# Ensure the anchor exists in the main pf config.
if ! pfctl -s nat 2>/dev/null | grep -q "${ANCHOR_NAME}"; then
    echo "Adding anchor to main pf ruleset..."
    # We need to add rdr-anchor to the existing config.
    # Read current nat rules from pf, prepend our anchor.
    {
        echo "rdr-anchor \"${ANCHOR_NAME}\""
        pfctl -s nat 2>/dev/null
    } | run "pfctl -f -"
fi

# Load rules into the anchor.
echo "Loading redirect rules..."
echo "$RULES" | run "pfctl -a '${ANCHOR_NAME}' -f -"

# Enable pf if not already enabled.
if ! pfctl -s info 2>/dev/null | grep -q "Status: Enabled"; then
    echo "Enabling pf..."
    run "pfctl -e"
fi

echo ""
echo "Redirect active. Verify with:"
echo "  sudo pfctl -a '${ANCHOR_NAME}' -s nat"
echo ""
echo "To tear down:"
echo "  sudo $(basename "$0") --teardown"
echo ""
echo "Make sure seestar-proxy is running:"
echo "  seestar-proxy --upstream ${SEESTAR_IP}"
