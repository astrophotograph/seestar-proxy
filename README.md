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

Open your Seestar app — it should discover the proxy automatically. The web dashboard is at `http://localhost:4090/`.

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
