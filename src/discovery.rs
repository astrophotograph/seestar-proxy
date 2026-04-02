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
                let response: Value = serde_json::from_slice(slice)?;
                return Ok::<Value, anyhow::Error>(response);
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
