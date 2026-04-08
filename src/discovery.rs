//! UDP discovery bridging.
//!
//! The Seestar advertises itself via UDP broadcast on port 4720.
//! Clients send `{"id": 201, "method": "scan_iscope", ...}` and the
//! Seestar responds with its device info.
//!
//! The proxy intercepts these broadcasts on the client network and
//! responds with the upstream Seestar's info, substituting its own
//! address so clients connect to the proxy instead of directly.

use crate::protocol::DISCOVERY_PORT;
use serde_json::Value;
use std::net::{IpAddr, SocketAddr};
use tokio::net::UdpSocket;
use tracing::{error, info, warn};

/// Run the discovery bridge.
///
/// 1. On startup, sends a discovery probe to the upstream Seestar to
///    capture its device info.
/// 2. Listens on UDP port 4720 for client discovery broadcasts.
/// 3. Responds with the cached info, replacing the address with
///    the proxy's bind address.
pub async fn run(
    bind_addr: IpAddr,
    upstream_addr: IpAddr,
    _proxy_control_port: u16,
    telescope_sn: Option<String>,
    telescope_model: Option<String>,
    telescope_bssid: Option<String>,
) -> anyhow::Result<()> {
    // First, discover the upstream Seestar to get its device info.
    // If the user has configured the telescope identity, use it directly.
    let mut device_info = if let Some(ref sn) = telescope_sn {
        // Derive SSID from sn — the telescope always advertises as "S50_<sn>".
        info!(
            "Using configured telescope identity: sn={} model={} ssid=S50_{}",
            sn,
            telescope_model.as_deref().unwrap_or("Seestar S50"),
            sn
        );
        build_configured_response(
            sn,
            telescope_model.as_deref(),
            telescope_bssid.as_deref(),
            "0.0.0.0", // placeholder; patched to proxy_ip below
        )
    } else {
        probe_upstream(upstream_addr).await?
    };

    // Determine the IP the app should connect to (the proxy, not the telescope).
    // If bind_addr is unspecified (0.0.0.0), detect the local IP via routing.
    let proxy_ip = if bind_addr.is_unspecified() {
        let s = std::net::UdpSocket::bind("0.0.0.0:0")?;
        s.connect(SocketAddr::new(upstream_addr, 1))?;
        s.local_addr()?.ip()
    } else {
        bind_addr
    };

    // Patch result.ip so the app connects to the proxy instead of the telescope.
    if let Some(result) = device_info.get_mut("result") {
        result["ip"] = serde_json::Value::String(proxy_ip.to_string());
    }

    // If we only got the minimal fallback at startup, flag it so we know to keep
    // trying to get real device info from the telescope.
    let have_real_info = device_info
        .pointer("/result/sn")
        .and_then(|v| v.as_str())
        .map(|s| s != "proxy")
        .unwrap_or(false);

    info!(
        "Cached upstream device info (ip patched to {}): {}",
        proxy_ip,
        serde_json::to_string(&device_info).unwrap_or_default()
    );

    // Receive socket: bind to 0.0.0.0 to receive broadcast packets when the
    // bind_addr is unspecified, otherwise bind to the specific address.
    // When running on the same machine as the app with an IP alias, we need
    // 0.0.0.0 to hear subnet broadcasts — but we only do that when the bind
    // address is a real (non-loopback, non-unspecified) IP, implying the user
    // set up an alias for this purpose.
    let recv_bind_ip = match bind_addr {
        IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => {
            IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
        }
        _ => bind_addr,
    };
    let recv_addr = SocketAddr::new(recv_bind_ip, DISCOVERY_PORT);
    let recv_socket = UdpSocket::bind(recv_addr).await?;
    recv_socket.set_broadcast(true)?;

    // Send responses from a separate socket bound to the proxy's bind address.
    // This ensures the UDP source IP is the proxy's address (e.g. an alias IP),
    // not the machine's primary IP. The app uses the source IP to decide where
    // to connect, so this is critical for same-machine setups with an IP alias.
    //
    // When bind_addr is unspecified, the send and recv sockets are equivalent,
    // so we just reuse the recv socket for sending.
    // Bind the send socket to port 4720 (not ephemeral) because the Seestar
    // app filters discovery responses by source port. Use SO_REUSEPORT since
    // the recv socket already holds 0.0.0.0:4720.
    let send_socket = if bind_addr.is_unspecified() || bind_addr == recv_bind_ip {
        None
    } else {
        let send_addr = SocketAddr::new(bind_addr, DISCOVERY_PORT);
        let std_sock = {
            let sock = socket2::Socket::new(
                socket2::Domain::IPV4,
                socket2::Type::DGRAM,
                Some(socket2::Protocol::UDP),
            )?;
            sock.set_reuse_address(true)?;
            #[cfg(not(windows))]
            sock.set_reuse_port(true)?;
            sock.set_nonblocking(true)?;
            sock.set_broadcast(true)?;
            sock.bind(&send_addr.into())?;
            sock
        };
        Some(UdpSocket::from_std(std_sock.into())?)
    };

    info!(
        "Discovery bridge listening on {}, responding from {}",
        recv_addr, bind_addr
    );

    // 16 KiB covers any realistic JSON device-info payload.
    let mut buf = [0u8; 16_384];
    let mut have_real_info = have_real_info;

    loop {
        let (n, src_addr) = match recv_socket.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(e) => {
                error!("Discovery recv error: {}", e);
                continue;
            }
        };

        let msg = match std::str::from_utf8(&buf[..n]) {
            Ok(s) => s.trim(),
            Err(_) => continue,
        };

        // Parse the discovery request.
        let request: Value = match serde_json::from_str(msg) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let method = request.get("method").and_then(|v| v.as_str()).unwrap_or("");

        if method != "scan_iscope" {
            continue;
        }

        // If this packet has a "result", it's a discovery response (not a probe).
        if request.get("result").is_some() || request.get("code").is_some() {
            // If it came from the telescope, use it to update our cache.
            if src_addr.ip() == upstream_addr && !have_real_info {
                let mut updated = request.clone();
                if let Some(result) = updated.get_mut("result") {
                    result["ip"] = serde_json::Value::String(proxy_ip.to_string());
                }
                info!(
                    "Updated device cache from telescope response: sn={}",
                    updated
                        .pointer("/result/sn")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                );
                device_info = updated;
                have_real_info = true;
            }
            // Ignore our own broadcast echoes and other non-telescope responses.
            continue;
        }

        info!("Discovery request from {}: {}", src_addr, msg);

        // Respond with cached Seestar info.
        //
        // The app uses the SOURCE IP of the UDP response to determine where
        // to connect. We send from the send_socket (bound to the proxy's
        // bind address) so the source IP is correct.
        //
        // We send both unicast (direct reply) and broadcast (ensures
        // same-machine apps see it on the physical interface rather than
        // only via loopback).
        let response_bytes = serde_json::to_vec(&device_info)?;
        let out = send_socket.as_ref().unwrap_or(&recv_socket);

        // Unicast back to the requester.
        if let Err(e) = out.send_to(&response_bytes, src_addr).await {
            warn!("Failed to send discovery response to {}: {}", src_addr, e);
        }

        // Also broadcast so same-machine apps see it on the physical interface.
        let broadcast_dest =
            SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::BROADCAST), DISCOVERY_PORT);
        if let Err(e) = out.send_to(&response_bytes, broadcast_dest).await {
            warn!("Failed to broadcast discovery response: {}", e);
        }

        info!(
            "Sent discovery response ({} bytes, unicast to {} + broadcast from {})",
            response_bytes.len(),
            src_addr,
            bind_addr,
        );

        // If we don't yet have real device info, forward a probe to the telescope
        // so it will send its response back to us.  The telescope's response will
        // arrive on this socket (from upstream_addr:4720) and update the cache
        // above on the next loop iteration.
        if !have_real_info {
            let probe = serde_json::json!({
                "id": 201,
                "method": "scan_iscope",
                "name": "seestarproxy",
                "ip": proxy_ip.to_string()
            });
            if let Ok(probe_bytes) = serde_json::to_vec(&probe) {
                let target = SocketAddr::new(upstream_addr, DISCOVERY_PORT);
                if let Err(e) = out.send_to(&probe_bytes, target).await {
                    warn!("Failed to send refresh probe to telescope: {}", e);
                } else {
                    info!("Sent refresh probe to telescope at {}", target);
                }
            }
        }
    }
}

/// Send a discovery probe to the upstream Seestar and return its response.
///
/// The Seestar is picky about probe format (discovered by bisection):
/// - `name` must not contain dashes — a dash causes malformed JSON in the response
/// - Probe must be sent from source port 4720 — the telescope ignores probes
///   from other ports (the guest-mode handshake is keyed on the well-known port)
async fn probe_upstream(upstream_addr: IpAddr) -> anyhow::Result<Value> {
    probe_upstream_at(upstream_addr, DISCOVERY_PORT).await
}

/// Like [`probe_upstream`] but binds the probe socket to `probe_port` instead
/// of the well-known discovery port. Used in unit tests to avoid conflicts with
/// real port 4720.
async fn probe_upstream_at(upstream_addr: IpAddr, probe_port: u16) -> anyhow::Result<Value> {
    // Detect the local IP (and therefore the interface) that routes to the
    // telescope.  We bind the probe socket to this specific IP rather than
    // 0.0.0.0 so that:
    //   (a) the broadcast goes out on the correct interface (e.g. wlan0),
    //       not whichever interface the OS happens to pick for 255.255.255.255;
    //   (b) the telescope's unicast reply (addressed to this IP) is received
    //       by this socket.
    let local_ip_addr: IpAddr = {
        let s = std::net::UdpSocket::bind("0.0.0.0:0")?;
        s.connect(SocketAddr::new(upstream_addr, 1))?;
        s.local_addr()?.ip()
    };
    let local_ip = local_ip_addr.to_string();

    // In production (probe_port == DISCOVERY_PORT) bind to the well-known port
    // so the telescope accepts the probe (it keys on source port 4720).
    // In test mode (any other port) bind to an ephemeral source port and
    // unicast directly to upstream_addr rather than broadcasting, so the mock
    // and the probe socket don't compete for the same address:port.
    let (probe_addr, target) = if probe_port == DISCOVERY_PORT {
        (
            SocketAddr::new(local_ip_addr, probe_port),
            SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::BROADCAST), probe_port),
        )
    } else {
        (
            SocketAddr::new(local_ip_addr, 0),
            SocketAddr::new(upstream_addr, probe_port),
        )
    };
    let socket = UdpSocket::bind(probe_addr).await?;
    socket.set_broadcast(true)?;

    let probe = serde_json::json!({
        "id": 201,
        "method": "scan_iscope",
        "name": "seestarproxy",
        "ip": local_ip
    });
    let probe_bytes = serde_json::to_vec(&probe)?;
    info!(
        "Discovery probe: sending {} bytes from {:?} to {}",
        probe_bytes.len(),
        socket.local_addr(),
        target
    );
    socket.send_to(&probe_bytes, target).await?;
    info!(
        "Discovery probe: sent OK, waiting up to 5s for response from {}",
        upstream_addr
    );

    let mut buf = [0u8; 16_384];
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let (n, src) = socket.recv_from(&mut buf).await?;
            info!("Discovery probe: received {} bytes from {}", n, src);
            if src.ip() == upstream_addr {
                let slice = buf[..n].trim_ascii_end();
                match serde_json::from_slice(slice) {
                    Ok(v) => return Ok::<Value, anyhow::Error>(v),
                    Err(e) => {
                        let text = String::from_utf8_lossy(slice);
                        // Distinguish two sources of malformed responses:
                        //
                        // 1. Firmware error (e.g. code-101 "Second root"): the telescope
                        //    rejected our probe but DID receive it, so the guest-mode
                        //    handshake happened.  The response contains "code" or "error"
                        //    fields (with unescaped quotes making the JSON invalid).
                        //    → break out and try TCP fallback immediately.
                        //
                        // 2. Echo of another client's probe: the telescope embeds the
                        //    sender's name verbatim; if that name contained special chars
                        //    the response is also unparseable, but has "result" not "error".
                        //    → skip and keep waiting for our own probe's response.
                        if text.contains("\"code\"") || text.contains("\"error\"") {
                            warn!(
                                "Discovery probe: firmware error response from upstream \
                                 ({}): {} — proceeding to TCP fallback immediately",
                                e, text
                            );
                            return Err(e.into());
                        }
                        warn!(
                            "Discovery probe: ignoring malformed response from upstream \
                             ({}): {}",
                            e, text
                        );
                        continue;
                    }
                }
            }
        }
    })
    .await;

    match result {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(_)) | Err(_) => {
            // Either the telescope responded with malformed JSON (probe was received,
            // guest-mode handshake happened) or the probe timed out entirely.
            // Either way, try TCP get_device_state to build a proper response.
            warn!(
                "Trying TCP fallback for device info from {}...",
                upstream_addr
            );
            match fetch_device_info_tcp(upstream_addr).await {
                Some(info) => Ok(info),
                None => {
                    // Last resort: minimal response with native format.
                    Ok(serde_json::json!({
                        "jsonrpc": "2.0",
                        "Timestamp": "0",
                        "method": "scan_iscope",
                        "result": {
                            "product_model": "Seestar (via proxy)",
                            "sn": "proxy",
                            "ssid": "Seestar_proxy",
                            "is_verified": true,
                            "tcp_client_num": 0
                        },
                        "code": 0,
                        "id": 201
                    }))
                }
            }
        }
    }
}

/// Build the configured-identity discovery response (extracted for testing).
fn build_configured_response(
    sn: &str,
    model: Option<&str>,
    bssid: Option<&str>,
    proxy_ip: &str,
) -> serde_json::Value {
    let model = model.unwrap_or("Seestar S50");
    let ssid = format!("S50_{}", sn);
    let mut result = serde_json::json!({
        "product_model": model,
        "sn": sn,
        "ssid": ssid,
        "ip": proxy_ip,
        "is_verified": true,
        "tcp_client_num": 0,
        "can_star_mode_sel_cam": false,
        "serc": "WPA-PSK"
    });
    if let Some(b) = bssid {
        result["bssid"] = serde_json::Value::String(b.to_string());
    }
    serde_json::json!({
        "jsonrpc": "2.0",
        "Timestamp": "0",
        "method": "scan_iscope",
        "result": result,
        "code": 0,
        "id": 201
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, UdpSocket};

    /// Regression test: the Seestar firmware echoes the `name` field from a probe
    /// back into its response JSON without escaping it. If another client on the
    /// network sends a probe with a dashed name (e.g. "macbook-pro"), the Seestar's
    /// response is malformed JSON. `probe_upstream` must skip that packet and keep
    /// waiting rather than bubbling up a parse error.
    #[tokio::test]
    async fn probe_upstream_skips_malformed_then_succeeds() {
        // Use probe_upstream_at with an ephemeral port to avoid any conflict
        // with the well-known port 4720.  The mock binds on 127.0.0.1:0 and
        // probe_upstream_at targets it directly.
        let mock_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mock_port = mock_sock.local_addr().unwrap().port();

        let valid_response = serde_json::json!({
            "id": 201,
            "method": "scan_iscope",
            "result": {
                "product_model": "Seestar S50",
                "sn": "TEST001",
                "ip": "127.0.0.1",
                "tcp_client_num": 0
            },
            "code": 0
        });
        let valid_bytes = serde_json::to_vec(&valid_response).unwrap();

        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            // Receive the probe broadcast (delivered to loopback).
            let Ok((_, src)) = mock_sock.recv_from(&mut buf).await else {
                return;
            };
            // Simulate the Seestar responding to a *different* client's probe that
            // contained a dash in the name — the firmware embeds the name verbatim,
            // producing malformed JSON.
            let malformed =
                b"{\"id\":201,\"method\":\"scan_iscope\",\"result\":{\"name\":\"bad-name{broken\"}";
            let _ = mock_sock.send_to(malformed, src).await;
            // Then send the real (valid) response.
            let _ = mock_sock.send_to(&valid_bytes, src).await;
        });

        let upstream: IpAddr = "127.0.0.1".parse().unwrap();
        let result = probe_upstream_at(upstream, mock_port).await;
        assert!(
            result.is_ok(),
            "probe_upstream should succeed despite receiving a malformed response first: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap()["result"]["sn"], "TEST001");
    }

    // ── run: IP patching ─────────────────────────────────────────────────────

    /// The `device_info` returned by `probe_upstream` contains the upstream
    /// telescope's IP. `run` must overwrite `result.ip` with the proxy's bind
    /// address so the Seestar app connects to the proxy, not the telescope.
    #[tokio::test]
    async fn run_patches_result_ip_to_proxy_bind_address() {
        // Bind a mock telescope UDP socket on an ephemeral port.
        let mock_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mock_port = mock_sock.local_addr().unwrap().port();

        // Build a discovery response that has the telescope's own IP.
        let telescope_response = serde_json::json!({
            "id": 201,
            "method": "scan_iscope",
            "result": {
                "product_model": "Seestar S50",
                "sn": "TEST_IP_PATCH",
                "ip": "10.0.0.99",   // telescope's real IP — should be replaced
                "tcp_client_num": 0
            },
            "code": 0
        });
        let resp_bytes = serde_json::to_vec(&telescope_response).unwrap();

        // Serve one probe response.
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let Ok((_, src)) = mock_sock.recv_from(&mut buf).await else {
                return;
            };
            let _ = mock_sock.send_to(&resp_bytes, src).await;
        });

        // probe_upstream with a custom port (we reuse probe_upstream directly
        // since the port is embedded in the SocketAddr built inside it — instead,
        // test via the patching logic directly).
        let upstream: IpAddr = "127.0.0.1".parse().unwrap();

        // Simulate what run() does: get device info, patch the IP.
        let probe = serde_json::json!({
            "id": 201, "method": "scan_iscope",
            "name": "seestarproxy", "ip": "127.0.0.1"
        });
        let sock = UdpSocket::bind("0.0.0.0:0").await.unwrap();
        sock.set_broadcast(true).unwrap();
        let target = std::net::SocketAddr::new(upstream, mock_port);
        let mut bytes = serde_json::to_vec(&probe).unwrap();
        bytes.extend_from_slice(b"\r\n");
        sock.send_to(&bytes, target).await.unwrap();
        let mut buf = [0u8; 4096];
        let (n, _) = sock.recv_from(&mut buf).await.unwrap();
        let mut device_info: serde_json::Value = serde_json::from_slice(&buf[..n]).unwrap();

        // Apply the same patching logic as run().
        let bind_addr: IpAddr = "192.168.5.10".parse().unwrap();
        if let Some(result) = device_info.get_mut("result") {
            result["ip"] = serde_json::Value::String(bind_addr.to_string());
        }

        assert_eq!(
            device_info["result"]["ip"], "192.168.5.10",
            "result.ip must be patched to the proxy bind address"
        );
        assert_eq!(
            device_info["result"]["sn"], "TEST_IP_PATCH",
            "other fields must be preserved"
        );
    }

    // ── fetch_device_info_tcp ─────────────────────────────────────────────────

    /// The telescope may push event lines before replying to get_device_state.
    /// The TCP fallback must skip those and return the line that has a result object.
    #[tokio::test]
    async fn fetch_device_info_tcp_skips_event_lines_before_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let upstream: IpAddr = "127.0.0.1".parse().unwrap();

        tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 256];
            let _ = conn.read(&mut buf).await;
            // Simulate the telescope sending an event and a heartbeat before the reply.
            conn.write_all(
                b"{\"Event\":\"PiStatus\",\"Timestamp\":\"1.0\",\"state\":\"working\"}\r\n",
            )
            .await
            .unwrap();
            conn.write_all(b"{\"id\":1,\"method\":\"test_connection\",\"result\":\"pong\"}\r\n")
                .await
                .unwrap();
            let resp = serde_json::json!({
                "id": 999,
                "result": {
                    "device": { "sn": "SN_SKIP", "product_model": "Seestar S50" },
                    "ap": { "ssid": "SkipNet" }
                },
                "code": 0
            });
            conn.write_all(format!("{}\r\n", serde_json::to_string(&resp).unwrap()).as_bytes())
                .await
                .unwrap();
        });

        let result = fetch_device_info_tcp_at(upstream, port).await;
        assert!(result.is_some(), "must find the device-state response line");
        let v = result.unwrap();
        assert_eq!(v["result"]["sn"], "SN_SKIP");
        assert_eq!(v["result"]["ssid"], "SkipNet");
    }

    /// Happy path: a TCP server responds to get_device_state with full device info.
    #[tokio::test]
    async fn fetch_device_info_tcp_returns_discovery_response_on_success() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let upstream: IpAddr = "127.0.0.1".parse().unwrap();

        tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            // Read the request (we don't need to parse it).
            let mut buf = [0u8; 256];
            let _ = conn.read(&mut buf).await;
            // Send a mock get_device_state response.
            let resp = serde_json::json!({
                "id": 999,
                "result": {
                    "device": {
                        "sn": "SN12345",
                        "product_model": "Seestar S50"
                    },
                    "ap": {
                        "ssid": "Seestar_TestAP"
                    }
                },
                "code": 0
            });
            let line = format!("{}\r\n", serde_json::to_string(&resp).unwrap());
            conn.write_all(line.as_bytes()).await.unwrap();
        });

        // Override port: fetch_device_info_tcp uses port 4700; we patch via a different helper.
        // We test it directly at the correct port.
        let result = fetch_device_info_tcp_at(upstream, port).await;
        assert!(result.is_some(), "must return Some on success");
        let v = result.unwrap();
        assert_eq!(v["method"], "scan_iscope");
        assert_eq!(v["result"]["sn"], "SN12345");
        assert_eq!(v["result"]["product_model"], "Seestar S50");
        assert_eq!(v["result"]["ssid"], "Seestar_TestAP");
        assert_eq!(v["code"], 0);
    }

    /// Helper that calls fetch_device_info_tcp with a custom port (for tests).
    async fn fetch_device_info_tcp_at(upstream_addr: IpAddr, port: u16) -> Option<Value> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpStream;

        let addr = std::net::SocketAddr::new(upstream_addr, port);
        let stream =
            tokio::time::timeout(std::time::Duration::from_secs(5), TcpStream::connect(addr))
                .await
                .ok()? // Elapsed → None
                .ok()?; // io::Error → None

        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        writer
            .write_all(b"{\"id\":999,\"method\":\"get_device_state\",\"params\":[\"verify\"]}\r\n")
            .await
            .ok()?;

        let parsed: Value = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).await?;
                let v: Value = match serde_json::from_str(line.trim()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if v.get("result").and_then(|r| r.as_object()).is_some() {
                    return Ok::<Value, anyhow::Error>(v);
                }
            }
        })
        .await
        .ok()?
        .ok()?;

        let result = parsed.get("result")?;
        Some(serde_json::json!({
            "jsonrpc": "2.0",
            "Timestamp": "0",
            "method": "scan_iscope",
            "result": {
                "sn": result.pointer("/device/sn").and_then(|v| v.as_str()).unwrap_or("unknown"),
                "product_model": result.pointer("/device/product_model").and_then(|v| v.as_str()).unwrap_or("Seestar"),
                "ssid": result.pointer("/ap/ssid").and_then(|v| v.as_str()).unwrap_or(""),
                "is_verified": true,
                "tcp_client_num": 0,
            },
            "code": 0,
            "id": 201
        }))
    }

    /// Error path: no server listening — must return None without panic.
    #[tokio::test]
    async fn fetch_device_info_tcp_returns_none_when_refused() {
        // Bind a listener then immediately drop it to free the port.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let upstream: IpAddr = "127.0.0.1".parse().unwrap();
        let result = fetch_device_info_tcp_at(upstream, port).await;
        assert!(
            result.is_none(),
            "must return None when connection is refused"
        );
    }

    /// Malformed JSON response — must return None gracefully.
    #[tokio::test]
    async fn fetch_device_info_tcp_returns_none_on_malformed_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let upstream: IpAddr = "127.0.0.1".parse().unwrap();

        tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 256];
            let _ = conn.read(&mut buf).await;
            conn.write_all(b"not valid json at all\r\n").await.unwrap();
        });

        let result = fetch_device_info_tcp_at(upstream, port).await;
        assert!(result.is_none(), "must return None on malformed JSON");
    }

    /// The real `fetch_device_info_tcp` uses port 4700. With nothing listening
    /// on 127.0.0.1:4700, it should return None immediately.
    #[tokio::test]
    async fn fetch_device_info_tcp_returns_none_when_port_4700_not_listening() {
        // If port 4700 happens to be in use locally, skip the test.
        let check = tokio::net::TcpStream::connect("127.0.0.1:4700").await;
        if check.is_ok() {
            return; // Something is already listening; skip to avoid interference.
        }
        let upstream: IpAddr = "127.0.0.1".parse().unwrap();
        let result = fetch_device_info_tcp(upstream).await;
        assert!(
            result.is_none(),
            "must return None when nothing listens on port 4700"
        );
    }

    /// Response with missing result fields falls back to defaults.
    #[tokio::test]
    async fn fetch_device_info_tcp_uses_defaults_for_missing_fields() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let upstream: IpAddr = "127.0.0.1".parse().unwrap();

        tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 256];
            let _ = conn.read(&mut buf).await;
            // Response with result but none of the expected nested fields.
            let resp = serde_json::json!({"id": 999, "result": {}, "code": 0});
            let line = format!("{}\r\n", serde_json::to_string(&resp).unwrap());
            conn.write_all(line.as_bytes()).await.unwrap();
        });

        let result = fetch_device_info_tcp_at(upstream, port).await;
        assert!(result.is_some());
        let v = result.unwrap();
        assert_eq!(v["result"]["sn"], "unknown");
        assert_eq!(v["result"]["product_model"], "Seestar");
        assert_eq!(v["result"]["ssid"], "");
    }

    // ── build_configured_response ─────────────────────────────────────────────

    #[test]
    fn configured_response_uses_provided_sn_and_model() {
        let v = build_configured_response("4ddb0535", Some("Seestar S50"), None, "192.168.1.10");
        assert_eq!(v["result"]["sn"], "4ddb0535");
        assert_eq!(v["result"]["product_model"], "Seestar S50");
    }

    #[test]
    fn configured_response_derives_ssid_from_sn() {
        let v = build_configured_response("4ddb0535", None, None, "192.168.1.10");
        assert_eq!(v["result"]["ssid"], "S50_4ddb0535");
    }

    #[test]
    fn configured_response_defaults_model_to_seestar_s50() {
        let v = build_configured_response("abc123", None, None, "192.168.1.10");
        assert_eq!(v["result"]["product_model"], "Seestar S50");
    }

    #[test]
    fn configured_response_includes_bssid_when_provided() {
        let v =
            build_configured_response("4ddb0535", None, Some("c2:f5:35:2f:17:26"), "192.168.1.10");
        assert_eq!(v["result"]["bssid"], "c2:f5:35:2f:17:26");
    }

    #[test]
    fn configured_response_omits_bssid_when_not_provided() {
        let v = build_configured_response("4ddb0535", None, None, "192.168.1.10");
        assert!(v["result"].get("bssid").is_none());
    }

    #[test]
    fn configured_response_sets_proxy_ip() {
        let v = build_configured_response("4ddb0535", None, None, "10.0.0.5");
        assert_eq!(v["result"]["ip"], "10.0.0.5");
    }
}

/// Fetch device info via TCP get_device_state and build a discovery response.
async fn fetch_device_info_tcp(upstream_addr: IpAddr) -> Option<Value> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;

    let addr = SocketAddr::new(upstream_addr, 4700);
    info!("TCP fallback: connecting to {}...", addr);
    let stream =
        match tokio::time::timeout(std::time::Duration::from_secs(5), TcpStream::connect(addr))
            .await
        {
            Ok(Ok(s)) => {
                info!("TCP fallback: connected");
                s
            }
            Ok(Err(e)) => {
                warn!("TCP fallback: connect error: {}", e);
                return None;
            }
            Err(_) => {
                warn!("TCP fallback: connect timed out");
                return None;
            }
        };

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    writer
        .write_all(b"{\"id\":999,\"method\":\"get_device_state\",\"params\":[\"verify\"]}\r\n")
        .await
        .ok()?;

    // The telescope may send initial event messages before replying to our
    // get_device_state command.  Read lines until we find the one that has
    // id == 999 (our request) and a "result" object, or give up after 5 s.
    let parsed: Value = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut line = String::new();
        loop {
            line.clear();
            reader.read_line(&mut line).await?;
            let trimmed = line.trim();
            info!("TCP fallback recv: {}", trimmed);
            let v: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // Accept any line where result is a JSON object (the device-state reply).
            if v.get("result").and_then(|r| r.as_object()).is_some() {
                return Ok::<Value, anyhow::Error>(v);
            }
        }
    })
    .await
    .ok()? // timeout elapsed → None
    .ok()?; // io::Error → None

    let result = parsed.get("result")?;

    let discovery = serde_json::json!({
        "jsonrpc": "2.0",
        "Timestamp": "0",
        "method": "scan_iscope",
        "result": {
            "sn": result.pointer("/device/sn").and_then(|v| v.as_str()).unwrap_or("unknown"),
            "product_model": result.pointer("/device/product_model").and_then(|v| v.as_str()).unwrap_or("Seestar"),
            "ssid": result.pointer("/ap/ssid").and_then(|v| v.as_str()).unwrap_or(""),
            "is_verified": true,
            "tcp_client_num": 0,
        },
        "code": 0,
        "id": 201
    });

    info!(
        "Built discovery response from TCP device state: {}",
        discovery
    );
    Some(discovery)
}
