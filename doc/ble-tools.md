# BLE Tools

Two optional binaries for interacting with the Seestar telescope over Bluetooth LE. Both require
the `bluetooth` feature flag and `libdbus-1-dev` on Linux.

```bash
# Install build dependency (Debian/Ubuntu/Raspberry Pi OS)
sudo apt install -y libdbus-1-dev pkg-config

# Build
cargo build --features bluetooth --bin ble_scan
cargo build --features bluetooth --bin ble_pair
```

---

## ble_scan

Scans for Seestar devices nearby, connects to the first match, and prints the full GATT profile —
every service, every characteristic (with properties and raw values where readable), and every
descriptor. Useful for discovering UUIDs and verifying the BLE stack is working.

```bash
cargo run --bin ble_scan --features bluetooth
```

Filter by device name prefix (default: `S50_`):

```bash
cargo run --bin ble_scan --features bluetooth -- --name S50_4ddb0535
```

**Options**

| Flag | Default | Description |
|------|---------|-------------|
| `--name <PREFIX>` | `S50_` | Device name prefix to match (case-insensitive) |
| `--scan-secs <N>` | `15` | How long to scan before giving up |

---

## ble_pair

Authenticates to the telescope over BLE and optionally configures home WiFi credentials. When
`--ssid` and `--password` are omitted it connects, authenticates, and prints the telescope's
current WiFi state — useful for verifying the tool works before making any changes.

```bash
# Show current WiFi state
cargo run --bin ble_pair --features bluetooth -- \
    --name S50_4ddb0535 --sn 20240529115404

# Configure home WiFi
cargo run --bin ble_pair --features bluetooth -- \
    --name S50_4ddb0535 --sn 20240529115404 \
    --ssid MyNetwork --password MyPassword
```

**Options**

| Flag | Default | Description |
|------|---------|-------------|
| `--name <PREFIX>` | `S50_` | Device name prefix to match |
| `--sn <TOKEN>` | (see below) | BLE session token |
| `--ssid <SSID>` | — | Home WiFi SSID to configure |
| `--password <PWD>` | — | Home WiFi password to configure |
| `--scan-secs <N>` | `15` | BLE scan timeout |
| `--timeout-secs <N>` | `5` | Per-command response timeout |
| `--challenge-wait-secs <N>` | `10` | How long to wait for a spontaneous challenge on connect |

---

## BLE protocol notes

The Seestar uses a single custom GATT service with one characteristic for both writes and
notifications:

| Role | UUID |
|------|------|
| Service | `850e1701-6ecf-49f1-a13f-f5c2d9174f9f` |
| Characteristic | `850e1702-6ecf-49f1-a13f-f5c2d9174f9f` |

Commands are JSON objects written to the characteristic. Responses arrive as NOTIFY notifications.
The fields `ble_method` and `ble_sn` are used instead of the TCP protocol's `method` and `sn`.

### Authentication

The session token (`--sn`) is a timestamp string (e.g. `20240529115404`) that the telescope stores
in `/home/pi/.ZWO/ble.conf`. Once a token is trusted (marked `Y` in that file), subsequent
connections with the same token receive an empty ack instead of a new challenge — no re-signing
needed.

**Getting the session token**

The session token is created when the Seestar iOS app does its initial BLE pairing. There are two
ways to find it:

1. **SSH to the telescope** and read `/home/pi/.ZWO/ble.conf`. The `[SN]` section lists all
   registered tokens.

2. **Capture BLE traffic** with `btmon` while the iOS app connects. The `ble_sn` value is sent
   in plaintext (no BLE-level encryption) and appears in the GATT write to characteristic
   `850e1702-...`.

### WiFi configuration

After authentication, `ble_pair` tries `iscope_set_wifi` then `set_wifi_ssid` with
`{"ssid": "...", "password": "..."}`. Both methods are forwarded by the telescope's `air_ble`
daemon to its local TCP service on port 4700. An empty ack (no `code` or `error` field) indicates
the command was accepted.

The telescope runs `air_ble` as a daemon alongside `bsa_server` (the Broadcom BLE stack). It
operates in bridge mode — simultaneously running its own WiFi AP (`S50_XXXXXXXX`) and connected
to the home network — so BLE pairing works without the telescope needing to switch modes.
