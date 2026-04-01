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
        &allowed_ips,
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
        None,   // preshared key
        None,   // keepalive interval (client sets this)
        0,      // tunnel index
        None,   // rate limiter
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
    tokio::spawn(packet_loop(udp, tunn, net_channels, upstream_v4.octets(), device_info_json));

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
    }).await {
        Ok(Ok(response)) => {
            info!("Cached native discovery response: {}", &response[..response.len().min(200)]);
            response
        }
        _ => {
            warn!("Discovery probe to {} timed out, building response from get_device_state", upstream_ip);
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
        writer.write_all(b"{\"id\":999,\"method\":\"get_device_state\",\"params\":[\"verify\"]}\r\n").await?;
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        Ok::<String, anyhow::Error>(line)
    }).await {
        Ok(Ok(response)) => {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(response.trim()) {
                let result = parsed.get("result").cloned().unwrap_or(serde_json::json!({}));
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
                info!("Built discovery response from device state: {}", discovery_response);
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
    }).to_string()
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
                    match process_tunn_result(&udp, src, probe_result, &net.inject_tx).await {
                        TunnAction::Decrypted(pkt) => {
                            handle_decrypted_packet(&pkt, upstream_ip_bytes, &device_info_json, &mut tunn, &udp, src).await;
                        }
                        _ => {}
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
                    if let TunnResult::WriteToNetwork(data) = result {
                        if let Err(e) = udp.send_to(data, addr).await {
                            warn!("WireGuard send error: {}", e);
                        }
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
                if session_active {
                    if let Some(addr) = peer_addr {
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
                        data[12], data[13], data[14], data[15], src_port,
                        dst_ip[0], dst_ip[1], dst_ip[2], dst_ip[3], dst_port,
                        data.len()
                    );
                } else {
                    debug!(
                        "WireGuard decrypted: {} -> {}.{}.{}.{} ({} bytes)",
                        proto_name,
                        dst_ip[0], dst_ip[1], dst_ip[2], dst_ip[3],
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
