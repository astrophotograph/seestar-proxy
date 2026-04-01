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

use boringtun::noise::{Tunn, TunnResult, errors::WireGuardError};
use keys::{WgKeypair, generate_client_keypair, private_key_b64, public_key_b64};
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

    // Generate client keys (for the QR config).
    let (client_private, client_public) = generate_client_keypair();
    let client_priv_b64 = private_key_b64(&client_private);
    let client_pub_b64 = public_key_b64(&client_public);

    // Determine endpoint for client config.
    let endpoint_str = endpoint.unwrap_or_else(|| {
        format!("{}:{}", bind_addr, port)
    });

    // Build client config and QR.
    let allowed_ips = format!("{}/32", upstream_ip);
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

    // Spawn the packet processing loop with netstack integration.
    tokio::spawn(packet_loop(udp, tunn, net_channels));

    Ok(wg_info)
}

/// Main packet processing loop.
///
/// Receives encrypted UDP datagrams, decrypts via boringtun, and processes
/// the resulting IP packets.
async fn packet_loop(
    udp: UdpSocket,
    mut tunn: Tunn,
    mut net: netstack::NetStackChannels,
) {
    let mut recv_buf = vec![0u8; 65536];
    let mut peer_addr: Option<SocketAddr> = None;

    info!("WireGuard packet loop started (with TCP stack)");

    let mut timer = tokio::time::interval(std::time::Duration::from_millis(250));

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

                // Decrypt — loop to handle chained results.
                let mut dst = vec![0u8; 65536];
                loop {
                    let tunn_result = tunn.decapsulate(None, &recv_buf[..len], &mut dst);
                    match process_tunn_result(&udp, src, tunn_result, &net.inject_tx).await {
                        TunnAction::Continue => {
                            let cont = tunn.decapsulate(None, &[], &mut dst);
                            if matches!(process_tunn_result(&udp, src, cont, &net.inject_tx).await, TunnAction::Done) {
                                break;
                            }
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

                if proto == 6 && data.len() >= 24 {
                    let dst_port = u16::from_be_bytes([data[22], data[23]]);
                    debug!(
                        "WireGuard decrypted: {} -> {}.{}.{}.{}:{} ({} bytes)",
                        proto_name,
                        dst_ip[0], dst_ip[1], dst_ip[2], dst_ip[3],
                        dst_port,
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
            if inject_tx.try_send(data.to_vec()).is_err() {
                warn!("NetStack inject channel full, dropping packet");
            }
            TunnAction::Done
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
}
