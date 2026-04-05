# seestar-proxy

TCP proxy and multiplexer for [Seestar](https://www.zwoastro.com/seestar/) smart telescopes.

Multiple clients can share a single Seestar through the proxy. The proxy handles JSON-RPC request/response routing (port 4700), binary imaging frame fan-out (port 4800), and UDP discovery bridging (port 4720).

## Features

- **Multiplexing** — multiple apps connect to one telescope simultaneously
- **Discovery bridging** — responds to `scan_iscope` UDP broadcasts so apps auto-discover the proxy
- **WireGuard tunnel** — embedded VPN endpoint for remote access from any network
- **Web dashboard** — real-time stats, traffic log, and WireGuard QR code at `http://proxy:4090/`
- **Lua scripting** — filter, modify, or log messages with hook scripts
- **Traffic recording** — capture sessions for replay and development
- **Tiny binary** — ~2 MB static binary, runs on Raspberry Pi Zero and up

## Quick Start

Download the latest release for your platform:

```bash
# Linux x86_64
curl -fSL -o seestar-proxy \
  https://github.com/astrophotograph/seestar-proxy/releases/latest/download/seestar-proxy-x86_64-unknown-linux-musl
chmod +x seestar-proxy

# Raspberry Pi 4/5 (64-bit)
curl -fSL -o seestar-proxy \
  https://github.com/astrophotograph/seestar-proxy/releases/latest/download/seestar-proxy-aarch64-unknown-linux-musl
chmod +x seestar-proxy

# Raspberry Pi Zero/2/3 (32-bit)
curl -fSL -o seestar-proxy \
  https://github.com/astrophotograph/seestar-proxy/releases/latest/download/seestar-proxy-armv7-unknown-linux-musleabihf
chmod +x seestar-proxy
```

Run it:

```bash
./seestar-proxy --upstream 192.168.42.41 --discovery
```

The web dashboard is at `http://localhost:4090/`.

## First-Time App Setup

### Hotspot / same-network setup (non-iOS, or iOS via AP)

The Seestar app remembers the IP address it first connects to and reuses it in future sessions. To redirect the app through the proxy, it must connect to the proxy's IP **before** it ever connects directly to the scope. Follow this order:

1. **Turn the scope OFF** — if the scope is reachable, the app may connect directly and skip the proxy.
2. **Start the proxy** — with `--discovery` enabled so the app finds the proxy via UDP broadcast.
3. **Open the Seestar app** — it will discover the proxy (instead of the scope) and connect to the proxy's IP.
4. **Turn the scope ON** — the proxy detects it coming online and establishes the upstream connection automatically.

Once the app has connected through the proxy at least once, it remembers the proxy's IP and reconnects automatically on subsequent launches — no special startup order needed after that.

### iOS app on a home network (proxy on Raspberry Pi)

The iOS Seestar app has two connection modes:

- **Direct AP mode** — connects via Bluetooth, then switches your phone's WiFi to the telescope's own hotspot (`S50_XXXXXXXX`). This is the default first-time pairing flow.
- **Home network mode** — once a telescope is paired, the app can connect over your home WiFi via TCP without switching networks.

The proxy enables home network mode. **First-time pairing requires the direct AP flow** — do this once with the telescope directly (without the proxy). After that, start the proxy and the app will connect over the home network:

```bash
./seestar-proxy --upstream 192.168.1.123 --discovery
```

For faster startup, provide the telescope's serial number to skip the discovery probe (avoids a ~10 second delay at launch):

```bash
./seestar-proxy --upstream 192.168.1.123 --discovery --telescope-sn 4ddb0535
```

The serial number is the hex suffix of the telescope's WiFi AP name — if the AP is `S50_4ddb0535`, the serial number is `4ddb0535`.

### Troubleshooting

**App connects directly to the scope instead of the proxy**
The app has the telescope's IP cached. Power off the scope, clear the app's device history (or reinstall), then follow the startup order above.

**App does not discover the proxy**
Confirm `--discovery` is passed and that the proxy's `--bind` IP is on the same subnet as the app's device. Check that UDP port 4720 is not blocked by a firewall on the proxy host.

**App connects to proxy but imaging/control don't work**
The scope may not be on yet. Turn it on — the proxy reconnects upstream automatically within a few seconds.

## Raspberry Pi Hotspot

Turn a Pi into a dedicated Seestar WiFi hotspot with one command:

```bash
curl -fSL https://raw.githubusercontent.com/astrophotograph/seestar-proxy/main/scripts/pi-hotspot-setup.sh | sudo bash -s -- --seestar-ip 192.168.42.41
```

Or download and customize:

```bash
curl -fSL -o pi-setup.sh https://raw.githubusercontent.com/astrophotograph/seestar-proxy/main/scripts/pi-hotspot-setup.sh
chmod +x pi-setup.sh
sudo ./pi-setup.sh --seestar-ip 192.168.42.41 --ssid MySeestar --password mypassword
```

This installs the proxy, sets up a WiFi hotspot (hostapd + dnsmasq), configures NAT, and creates a systemd service. Connect your phone or laptop to the hotspot WiFi and open the Seestar app.

See `scripts/pi-hotspot-setup.sh --help` for all options.

## CLI Reference

```
seestar-proxy [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `-u, --upstream <IP>` | seestar.local | Seestar telescope IP or hostname |
| `-b, --bind <IP>` | 0.0.0.0 | Address to bind the proxy on |
| `--control-port <N>` | 4700 | Local JSON-RPC control port |
| `--imaging-port <N>` | 4800 | Local binary imaging port |
| `--upstream-control-port <N>` | 4700 | Seestar control port |
| `--upstream-imaging-port <N>` | 4800 | Seestar imaging port |
| `-d, --discovery` | off | Enable UDP discovery bridging (port 4720) |
| `-r, --record <DIR>` | — | Record traffic to a session directory |
| `--raw` | off | Raw pipe mode (transparent forwarding, single client) |
| `--dashboard-port <N>` | 4090 | Web dashboard port (0 to disable) |
| `--hook <PATH>` | — | Lua hook script (repeatable) |
| `--wireguard` | off | Enable WireGuard tunnel endpoint |
| `--wg-port <N>` | 51820 | WireGuard UDP listen port |
| `--wg-subnet <CIDR>` | 10.99.0.0/24 | WireGuard tunnel subnet |
| `--wg-key-file <PATH>` | ~/.seestar-proxy/wg.key | WireGuard key file |
| `--wg-endpoint <HOST:PORT>` | auto-detect | External endpoint for client config |
| `--telescope-sn <SN>` | — | Telescope serial number for discovery (required for iOS on home network) |
| `--telescope-model <MODEL>` | Seestar S50 | Telescope model name for discovery responses |
| `--telescope-bssid <MAC>` | — | Telescope AP BSSID for discovery responses |
| `-v, --verbose` | info | Increase log verbosity (repeat for debug/trace) |

## WireGuard Remote Access

Access your Seestar from anywhere:

```bash
./seestar-proxy --upstream 192.168.42.41 --discovery --wireguard
```

The proxy prints a QR code to the terminal. Scan it with the WireGuard app on your phone, then open the Seestar app — it discovers the telescope through the tunnel.

For remote access through a VPS/colo relay, see `scripts/colo-relay.sh`.

## Lua Hooks

Filter, modify, or log telescope commands with Lua scripts:

```bash
./seestar-proxy --upstream 192.168.42.41 --hook my-hooks.lua
```

Hook functions: `on_request`, `on_response`, `on_event`, `on_client_connect`, `on_client_disconnect`.

See [doc/hooks.md](doc/hooks.md) for the full API reference.

## Building from Source

Requires Rust 1.85+ (edition 2024).

```bash
# Development build
cargo build

# Release build (size-optimized)
cargo build --release

# Cross-compile for Raspberry Pi (requires cross: cargo install cross)
cross build --release --target aarch64-unknown-linux-musl
cross build --release --target armv7-unknown-linux-musleabihf

# Without WireGuard (smaller binary)
cargo build --release --no-default-features
```

## Architecture

See [doc/architecture.md](doc/architecture.md) for internals.

## License

GPL-3.0-or-later
