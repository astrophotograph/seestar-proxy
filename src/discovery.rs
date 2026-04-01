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

    // Listen for discovery broadcasts from clients.
    let listen_addr = SocketAddr::new(bind_addr, DISCOVERY_PORT);
    let socket = UdpSocket::bind(listen_addr).await?;
    socket.set_broadcast(true)?;
    info!("Discovery bridge listening on {}", listen_addr);

    // 16 KiB covers any realistic JSON device-info payload.
    let mut buf = [0u8; 16_384];

    loop {
        let (n, src_addr) = match socket.recv_from(&mut buf).await {
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

        let method = request
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if method != "scan_iscope" {
            continue;
        }

        info!("Discovery request from {}", src_addr);

        // Respond with cached Seestar info.
        // The app uses the SOURCE IP of the UDP response to determine where
        // to connect, so we don't need to modify the payload — the response
        // comes from the proxy's IP automatically.
        let response_bytes = serde_json::to_vec(&device_info)?;
        if let Err(e) = socket.send_to(&response_bytes, src_addr).await {
            warn!("Failed to send discovery response to {}: {}", src_addr, e);
        } else {
            info!(
                "Sent discovery response to {} ({} bytes) — app should connect to proxy",
                src_addr,
                response_bytes.len()
            );
        }
    }
}

/// Send a discovery probe to the upstream Seestar and return its response.
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
        "name": "seestar-proxy",
        "ip": local_ip
    });

    // Try unicast first, then broadcast if that fails.
    let target = SocketAddr::new(upstream_addr, DISCOVERY_PORT);
    let probe_bytes = serde_json::to_vec(&probe)?;
    socket.send_to(&probe_bytes, target).await?;

    // Also send broadcast in case the Seestar only responds to broadcast.
    let broadcast_target = SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::BROADCAST), DISCOVERY_PORT);
    let _ = socket.send_to(&probe_bytes, broadcast_target).await;

    let mut buf = [0u8; 16_384];
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let (n, src) = socket.recv_from(&mut buf).await?;
            if src.ip() == upstream_addr {
                let response: Value = serde_json::from_slice(&buf[..n])?;
                return Ok::<Value, anyhow::Error>(response);
            }
        }
    })
    .await;

    match result {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(e),
        Err(_) => {
            warn!("Discovery probe to {} timed out, using minimal info", upstream_addr);
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
    let stream = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        TcpStream::connect(addr),
    ).await.ok()?.ok()?;

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    writer.write_all(b"{\"id\":999,\"method\":\"get_device_state\",\"params\":[\"verify\"]}\r\n").await.ok()?;

    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader.read_line(&mut line),
    ).await.ok()?.ok()?;

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

    info!("Built discovery response from TCP device state: {}", discovery);
    Some(discovery)
}

