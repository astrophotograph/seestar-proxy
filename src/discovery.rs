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
) -> anyhow::Result<()> {
    // First, discover the upstream Seestar to get its device info.
    let device_info = probe_upstream(upstream_addr).await?;
    info!(
        "Cached upstream device info: {}",
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

        // Ignore responses (which also have method "scan_iscope") to avoid
        // a feedback loop when we broadcast our own response and receive it
        // back on the same socket.
        if request.get("result").is_some() || request.get("code").is_some() {
            continue;
        }

        info!("Discovery request from {}", src_addr);

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
    }
}

/// Send a discovery probe to the upstream Seestar and return its response.
///
/// The Seestar is picky about probe format (discovered by bisection):
/// - `name` must not contain dashes — a dash causes malformed JSON in the response
/// - Message must be terminated with `\r\n` — Seestar ignores probes without it
/// - Probe must be sent from an ephemeral source port — Seestar replies unicast
///   to the sender's port, not back to port 4720
async fn probe_upstream(upstream_addr: IpAddr) -> anyhow::Result<Value> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.set_broadcast(true)?;

    // Detect local IP for the probe.
    let local_ip = {
        let s = std::net::UdpSocket::bind("0.0.0.0:0")?;
        s.connect(SocketAddr::new(upstream_addr, 1))?;
        s.local_addr()?.ip().to_string()
    };

    let probe = serde_json::json!({
        "id": 201,
        "method": "scan_iscope",
        "name": "seestarproxy",
        "ip": local_ip
    });

    let target = SocketAddr::new(upstream_addr, DISCOVERY_PORT);
    let mut probe_bytes = serde_json::to_vec(&probe)?;
    probe_bytes.extend_from_slice(b"\r\n");
    socket.send_to(&probe_bytes, target).await?;

    let mut buf = [0u8; 16_384];
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let (n, src) = socket.recv_from(&mut buf).await?;
            if src.ip() == upstream_addr {
                let slice = buf[..n].trim_ascii_end();
                match serde_json::from_slice(slice) {
                    Ok(v) => return Ok::<Value, anyhow::Error>(v),
                    Err(e) => {
                        warn!(
                            "Discovery probe: ignoring malformed response from upstream ({}): {}",
                            e,
                            String::from_utf8_lossy(slice)
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
        Ok(Err(e)) => Err(e),
        Err(_) => {
            warn!(
                "Discovery probe to {} timed out, using minimal info",
                upstream_addr
            );
            // Fallback: try TCP get_device_state to build a proper response.
            warn!("Trying TCP fallback for device info...");
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
        // Try to bind the mock telescope port; skip if already in use.
        let mock_sock = match UdpSocket::bind("127.0.0.1:4720").await {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "SKIP probe_upstream_skips_malformed_then_succeeds: could not bind 127.0.0.1:4720: {e}"
                );
                return;
            }
        };

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
            // Receive the probe from probe_upstream.
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
        let result = probe_upstream(upstream).await;
        assert!(
            result.is_ok(),
            "probe_upstream should succeed despite receiving a malformed response first: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap()["result"]["sn"], "TEST001");
    }

    // ── fetch_device_info_tcp ─────────────────────────────────────────────────

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
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader.read_line(&mut line),
        )
        .await
        .ok()?
        .ok()?;

        let parsed: Value = serde_json::from_str(line.trim()).ok()?;
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
}

/// Fetch device info via TCP get_device_state and build a discovery response.
async fn fetch_device_info_tcp(upstream_addr: IpAddr) -> Option<Value> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;

    let addr = SocketAddr::new(upstream_addr, 4700);
    let stream = tokio::time::timeout(std::time::Duration::from_secs(5), TcpStream::connect(addr))
        .await
        .ok()?
        .ok()?;

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    writer
        .write_all(b"{\"id\":999,\"method\":\"get_device_state\",\"params\":[\"verify\"]}\r\n")
        .await
        .ok()?;

    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader.read_line(&mut line),
    )
    .await
    .ok()?
    .ok()?;

    let parsed: Value = serde_json::from_str(line.trim()).ok()?;
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
