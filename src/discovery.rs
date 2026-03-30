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
use tracing::{debug, error, info, warn};

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

    let mut buf = [0u8; 4096];

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

        debug!("Discovery request from {}", src_addr);

        // Build response with our cached info, substituting the proxy address.
        let mut response = device_info.clone();

        // The response should make clients connect to us, not the real Seestar.
        // Substitute the address in the result if present.
        if let Some(result) = response.get_mut("result") {
            if let Some(obj) = result.as_object_mut() {
                // Some clients use the source IP of the response, but we also
                // update any address field in the payload to be safe.
                let proxy_ip = if bind_addr.is_unspecified() {
                    // If bound to 0.0.0.0, use the source IP the client sees
                    // (we can't easily determine this, so leave the field alone
                    // and rely on the UDP source address).
                    None
                } else {
                    Some(bind_addr.to_string())
                };

                if let Some(ip) = proxy_ip {
                    obj.insert("ip".to_string(), Value::String(ip));
                }
            }
        }

        let response_bytes = serde_json::to_vec(&response)?;
        if let Err(e) = socket.send_to(&response_bytes, src_addr).await {
            warn!("Failed to send discovery response to {}: {}", src_addr, e);
        } else {
            debug!("Sent discovery response to {}", src_addr);
        }
    }
}

/// Send a discovery probe to the upstream Seestar and return its response.
async fn probe_upstream(upstream_addr: IpAddr) -> anyhow::Result<Value> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.set_broadcast(true)?;

    let probe = serde_json::json!({
        "id": 201,
        "method": "scan_iscope",
        "name": "seestar-proxy",
        "ip": "0.0.0.0"
    });

    let target = SocketAddr::new(upstream_addr, DISCOVERY_PORT);
    let probe_bytes = serde_json::to_vec(&probe)?;

    // Send probe and wait for response (with timeout).
    socket.send_to(&probe_bytes, target).await?;

    let mut buf = [0u8; 4096];
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
            // Return minimal info so the proxy can still function.
            Ok(serde_json::json!({
                "id": 201,
                "result": {
                    "product_model": "Seestar (via proxy)",
                    "sn": "proxy",
                    "tcp_client_num": 0
                }
            }))
        }
    }
}
