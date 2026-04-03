//! Embedded WireGuard tunnel endpoint for cross-subnet telescope access.
//!
//! Architecture:
//! ```text
//! [Phone/Laptop WireGuard]  ──UDP──▶  UdpSocket(:51820)
//!                                          │
//!                                     boringtun decrypt
//!                                          │
//!                                     raw IP packets
//!                                          │
//!                                     smoltcp userspace TCP
//!                                          │
//!                                  ┌───────┴────────┐
//!                              :4700 control    :4800 imaging
//!                                  │                │
//!                           inject into         inject into
//!                           control proxy       imaging proxy
//! ```

pub mod bridge;
pub mod dns;
pub mod keys;
pub mod netstack;
pub mod qr;
pub mod tunnel_discovery;

use boringtun::noise::{Tunn, TunnResult, errors::WireGuardError};
use keys::WgKeypair;
use std::net::SocketAddr;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Shared WireGuard state for the dashboard.
#[derive(Clone)]
pub struct WgInfo {
    pub enabled: bool,
    pub port: u16,
    pub server_public_key: String,
    pub client_config: String,
    pub client_config_svg: String,
    pub endpoint: String,
}

/// Start the WireGuard tunnel endpoint.
///
/// Returns `WgInfo` for the dashboard, and spawns the packet processing
/// loop as a background task. Tunnel TCP connections are sent to the
/// provided injection channels.
#[allow(clippy::too_many_arguments)]
pub async fn start(
    bind_addr: std::net::IpAddr,
    port: u16,
    key_file: &std::path::Path,
    endpoint: Option<String>,
    upstream_ip: std::net::IpAddr,
    upstream_control_port: u16,
    upstream_imaging_port: u16,
    _control_inject_tx: mpsc::Sender<tokio::net::TcpStream>,
    _imaging_inject_tx: mpsc::Sender<tokio::net::TcpStream>,
) -> anyhow::Result<WgInfo> {
    // Load or generate server keys.
    let server_keys = WgKeypair::load_or_generate(key_file)?;
    let server_pub_b64 = server_keys.public_key_b64();

    // Load or generate client keys (persisted so QR code survives restarts).
    let client_key_file = key_file.with_file_name("wg_client.key");
    let client_keys = WgKeypair::load_or_generate(&client_key_file)?;
    let client_priv_b64 = keys::private_key_b64(&client_keys.private);
    let client_pub_b64 = keys::public_key_b64(&client_keys.public);
    let client_public = client_keys.public;

    // Determine endpoint for client config.
    let endpoint_str = endpoint.unwrap_or_else(|| {
        // Try to detect a non-loopback IP for the endpoint.
        let detected = detect_local_ip().unwrap_or_else(|| bind_addr.to_string());
        if detected == "0.0.0.0" || detected == "::" {
            warn!("Could not detect external IP. Use --wg-endpoint to set the proxy's reachable address.");
        }
        format!("{}:{}", detected, port)
    });

    // Build client config and QR.
    // Route all traffic through the tunnel so UDP discovery broadcasts
    // (which go to 255.255.255.255 or subnet broadcast) are captured.
    // The phone's internet will also go through the proxy machine.
    let allowed_ips = "0.0.0.0/0";
    let client_config = qr::client_config(
        &client_priv_b64,
        "10.99.0.2/32",
        &server_pub_b64,
        &endpoint_str,
        allowed_ips,
    );

    let client_config_svg = qr::config_to_svg(&client_config)
        .unwrap_or_else(|e| format!("QR generation failed: {}", e));

    // Print QR to terminal.
    if let Ok(terminal_qr) = qr::config_to_terminal(&client_config) {
        println!("\nWireGuard Client Config (scan with WireGuard app):\n");
        println!("{}", terminal_qr);
        println!();
    }

    info!("WireGuard server public key: {}", server_pub_b64);
    info!("WireGuard client public key: {}", client_pub_b64);
    info!("WireGuard endpoint: {}", endpoint_str);

    let wg_info = WgInfo {
        enabled: true,
        port,
        server_public_key: server_pub_b64,
        client_config: client_config.clone(),
        client_config_svg,
        endpoint: endpoint_str,
    };

    // Create the WireGuard tunnel.
    let tunn = Tunn::new(
        server_keys.private.clone(),
        client_public,
        None, // preshared key
        None, // keepalive interval (client sets this)
        0,    // tunnel index
        None, // rate limiter
    );

    // Bind UDP socket.
    let listen_addr = SocketAddr::new(bind_addr, port);
    let udp = UdpSocket::bind(listen_addr).await?;
    info!("WireGuard listening on UDP {}", listen_addr);

    // Start the userspace TCP/IP stack.
    let upstream_v4 = match upstream_ip {
        std::net::IpAddr::V4(v4) => v4,
        _ => anyhow::bail!("WireGuard requires IPv4 upstream"),
    };
    let server_v4: std::net::Ipv4Addr = "10.99.0.1".parse().unwrap();
    let (net_channels, mut conn_rx) = netstack::start(
        server_v4,
        upstream_v4,
        upstream_control_port,
        upstream_imaging_port,
    );

    // Spawn the bridge task: routes accepted tunnel TCP connections to local proxy.
    let local_control_port = upstream_control_port;
    let local_imaging_port = upstream_imaging_port;
    tokio::spawn(async move {
        while let Some(tunnel_stream) = conn_rx.recv().await {
            let local_port = if tunnel_stream.dest_port == upstream_control_port {
                local_control_port
            } else {
                local_imaging_port
            };
            tokio::spawn(bridge::bridge_to_local(tunnel_stream, local_port));
        }
    });

    // Fetch device info from the Seestar for tunnel discovery responses.
    let device_info_json = fetch_device_info(upstream_ip, upstream_control_port).await;

    // Spawn the packet processing loop with netstack + discovery injection.
    tokio::spawn(packet_loop(
        udp,
        tunn,
        net_channels,
        upstream_v4.octets(),
        device_info_json,
    ));

    Ok(wg_info)
}

/// Fetch device info from the real Seestar by sending a scan_iscope UDP probe.
/// This captures the exact native response format so tunnel clients get
/// an identical discovery response to what they'd see on the local network.
async fn fetch_device_info(upstream_ip: std::net::IpAddr, _control_port: u16) -> String {
    let probe = serde_json::json!({
        "id": 201,
        "method": "scan_iscope",
        "name": "seestar-proxy",
        "ip": "0.0.0.0"
    });

    let target = SocketAddr::new(upstream_ip, crate::protocol::DISCOVERY_PORT);

    match tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let probe_bytes = serde_json::to_vec(&probe)?;
        socket.send_to(&probe_bytes, target).await?;

        let mut buf = [0u8; 4096];
        loop {
            let (n, src) = socket.recv_from(&mut buf).await?;
            if src.ip() == upstream_ip {
                let response = String::from_utf8_lossy(&buf[..n]).to_string();
                return Ok::<String, anyhow::Error>(response);
            }
        }
    })
    .await
    {
        Ok(Ok(response)) => {
            info!(
                "Cached native discovery response: {}",
                &response[..response.len().min(200)]
            );
            response
        }
        _ => {
            warn!(
                "Discovery probe to {} timed out, building response from get_device_state",
                upstream_ip
            );
            // Fallback: try TCP get_device_state.
            fetch_device_info_tcp(upstream_ip, _control_port).await
        }
    }
}

/// Fallback: build a discovery response from get_device_state over TCP.
async fn fetch_device_info_tcp(upstream_ip: std::net::IpAddr, control_port: u16) -> String {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;

    let addr = SocketAddr::new(upstream_ip, control_port);
    match tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let stream = TcpStream::connect(addr).await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        writer
            .write_all(b"{\"id\":999,\"method\":\"get_device_state\",\"params\":[\"verify\"]}\r\n")
            .await?;
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        Ok::<String, anyhow::Error>(line)
    })
    .await
    {
        Ok(Ok(response)) => {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(response.trim()) {
                let result = parsed
                    .get("result")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                let discovery_response = serde_json::json!({
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
                    "Built discovery response from device state: {}",
                    discovery_response
                );
                serde_json::to_string(&discovery_response).unwrap_or_default()
            } else {
                default_device_info()
            }
        }
        _ => {
            warn!("Could not fetch device info for tunnel discovery");
            default_device_info()
        }
    }
}

/// Try to detect a non-loopback local IP address for the WireGuard endpoint.
fn detect_local_ip() -> Option<String> {
    use std::net::UdpSocket;
    // Connect to an external address (doesn't actually send data) to determine
    // which local interface would be used.
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let addr = socket.local_addr().ok()?;
    let ip = addr.ip();
    if ip.is_loopback() || ip.is_unspecified() {
        None
    } else {
        Some(ip.to_string())
    }
}

fn default_device_info() -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "Timestamp": "0",
        "method": "scan_iscope",
        "result": {
            "sn": "proxy",
            "product_model": "Seestar (via proxy)",
            "ssid": "Seestar_proxy",
            "is_verified": true,
            "tcp_client_num": 0,
        },
        "code": 0,
        "id": 201
    })
    .to_string()
}

/// Main packet processing loop.
///
/// Receives encrypted UDP datagrams, decrypts via boringtun, and processes
/// the resulting IP packets. UDP discovery broadcasts are handled inline.
async fn packet_loop(
    udp: UdpSocket,
    mut tunn: Tunn,
    mut net: netstack::NetStackChannels,
    upstream_ip_bytes: [u8; 4],
    device_info_json: String,
) {
    let mut recv_buf = vec![0u8; 65536];
    let mut peer_addr: Option<SocketAddr> = None;
    let mut session_active = false;

    // Client tunnel IP — we'll send proactive discovery to this address.
    let client_ip: [u8; 4] = [10, 99, 0, 2];

    info!("WireGuard packet loop started (with TCP stack)");

    let mut timer = tokio::time::interval(std::time::Duration::from_millis(250));
    // Periodic discovery announcements — send every 3 seconds while session is active.
    let mut discovery_timer = tokio::time::interval(std::time::Duration::from_secs(3));

    loop {
        tokio::select! {
            // Incoming UDP datagram from WireGuard client.
            result = udp.recv_from(&mut recv_buf) => {
                let (len, src) = match result {
                    Ok(r) => r,
                    Err(e) => {
                        error!("WireGuard UDP recv error: {}", e);
                        continue;
                    }
                };

                peer_addr = Some(src);

                // Mark session as active when we get data packets.
                if !session_active {
                    let mut probe_dst = vec![0u8; 65536];
                    let probe_result = tunn.decapsulate(None, &recv_buf[..len], &mut probe_dst);
                    if matches!(&probe_result, TunnResult::WriteToTunnelV4(_, _)) {
                        session_active = true;
                        info!("WireGuard session active — starting periodic discovery announcements");
                    }
                    // Process the packet (handles both handshake and data).
                    if let TunnAction::Decrypted(pkt) = process_tunn_result(&udp, src, probe_result, &net.inject_tx).await {
                        handle_decrypted_packet(&pkt, upstream_ip_bytes, &device_info_json, &mut tunn, &udp, src).await;
                    }
                    continue;
                }

                // Decrypt — loop to handle chained results.
                let mut dst = vec![0u8; 65536];
                loop {
                    let tunn_result = tunn.decapsulate(None, &recv_buf[..len], &mut dst);
                    match process_tunn_result(&udp, src, tunn_result, &net.inject_tx).await {
                        TunnAction::Continue => {
                            let cont = tunn.decapsulate(None, &[], &mut dst);
                            match process_tunn_result(&udp, src, cont, &net.inject_tx).await {
                                TunnAction::Decrypted(pkt) => {
                                    handle_decrypted_packet(&pkt, upstream_ip_bytes, &device_info_json, &mut tunn, &udp, src).await;
                                    break;
                                }
                                TunnAction::Done => break,
                                TunnAction::Continue => {} // keep looping
                            }
                        }
                        TunnAction::Decrypted(pkt) => {
                            handle_decrypted_packet(&pkt, upstream_ip_bytes, &device_info_json, &mut tunn, &udp, src).await;
                            break;
                        }
                        TunnAction::Done => break,
                    }
                }
            }

            // Outgoing IP packets from smoltcp → encrypt and send via WireGuard.
            Some(packet) = net.egress_rx.recv() => {
                if let Some(addr) = peer_addr {
                    let mut dst = vec![0u8; 65536];
                    let result = tunn.encapsulate(&packet, &mut dst);
                    if let TunnResult::WriteToNetwork(data) = result
                        && let Err(e) = udp.send_to(data, addr).await
                    {
                        warn!("WireGuard send error: {}", e);
                    }
                }
            }

            // Periodic timer for WireGuard protocol maintenance.
            _ = timer.tick() => {
                if let Some(addr) = peer_addr {
                    let mut dst = vec![0u8; 65536];
                    let tunn_result = tunn.update_timers(&mut dst);
                    let _ = process_tunn_result(&udp, addr, tunn_result, &net.inject_tx).await;
                }
            }

            // Periodic discovery announcements while session is active.
            _ = discovery_timer.tick() => {
                if session_active
                    && let Some(addr) = peer_addr
                {
                    let discovery_packet = tunnel_discovery::build_discovery_response(
                        upstream_ip_bytes,
                        client_ip,
                        crate::protocol::DISCOVERY_PORT,
                        device_info_json.as_bytes(),
                    );
                    let mut enc = vec![0u8; 65536];
                    if let TunnResult::WriteToNetwork(data) = tunn.encapsulate(&discovery_packet, &mut enc) {
                        let _ = udp.send_to(data, addr).await;
                    }
                    debug!("Sent periodic discovery announcement through tunnel");
                }
            }
        }
    }
}

/// Process a `TunnResult` from boringtun — send responses, handle decrypted data.
///
/// This takes ownership of the buffer to avoid borrow conflicts between
/// the TunnResult (which borrows from the buffer) and subsequent operations.
async fn process_tunn_result(
    udp: &UdpSocket,
    peer: SocketAddr,
    result: TunnResult<'_>,
    inject_tx: &mpsc::Sender<Vec<u8>>,
) -> TunnAction {
    match result {
        TunnResult::Done => TunnAction::Done,

        TunnResult::WriteToNetwork(data) => {
            if let Err(e) = udp.send_to(data, peer).await {
                warn!("WireGuard send error: {}", e);
            }
            TunnAction::Continue // Check for more pending operations
        }

        TunnResult::WriteToTunnelV4(data, _src) => {
            if data.len() >= 20 {
                let proto = data[9];
                let dst_ip = &data[16..20];
                let proto_name = match proto {
                    6 => "TCP",
                    17 => "UDP",
                    1 => "ICMP",
                    _ => "?",
                };

                // Extract port for TCP and UDP.
                let ihl = (data[0] & 0x0f) as usize * 4;
                if (proto == 6 || proto == 17) && data.len() >= ihl + 4 {
                    let dst_port = u16::from_be_bytes([data[ihl + 2], data[ihl + 3]]);
                    let src_port = u16::from_be_bytes([data[ihl], data[ihl + 1]]);
                    debug!(
                        "WireGuard decrypted: {} {}.{}.{}.{}:{} -> {}.{}.{}.{}:{} ({} bytes)",
                        proto_name,
                        data[12],
                        data[13],
                        data[14],
                        data[15],
                        src_port,
                        dst_ip[0],
                        dst_ip[1],
                        dst_ip[2],
                        dst_ip[3],
                        dst_port,
                        data.len()
                    );
                } else {
                    debug!(
                        "WireGuard decrypted: {} -> {}.{}.{}.{} ({} bytes)",
                        proto_name,
                        dst_ip[0],
                        dst_ip[1],
                        dst_ip[2],
                        dst_ip[3],
                        data.len()
                    );
                }
            }
            // Feed decrypted IP packet into the userspace TCP stack.
            // Discovery interception happens in the packet loop (needs &mut tunn).
            if inject_tx.try_send(data.to_vec()).is_err() {
                warn!("NetStack inject channel full, dropping packet");
            }
            TunnAction::Decrypted(data.to_vec())
        }

        TunnResult::WriteToTunnelV6(_, _) => {
            debug!("WireGuard: ignoring IPv6 packet");
            TunnAction::Done
        }

        TunnResult::Err(WireGuardError::ConnectionExpired) => {
            debug!("WireGuard: connection expired");
            TunnAction::Done
        }

        TunnResult::Err(e) => {
            debug!("WireGuard error: {:?}", e);
            TunnAction::Done
        }
    }
}

enum TunnAction {
    Done,
    Continue,
    /// A decrypted IPv4 packet was received and injected into the TCP stack.
    /// The packet bytes are returned so the caller can also check for UDP discovery and ICMP.
    Decrypted(Vec<u8>),
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── WgInfo ────────────────────────────────────────────────────────────────

    #[test]
    fn wg_info_fields_are_accessible() {
        let info = WgInfo {
            enabled: true,
            port: 51820,
            server_public_key: "server_pub".to_string(),
            client_config: "[Interface]\nPrivateKey = x\n".to_string(),
            client_config_svg: "<svg/>".to_string(),
            endpoint: "10.0.0.1:51820".to_string(),
        };
        assert!(info.enabled);
        assert_eq!(info.port, 51820);
        assert_eq!(info.endpoint, "10.0.0.1:51820");
    }

    #[test]
    fn wg_info_clone_is_independent() {
        let info = WgInfo {
            enabled: false,
            port: 9999,
            server_public_key: "key".to_string(),
            client_config: "cfg".to_string(),
            client_config_svg: "svg".to_string(),
            endpoint: "host:9999".to_string(),
        };
        let cloned = info.clone();
        assert_eq!(cloned.port, info.port);
        assert_eq!(cloned.endpoint, info.endpoint);
    }

    // ── process_tunn_result() ─────────────────────────────────────────────────

    async fn dummy_udp() -> (UdpSocket, std::net::SocketAddr) {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        (sock, addr)
    }

    #[tokio::test]
    async fn process_tunn_result_done_returns_done() {
        let (udp, peer) = dummy_udp().await;
        let (inject_tx, _rx) = mpsc::channel::<Vec<u8>>(8);
        let action = process_tunn_result(&udp, peer, TunnResult::Done, &inject_tx).await;
        assert!(matches!(action, TunnAction::Done));
    }

    #[tokio::test]
    async fn process_tunn_result_ipv6_returns_done() {
        let (udp, peer) = dummy_udp().await;
        let (inject_tx, _rx) = mpsc::channel::<Vec<u8>>(8);
        let mut buf = [];
        let action = process_tunn_result(
            &udp,
            peer,
            TunnResult::WriteToTunnelV6(&mut buf, std::net::Ipv6Addr::LOCALHOST),
            &inject_tx,
        )
        .await;
        assert!(matches!(action, TunnAction::Done));
    }

    #[tokio::test]
    async fn process_tunn_result_connection_expired_returns_done() {
        let (udp, peer) = dummy_udp().await;
        let (inject_tx, _rx) = mpsc::channel::<Vec<u8>>(8);
        let action = process_tunn_result(
            &udp,
            peer,
            TunnResult::Err(WireGuardError::ConnectionExpired),
            &inject_tx,
        )
        .await;
        assert!(matches!(action, TunnAction::Done));
    }

    #[tokio::test]
    async fn process_tunn_result_other_wg_error_returns_done() {
        let (udp, peer) = dummy_udp().await;
        let (inject_tx, _rx) = mpsc::channel::<Vec<u8>>(8);
        let action = process_tunn_result(
            &udp,
            peer,
            TunnResult::Err(WireGuardError::InvalidMac),
            &inject_tx,
        )
        .await;
        assert!(matches!(action, TunnAction::Done));
    }

    #[tokio::test]
    async fn process_tunn_result_write_to_network_returns_continue() {
        let (udp, peer) = dummy_udp().await;
        let (inject_tx, _rx) = mpsc::channel::<Vec<u8>>(8);
        let mut data = *b"hello";
        let action = process_tunn_result(
            &udp,
            peer,
            TunnResult::WriteToNetwork(&mut data),
            &inject_tx,
        )
        .await;
        assert!(matches!(action, TunnAction::Continue));
    }

    #[tokio::test]
    async fn process_tunn_result_write_to_tunnel_v4_returns_decrypted() {
        let (udp, peer) = dummy_udp().await;
        let (inject_tx, _rx) = mpsc::channel::<Vec<u8>>(8);

        // Minimal 40-byte IPv4+TCP packet (checksums don't matter for this test).
        let mut pkt = vec![0u8; 40];
        pkt[0] = 0x45; // IPv4, IHL=5 (20 bytes)
        pkt[9] = 6; // Protocol: TCP
        pkt[16..20].copy_from_slice(&[10, 99, 0, 1]); // dst IP
        pkt[20..22].copy_from_slice(&[0xD4, 0x31]); // src port = 54321
        pkt[22..24].copy_from_slice(&[0x12, 0x5C]); // dst port = 4700

        let action = process_tunn_result(
            &udp,
            peer,
            TunnResult::WriteToTunnelV4(&mut pkt, std::net::Ipv4Addr::new(10, 99, 0, 2)),
            &inject_tx,
        )
        .await;
        assert!(matches!(action, TunnAction::Decrypted(_)));
    }

    #[tokio::test]
    async fn process_tunn_result_v4_short_packet_still_returns_decrypted() {
        let (udp, peer) = dummy_udp().await;
        let (inject_tx, _rx) = mpsc::channel::<Vec<u8>>(8);
        // Packet shorter than 20 bytes — the length check branch fires.
        let mut pkt = vec![0u8; 10];
        let action = process_tunn_result(
            &udp,
            peer,
            TunnResult::WriteToTunnelV4(&mut pkt, std::net::Ipv4Addr::LOCALHOST),
            &inject_tx,
        )
        .await;
        assert!(matches!(action, TunnAction::Decrypted(_)));
    }

    #[tokio::test]
    async fn process_tunn_result_v4_udp_protocol_extracts_ports() {
        let (udp, peer) = dummy_udp().await;
        let (inject_tx, _rx) = mpsc::channel::<Vec<u8>>(8);
        let mut pkt = vec![0u8; 40];
        pkt[0] = 0x45;
        pkt[9] = 17; // UDP
        pkt[16..20].copy_from_slice(&[192, 168, 1, 1]);
        pkt[20..22].copy_from_slice(&[0x11, 0xD0]); // src port = 4560
        pkt[22..24].copy_from_slice(&[0x12, 0x70]); // dst port = 4720 (discovery)
        let action = process_tunn_result(
            &udp,
            peer,
            TunnResult::WriteToTunnelV4(&mut pkt, std::net::Ipv4Addr::new(10, 99, 0, 2)),
            &inject_tx,
        )
        .await;
        assert!(matches!(action, TunnAction::Decrypted(_)));
    }

    #[tokio::test]
    async fn process_tunn_result_v4_unknown_protocol_skips_port_extraction() {
        let (udp, peer) = dummy_udp().await;
        let (inject_tx, _rx) = mpsc::channel::<Vec<u8>>(8);
        let mut pkt = vec![0u8; 40];
        pkt[0] = 0x45;
        pkt[9] = 1; // ICMP — no port extraction
        pkt[16..20].copy_from_slice(&[192, 168, 1, 1]);
        let action = process_tunn_result(
            &udp,
            peer,
            TunnResult::WriteToTunnelV4(&mut pkt, std::net::Ipv4Addr::new(10, 99, 0, 2)),
            &inject_tx,
        )
        .await;
        assert!(matches!(action, TunnAction::Decrypted(_)));
    }

    // ── handle_decrypted_packet() ─────────────────────────────────────────────

    fn make_test_tunn() -> Tunn {
        use rand_core::OsRng;
        use x25519_dalek::{PublicKey, StaticSecret};
        let server_private = StaticSecret::random_from_rng(OsRng);
        let client_private = StaticSecret::random_from_rng(OsRng);
        let client_public = PublicKey::from(&client_private);
        Tunn::new(server_private, client_public, None, None, 0, None)
    }

    /// A TCP packet to port 80 doesn't match DNS, discovery, or ICMP — it's a no-op.
    #[tokio::test]
    async fn handle_decrypted_packet_noop_for_plain_tcp() {
        let (udp, peer) = dummy_udp().await;
        let upstream_ip = [192, 168, 1, 200u8];
        let mut tunn = make_test_tunn();
        let mut pkt = vec![0u8; 40];
        pkt[0] = 0x45; // IPv4, IHL=5
        pkt[9] = 6; // TCP
        pkt[16..20].copy_from_slice(&upstream_ip);
        pkt[22..24].copy_from_slice(&[0x00, 0x50]); // dst port 80
        // Should complete without panic.
        handle_decrypted_packet(&pkt, upstream_ip, "{}", &mut tunn, &udp, peer).await;
    }

    /// An ICMP echo request to the upstream IP should trigger the ICMP handler path.
    #[tokio::test]
    async fn handle_decrypted_packet_handles_icmp_echo() {
        let (udp, peer) = dummy_udp().await;
        let upstream_ip = [10, 99, 0, 1u8];
        let mut tunn = make_test_tunn();
        // Minimal ICMP echo request: 28+ bytes, protocol=1, dst=upstream_ip, ICMP type=8.
        let mut pkt = vec![0u8; 28];
        pkt[0] = 0x45; // IPv4, IHL=5
        pkt[9] = 1; // ICMP
        pkt[16..20].copy_from_slice(&upstream_ip); // dst = upstream
        pkt[20] = 8; // ICMP echo request
        pkt[21] = 0; // code
        // No real checksum — handle_icmp_echo builds the reply directly.
        handle_decrypted_packet(&pkt, upstream_ip, "{}", &mut tunn, &udp, peer).await;
    }

    /// An ICMP echo request to a different IP is silently ignored.
    #[tokio::test]
    async fn handle_decrypted_packet_ignores_icmp_to_wrong_dest() {
        let (udp, peer) = dummy_udp().await;
        let upstream_ip = [10, 99, 0, 1u8];
        let mut tunn = make_test_tunn();
        let mut pkt = vec![0u8; 28];
        pkt[0] = 0x45;
        pkt[9] = 1; // ICMP
        pkt[16..20].copy_from_slice(&[1, 2, 3, 4]); // wrong dst
        pkt[20] = 8; // echo request
        handle_decrypted_packet(&pkt, upstream_ip, "{}", &mut tunn, &udp, peer).await;
    }

    // ── wireguard fetch_device_info_tcp (timeout path) ────────────────────────

    /// fetch_device_info_tcp with an unreachable address falls back to default JSON.
    #[tokio::test]
    async fn fetch_device_info_tcp_falls_back_when_unreachable() {
        // 127.0.0.1 should not have a Seestar on port 4700 in the test environment.
        let result = fetch_device_info_tcp("127.0.0.1".parse().unwrap(), 1).await;
        // Whether it gets a connection or not, it must return a valid JSON string.
        assert!(!result.is_empty(), "must return non-empty fallback JSON");
        let _v: serde_json::Value =
            serde_json::from_str(&result).expect("fallback must be valid JSON");
    }

    // ── detect_local_ip() ─────────────────────────────────────────────────────

    #[test]
    fn detect_local_ip_returns_non_loopback_or_none() {
        // This may return None in CI environments without a default route,
        // or Some(ip) on machines with internet access. Either is valid.
        match detect_local_ip() {
            Some(ip) => {
                let parsed: std::net::IpAddr = ip.parse().expect("must be a valid IP");
                assert!(!parsed.is_loopback(), "must not return loopback");
                assert!(!parsed.is_unspecified(), "must not return 0.0.0.0");
            }
            None => {
                // Fine — no default route available in this environment.
            }
        }
    }

    // ── default_device_info() ─────────────────────────────────────────────────

    #[test]
    fn default_device_info_is_valid_json() {
        let s = default_device_info();
        let v: serde_json::Value = serde_json::from_str(&s).expect("must be valid JSON");
        assert_eq!(v["method"], "scan_iscope");
        assert_eq!(v["code"], 0);
        assert_eq!(v["id"], 201);
        assert_eq!(v["result"]["sn"], "proxy");
        assert_eq!(v["result"]["is_verified"], true);
    }

    #[test]
    fn default_device_info_contains_product_model() {
        let s = default_device_info();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let model = v["result"]["product_model"].as_str().unwrap();
        assert!(
            model.contains("Seestar"),
            "product_model must mention Seestar"
        );
    }
}

/// Handle a decrypted IP packet: check for discovery broadcasts and ICMP pings.
/// If matched, encrypt and send the response back through the tunnel.
async fn handle_decrypted_packet(
    pkt: &[u8],
    upstream_ip: [u8; 4],
    device_info_json: &str,
    tunn: &mut Tunn,
    udp: &UdpSocket,
    peer: SocketAddr,
) {
    // Check for DNS query — intercept Seestar-related names.
    if let Some(response) = dns::handle_dns_query(pkt, upstream_ip) {
        let mut enc = vec![0u8; 65536];
        if let TunnResult::WriteToNetwork(data) = tunn.encapsulate(&response, &mut enc) {
            let _ = udp.send_to(data, peer).await;
        }
        return;
    }

    // Check for UDP discovery broadcast.
    if let Some(response) = tunnel_discovery::handle_discovery(pkt, upstream_ip, device_info_json) {
        let mut enc = vec![0u8; 65536];
        if let TunnResult::WriteToNetwork(data) = tunn.encapsulate(&response, &mut enc) {
            let _ = udp.send_to(data, peer).await;
        }
        return;
    }

    // Check for ICMP echo request (ping).
    if let Some(reply) = tunnel_discovery::handle_icmp_echo(pkt, upstream_ip) {
        let mut enc = vec![0u8; 65536];
        if let TunnResult::WriteToNetwork(data) = tunn.encapsulate(&reply, &mut enc) {
            let _ = udp.send_to(data, peer).await;
        }
    }
}
