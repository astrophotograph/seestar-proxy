# Seestar Proxy — Architecture & Project Overview

## Overview

Seestar Proxy is a TCP proxy and multiplexer written in Rust that enables multiple clients to simultaneously communicate with a single [Seestar](https://www.zwoastro.com/product/seestar/) smart telescope. Without the proxy, the telescope only accepts one connection at a time. The proxy intercepts all client connections, multiplexes traffic, and routes responses back to the correct originating client.

Secondary goals include Lua scripting hooks for filtering/modifying traffic, an embedded HTTP dashboard for live monitoring, WireGuard remote access, and traffic recording for offline development.

---

## High-Level Architecture

```
┌─────────────┐        ┌───────────────────────────────────┐        ┌─────────────┐
│  Client A   │──────▶ │                                   │        │             │
│  Client B   │──────▶ │         Seestar Proxy             │──────▶ │  Telescope  │
│  Client C   │──────▶ │                                   │        │             │
└─────────────┘        └───────────────────────────────────┘        └─────────────┘
                         Port 4700  JSON-RPC (control)
                         Port 4800  Binary frames (imaging)
                         Port 4720  UDP discovery (optional)
                         Port 4090  HTTP dashboard (optional)
```

The proxy exposes the same ports as the telescope itself, so existing clients require no configuration changes—they simply connect to the proxy's IP address instead of the telescope's.

---

## Module Structure

```
src/
├── main.rs         Entry point, CLI parsing, task orchestration, graceful shutdown
├── lib.rs          Re-exports all modules (used for integration tests)
├── config.rs       CLI arguments, TOML config file, environment variables
├── protocol.rs     Protocol constants, frame header parsing, JSON-RPC helpers
├── control.rs      JSON-RPC multiplexer (port 4700)
├── imaging.rs      Binary frame fan-out (port 4800)
├── discovery.rs    UDP discovery bridge (port 4720)
├── recorder.rs     Optional traffic recording to disk
├── dashboard.rs    HTTP status dashboard (port 4090)
├── hooks.rs        Lua scripting engine (--hook)
├── metrics.rs      Shared runtime counters read by the dashboard
├── transparent.rs  SO_ORIGINAL_DST resolution for iptables-transparent mode (Linux)
├── ntp.rs          NTP clock sync for headless devices (feature = "ntp")
├── wireguard.rs    Embedded WireGuard endpoint (feature = "wireguard")
└── tailscale.rs    Embedded Tailscale node (feature = "tailscale")
```

---

## Components

### `main.rs` — Entry Point

Parses CLI arguments (via `config::Config::load()`), optionally syncs the system clock via NTP, configures logging, and spawns independent Tokio tasks:

- **Control proxy** — multiplexes JSON-RPC on port 4700
- **Imaging proxy** — fans out binary frames on port 4800
- **Discovery bridge** — optionally bridges UDP discovery on port 4720
- **HTTP dashboard** — serves live stats and traffic log on port 4090
- **WireGuard endpoint** — optional embedded VPN (feature = "wireguard")
- **Tailscale node** — optional embedded VPN (feature = "tailscale")

On `Ctrl+C`, the main task cancels all children and finalizes any active recording session.

---

### `config.rs` — Configuration

Configuration is resolved in this precedence order (highest to lowest):

1. CLI flags
2. TOML config file (`--config`, or auto-detected from `~/.config/seestar-proxy/config.toml` / `/etc/seestar-proxy/config.toml`)
3. Environment variables (`SEESTAR_*`)
4. Built-in defaults

| Flag | Env var | Default | Description |
|------|---------|---------|-------------|
| `-u, --upstream <HOST>` | `SEESTAR_UPSTREAM` | `seestar.local` | Telescope IP address or hostname |
| `-b, --bind <IP>` | `SEESTAR_BIND` | `0.0.0.0` | Local address to listen on |
| `--control-port <N>` | `SEESTAR_CONTROL_PORT` | `4700` | Local JSON-RPC port |
| `--imaging-port <N>` | `SEESTAR_IMAGING_PORT` | `4800` | Local imaging port |
| `--upstream-control-port <N>` | `SEESTAR_UPSTREAM_CONTROL_PORT` | `4700` | Telescope control port |
| `--upstream-imaging-port <N>` | `SEESTAR_UPSTREAM_IMAGING_PORT` | `4800` | Telescope imaging port |
| `-d, --discovery` | `SEESTAR_DISCOVERY` | off | Enable UDP discovery bridging |
| `-r, --record <DIR>` | `SEESTAR_RECORD` | — | Record traffic to a session directory |
| `--raw` | — | off | Raw pipe mode (transparent forwarding, single client) |
| `--transparent` | `SEESTAR_TRANSPARENT` | off | Resolve upstream from `SO_ORIGINAL_DST` (Linux iptables) |
| `--dashboard-port <N>` | `SEESTAR_DASHBOARD_PORT` | `4090` | HTTP dashboard port (0 = disable) |
| `--hook <PATH>` | — | — | Lua hook script (repeatable) |
| `--wireguard` | `SEESTAR_WIREGUARD` | off | Enable WireGuard tunnel endpoint |
| `--wg-port <N>` | `SEESTAR_WG_PORT` | `51820` | WireGuard UDP listen port |
| `--wg-subnet <CIDR>` | `SEESTAR_WG_SUBNET` | `10.99.0.0/24` | WireGuard tunnel subnet |
| `--wg-key-file <PATH>` | `SEESTAR_WG_KEY_FILE` | `~/.seestar-proxy/wg.key` | WireGuard key file |
| `--wg-endpoint <HOST:PORT>` | `SEESTAR_WG_ENDPOINT` | auto-detect | External endpoint for client config |
| `--tailscale` | `SEESTAR_TAILSCALE` | off | Enable embedded Tailscale node |
| `--ts-hostname <NAME>` | `SEESTAR_TS_HOSTNAME` | `seestar-proxy` | Tailscale node name |
| `--ts-authkey <KEY>` | `SEESTAR_TS_AUTHKEY` | — | Tailscale auth key (headless setup) |
| `--ntp-sync` | `SEESTAR_NTP_SYNC` | off | Sync system clock via NTP before start |
| `--ntp-server <HOST>` | `SEESTAR_NTP_SERVER` | `pool.ntp.org` | NTP server to query |
| `-v, --verbose` | — | info | Increase log verbosity (repeat for debug/trace) |
| `-c, --config <PATH>` | `SEESTAR_CONFIG` | — | Path to TOML config file |

---

### `protocol.rs` — Protocol Definitions

Defines both wire formats used by the Seestar.

**Binary frame format (port 4800)**

Each frame consists of an 80-byte big-endian header followed by a variable-length payload:

```
Offset  Size  Field
──────  ────  ─────────────────────────────
6       4     payload size (bytes)
14      1     control code
15      1     frame type ID (20=view, 21=preview, 23=stack)
16      2     image width
18      2     image height
(remaining bytes are padding)
```

**JSON-RPC format (port 4700)**

Messages are JSON objects delimited by `\r\n`. The module provides helpers for:
- Extracting and remapping request IDs
- Distinguishing requests, responses, and async events
- Extracting method names

---

### `control.rs` — JSON-RPC Multiplexer

The most stateful component. It maintains a single upstream connection to the telescope while allowing many clients to issue concurrent requests.

**The core problem:** two clients may use the same request ID (e.g. both send `{"id": 1, ...}`). If both are forwarded verbatim, the telescope's response with `id: 1` is ambiguous.

**Solution — ID remapping:**

1. Client sends a request with its own ID (e.g. `1`).
2. The proxy atomically generates a globally unique ID (starting at `10000`).
3. The request is rewritten with the new ID and forwarded to the telescope.
4. The original ID and the client's response channel are stored in a `ControlState` map keyed by the remapped ID.
5. When the telescope responds, the proxy looks up the remapped ID, restores the original ID, and delivers the response to the correct client.
6. Async events (no `id` field) are broadcast to all connected clients.
7. Each message is also passed through the `HookEngine` before forwarding (if hooks are loaded).

**Internal task layout:**

```
control::run()
├── upstream_writer_task     reads from an mpsc channel, writes to telescope
├── upstream_reader_task     reads from telescope, routes responses or broadcasts events
└── handle_client()          one task per connected client
    └── response_writer_task forwards queued responses back to the client
```

**Synchronization:**

- `Arc<Mutex<ControlState>>` — shared pending-request map
- `mpsc::channel` — client → upstream writer queue
- `mpsc::channel` (per client) — upstream reader → individual client
- `broadcast::channel` — async events to all clients
- `AtomicU64` — lock-free unique ID counter

---

### `imaging.rs` — Binary Frame Fan-Out

Simpler than the control path because imaging is unidirectional (telescope → clients only) and stateless.

**Design:**

1. A single upstream reader task reads frames from the telescope in a loop.
2. Each frame's 80-byte header is parsed to determine payload size.
3. The full frame (header + payload) is wrapped in an `Arc` for zero-copy sharing.
4. The frame is sent on a `broadcast::channel` (capacity 32).
5. Each connected client has a task that receives from the broadcast and writes to the client socket.

Frames larger than 50 MB are rejected as a sanity check. Late-joining clients miss frames that were broadcast before they subscribed—this is intentional, as imaging clients are expected to be continuously receiving.

---

### `discovery.rs` — UDP Discovery Bridge

Enables automatic discovery so clients find the proxy instead of the telescope directly.

**Operation:**

1. On startup, the bridge sends a `scan_iscope` UDP probe to the telescope.
2. The response (device info JSON) is cached.
3. The bridge listens on UDP port 4720 for client discovery probes.
4. On each client probe, the cached device info is returned with the telescope's IP address replaced by the proxy's address.

If the initial probe times out, a minimal fallback response is returned. The cache is not refreshed during the proxy's lifetime.

---

### `recorder.rs` — Traffic Recorder

When `--record <dir>` is specified, all traffic is written to disk in a format compatible with a mock telescope for offline development.

**Output layout:**

```
<session_dir>/
  manifest.json        metadata: frame counts, duration, start/end timestamps
  control.jsonl        one JSON object per line; each has timestamp, direction, message
  frames/
    frame_0000_preview.bin
    frame_0001_stack.bin
    ...
```

Only frames with image data (type IDs 20, 21, 23) are saved; handshake frames are skipped. The recorder uses a `Mutex` around the control log file and `AtomicU32` counters for frame indices. The manifest is written on finalization (graceful shutdown).

---

### `dashboard.rs` — HTTP Status Dashboard

Serves a live status page on port 4090 (configurable via `--dashboard-port`, disable with `0`).

**Routes:**

| Route | Description |
|-------|-------------|
| `GET /` | HTML dashboard with real-time stats |
| `GET /api/stream` | Server-Sent Events — stats + traffic log at 1 Hz |
| `GET /api/stats` | JSON snapshot (one-shot poll) |

The dashboard reads from `Arc<Metrics>` and, if WireGuard is enabled, displays the peer config as a QR code. It uses [axum](https://github.com/tokio-rs/axum) for routing.

---

### `metrics.rs` — Runtime Metrics

Shared atomic counters updated by `control.rs` and `imaging.rs` and read by the dashboard. Also maintains a bounded ring buffer of recent traffic log entries for the dashboard's live log view.

Key counters: `control_rx`, `control_tx`, `control_events`, `imaging_frames`, `imaging_bytes`, `control_clients`, `imaging_clients`, `upstream_control_up`, `upstream_imaging_up`.

---

### `hooks.rs` — Lua Scripting Engine

Loads one or more Lua scripts (via `--hook <path>`) into a shared `HookEngine`. Scripts can intercept traffic at several points in the proxy pipeline.

The engine checks at load time which hook functions are defined and skips calls for undefined hooks with no overhead. A runtime error in a hook is logged as a warning and the message is forwarded unchanged — a buggy script never crashes or blocks the proxy.

See [doc/hooks.md](hooks.md) for the full Lua API reference.

---

### `transparent.rs` — Transparent Proxy (Linux)

Extracts the original destination address from a TCP connection that was redirected by an iptables `REDIRECT` rule, using `getsockopt(SOL_IP, SO_ORIGINAL_DST)`. Enabled via `--transparent`.

This allows the proxy to intercept traffic from devices that connect directly to the telescope's IP without any client-side reconfiguration. Linux-only; returns `None` on other platforms.

---

### `ntp.rs` — NTP Clock Sync (feature = "ntp")

Queries an NTP server via plain UDP and sets the system clock if the offset exceeds a threshold (5 seconds). Runs once at startup before any proxy tasks are spawned.

Useful for headless Raspberry Pi devices that lack a real-time clock and may boot with a significantly wrong clock, which would cause TLS certificate validation failures and incorrect log timestamps.

---

### `wireguard.rs` — WireGuard Endpoint (feature = "wireguard")

Embeds a WireGuard VPN endpoint so the proxy is reachable from any network. On startup, a keypair is generated (or loaded from `--wg-key-file`), and a peer config + QR code are printed to the terminal and displayed in the dashboard.

---

### `tailscale.rs` — Tailscale Node (feature = "tailscale")

Embeds a Tailscale node so the proxy joins a tailnet and becomes reachable via Tailscale addresses. Configured via `--tailscale`, `--ts-hostname`, `--ts-authkey`.

---

## Concurrency Model

The proxy is fully async using [Tokio](https://tokio.rs/). All I/O is non-blocking and tasks communicate through channels rather than shared locks wherever possible. Locks (`Mutex`) are used only where shared mutable state is unavoidable (the pending-request map, recorder file handles, and the Lua engine).

```
main()
├── control::run()
│   ├── upstream_writer_task
│   ├── upstream_reader_task
│   └── handle_client()  ×N clients
│       └── response_writer_task
│
├── imaging::run()
│   ├── upstream_reader_task
│   └── handle_client()  ×N clients
│
├── discovery::run()
│
└── dashboard::run()
```

A single client failure (disconnect, bad data) is logged and the client task exits cleanly—it does not affect other clients or the upstream connection. Upstream connection loss terminates the affected proxy task (control or imaging) but not the entire process.

---

## Build & Deployment

The release profile is tuned for the Raspberry Pi: size-optimized (`opt-level = "s"`), LTO enabled, debug symbols stripped, and single codegen unit for maximum optimization.

```bash
# Development
cargo build

# Raspberry Pi / production
cargo build --release
# Binary: target/release/seestar-proxy

# Without WireGuard (smaller binary)
cargo build --release --no-default-features

# Cross-compile for Pi (requires cross)
cross build --release --target aarch64-unknown-linux-musl
cross build --release --target armv7-unknown-linux-musleabihf
```

**Feature flags:**

| Feature | Default | Description |
|---------|---------|-------------|
| `wireguard` | on | Embedded WireGuard VPN endpoint |
| `ntp` | off | NTP clock sync at startup |
| `tailscale` | off | Embedded Tailscale node |

**Example invocation:**

```bash
seestar-proxy --upstream 192.168.1.50 --discovery --record ./sessions/
```

---

## Key Design Decisions

**ID remapping over a single shared connection**
Rather than opening a new upstream connection per client (simple but resource-heavy), the proxy reuses one upstream TCP connection and uses ID remapping to correlate responses. This keeps upstream load minimal and avoids re-authentication per client.

**Broadcast for imaging, point-to-point for control**
Imaging frames are identical for all clients and can be discarded if a client is slow, making a broadcast channel the natural fit. Control responses are targeted and must not be delivered to the wrong client, requiring per-client channels.

**Arc-wrapped frames**
Binary frames can be large (up to tens of MB). Wrapping them in `Arc` lets the broadcast distribute them without copying—each client holds a reference to the same allocation.

**Discovery address substitution**
Clients use UDP discovery to find the telescope. By intercepting and rewriting discovery responses, the proxy makes itself transparent—clients connect to the proxy without knowing it exists.

**Lua hooks for extensibility without recompilation**
Rather than hard-coding filtering rules, the proxy exposes a Lua scripting interface. Users can block commands, modify messages, throttle polling, or inject synthetic responses without touching the proxy source code. Hooks are optional and have zero overhead when not loaded.

**Shared metrics via atomics**
All counters in `metrics.rs` use atomics (`AtomicU64`, `AtomicBool`, etc.) so the dashboard can read them without taking locks, keeping dashboard reads cheap even under heavy traffic.
