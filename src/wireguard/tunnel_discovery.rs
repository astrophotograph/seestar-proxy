//! Cross-subnet discovery injection for the WireGuard tunnel.
//!
//! Intercepts UDP packets to port 4720 (Seestar discovery) inside the
//! tunnel and responds with cached device info — making the Seestar
//! appear as a local device to tunnel clients.

use crate::protocol::DISCOVERY_PORT;
use tracing::{debug, info};

/// Check if a decrypted IPv4 packet is a UDP discovery broadcast to port 4720.
/// Returns `Some((src_ip, src_port, payload))` if it is.
pub fn parse_discovery_request(ip_packet: &[u8]) -> Option<([u8; 4], u16, &[u8])> {
    if ip_packet.len() < 28 {
        return None; // Too short for IP + UDP headers
    }

    let proto = ip_packet[9];
    if proto != 17 {
        return None; // Not UDP
    }

    // IP header length (IHL field, lower 4 bits of byte 0, in 32-bit words).
    let ihl = (ip_packet[0] & 0x0f) as usize * 4;
    if ip_packet.len() < ihl + 8 {
        return None; // Too short for UDP header
    }

    let udp = &ip_packet[ihl..];
    let dst_port = u16::from_be_bytes([udp[2], udp[3]]);

    if dst_port != DISCOVERY_PORT {
        return None;
    }

    let src_ip = [ip_packet[12], ip_packet[13], ip_packet[14], ip_packet[15]];
    let src_port = u16::from_be_bytes([udp[0], udp[1]]);
    let udp_len = u16::from_be_bytes([udp[4], udp[5]]) as usize;

    if udp.len() < udp_len {
        return None;
    }

    let payload = &udp[8..udp_len];
    Some((src_ip, src_port, payload))
}

/// Build an IPv4/UDP response packet for a discovery query.
///
/// The response is sent FROM the Seestar's IP back TO the client's IP/port.
pub fn build_discovery_response(
    src_ip: [u8; 4], // Seestar IP (we're pretending to be)
    dst_ip: [u8; 4], // Client IP in the tunnel
    dst_port: u16,   // Client's source port
    payload: &[u8],  // JSON response
) -> Vec<u8> {
    let ip_header_len = 20;
    let udp_header_len = 8;
    let total_len = ip_header_len + udp_header_len + payload.len();

    let mut packet = vec![0u8; total_len];

    // ── IPv4 Header ──
    packet[0] = 0x45; // Version 4, IHL 5 (20 bytes)
    packet[1] = 0x00; // DSCP/ECN
    let total_len_u16 = total_len as u16;
    packet[2..4].copy_from_slice(&total_len_u16.to_be_bytes());
    packet[4..6].copy_from_slice(&[0x00, 0x00]); // Identification
    packet[6..8].copy_from_slice(&[0x40, 0x00]); // Flags (Don't Fragment) + Fragment Offset
    packet[8] = 64; // TTL
    packet[9] = 17; // Protocol: UDP
    // Checksum at [10..12] — computed below
    packet[12..16].copy_from_slice(&src_ip);
    packet[16..20].copy_from_slice(&dst_ip);

    // IP header checksum.
    let checksum = ip_checksum(&packet[..ip_header_len]);
    packet[10..12].copy_from_slice(&checksum.to_be_bytes());

    // ── UDP Header ──
    let udp_start = ip_header_len;
    let udp_len = (udp_header_len + payload.len()) as u16;
    packet[udp_start..udp_start + 2].copy_from_slice(&DISCOVERY_PORT.to_be_bytes()); // Src port
    packet[udp_start + 2..udp_start + 4].copy_from_slice(&dst_port.to_be_bytes()); // Dst port
    packet[udp_start + 4..udp_start + 6].copy_from_slice(&udp_len.to_be_bytes());
    // UDP checksum at [udp_start+6..udp_start+8] — leave as 0 (optional for IPv4)

    // ── Payload ──
    packet[udp_start + 8..].copy_from_slice(payload);

    packet
}

/// Compute IPv4 header checksum.
fn ip_checksum(header: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i < header.len() {
        if i == 10 {
            // Skip the checksum field itself
            i += 2;
            continue;
        }
        let word = if i + 1 < header.len() {
            u16::from_be_bytes([header[i], header[i + 1]])
        } else {
            u16::from_be_bytes([header[i], 0])
        };
        sum += word as u32;
        i += 2;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Try to handle a discovery request in a decrypted IP packet.
/// Returns `Some(response_ip_packet)` if handled.
pub fn handle_discovery(
    ip_packet: &[u8],
    upstream_ip: [u8; 4],
    device_info_json: &str,
) -> Option<Vec<u8>> {
    let (client_ip, client_port, payload) = parse_discovery_request(ip_packet)?;

    let request = std::str::from_utf8(payload).ok()?;
    let json: serde_json::Value = serde_json::from_str(request).ok()?;

    let method = json.get("method").and_then(|v| v.as_str())?;
    if method != "scan_iscope" {
        return None;
    }

    info!(
        "WireGuard tunnel: discovery request from {}.{}.{}.{}:{}",
        client_ip[0], client_ip[1], client_ip[2], client_ip[3], client_port
    );

    // Build response with the upstream device info.
    let response_bytes = device_info_json.as_bytes();
    let response_packet =
        build_discovery_response(upstream_ip, client_ip, client_port, response_bytes);

    debug!(
        "WireGuard tunnel: sending discovery response ({} bytes)",
        response_packet.len()
    );

    Some(response_packet)
}

/// Handle an ICMP echo request (ping) and return an echo reply.
///
/// Returns `Some(response_ip_packet)` if the packet is an ICMP echo request
/// destined for the upstream Seestar IP.
pub fn handle_icmp_echo(ip_packet: &[u8], upstream_ip: [u8; 4]) -> Option<Vec<u8>> {
    if ip_packet.len() < 28 {
        return None;
    }

    let proto = ip_packet[9];
    if proto != 1 {
        return None; // Not ICMP
    }

    let dst_ip = [ip_packet[16], ip_packet[17], ip_packet[18], ip_packet[19]];
    // Only respond to pings for the Seestar IP.
    if dst_ip != upstream_ip {
        return None;
    }

    let ihl = (ip_packet[0] & 0x0f) as usize * 4;
    if ip_packet.len() < ihl + 8 {
        return None;
    }

    let icmp = &ip_packet[ihl..];
    let icmp_type = icmp[0];
    // Type 8 = Echo Request
    if icmp_type != 8 {
        return None;
    }

    let src_ip = [ip_packet[12], ip_packet[13], ip_packet[14], ip_packet[15]];

    debug!(
        "WireGuard tunnel: ICMP echo request from {}.{}.{}.{} -> {}.{}.{}.{}",
        src_ip[0], src_ip[1], src_ip[2], src_ip[3], dst_ip[0], dst_ip[1], dst_ip[2], dst_ip[3],
    );

    // Build the echo reply: swap src/dst IPs, change type 8→0, fix checksums.
    let mut reply = ip_packet.to_vec();

    // Swap IP addresses.
    reply[12..16].copy_from_slice(&dst_ip);
    reply[16..20].copy_from_slice(&src_ip);

    // Zero IP header checksum, recalculate.
    reply[10] = 0;
    reply[11] = 0;
    let ip_cksum = ip_checksum(&reply[..ihl]);
    reply[10..12].copy_from_slice(&ip_cksum.to_be_bytes());

    // Change ICMP type from 8 (request) to 0 (reply).
    reply[ihl] = 0;

    // Zero ICMP checksum, recalculate over entire ICMP message.
    reply[ihl + 2] = 0;
    reply[ihl + 3] = 0;
    let icmp_cksum = internet_checksum(&reply[ihl..]);
    reply[ihl + 2..ihl + 4].copy_from_slice(&icmp_cksum.to_be_bytes());

    Some(reply)
}

/// Internet checksum (RFC 1071) — used for both IP header and ICMP.
fn internet_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_non_udp_returns_none() {
        // TCP packet (proto=6)
        let mut pkt = vec![0u8; 40];
        pkt[0] = 0x45; // IPv4, IHL=5
        pkt[9] = 6; // TCP
        assert!(parse_discovery_request(&pkt).is_none());
    }

    #[test]
    fn parse_udp_wrong_port_returns_none() {
        let mut pkt = vec![0u8; 40];
        pkt[0] = 0x45;
        pkt[9] = 17; // UDP
        // Dst port = 80 (not 4720)
        pkt[22..24].copy_from_slice(&80u16.to_be_bytes());
        // UDP length
        pkt[24..26].copy_from_slice(&12u16.to_be_bytes());
        assert!(parse_discovery_request(&pkt).is_none());
    }

    #[test]
    fn parse_discovery_packet() {
        let payload = b"test";
        let udp_len: u16 = 8 + payload.len() as u16;
        let total_len: u16 = 20 + udp_len;
        let mut pkt = vec![0u8; total_len as usize];
        pkt[0] = 0x45;
        pkt[2..4].copy_from_slice(&total_len.to_be_bytes());
        pkt[9] = 17; // UDP
        pkt[12..16].copy_from_slice(&[10, 99, 0, 2]); // src IP
        // UDP header at offset 20
        pkt[20..22].copy_from_slice(&12345u16.to_be_bytes()); // src port
        pkt[22..24].copy_from_slice(&DISCOVERY_PORT.to_be_bytes()); // dst port
        pkt[24..26].copy_from_slice(&udp_len.to_be_bytes());
        pkt[28..28 + payload.len()].copy_from_slice(payload);

        let (src_ip, src_port, data) = parse_discovery_request(&pkt).unwrap();
        assert_eq!(src_ip, [10, 99, 0, 2]);
        assert_eq!(src_port, 12345);
        assert_eq!(data, b"test");
    }

    #[test]
    fn build_response_has_correct_structure() {
        let payload = b"hello";
        let pkt = build_discovery_response([10, 0, 0, 1], [10, 99, 0, 2], 54321, payload);
        assert_eq!(pkt.len(), 20 + 8 + 5); // IP + UDP + payload
        assert_eq!(pkt[0] & 0xf0, 0x40); // IPv4
        assert_eq!(pkt[9], 17); // UDP
        assert_eq!(&pkt[12..16], &[10, 0, 0, 1]); // src
        assert_eq!(&pkt[16..20], &[10, 99, 0, 2]); // dst
        assert_eq!(u16::from_be_bytes([pkt[22], pkt[23]]), 54321); // dst port
        assert_eq!(&pkt[28..], payload);
    }

    #[test]
    fn handle_discovery_scan_iscope() {
        let scan = serde_json::json!({"id": 201, "method": "scan_iscope", "name": "phone"});
        let scan_bytes = serde_json::to_vec(&scan).unwrap();

        let udp_len: u16 = 8 + scan_bytes.len() as u16;
        let total_len: u16 = 20 + udp_len;
        let mut pkt = vec![0u8; total_len as usize];
        pkt[0] = 0x45;
        pkt[2..4].copy_from_slice(&total_len.to_be_bytes());
        pkt[9] = 17;
        pkt[12..16].copy_from_slice(&[10, 99, 0, 2]);
        pkt[20..22].copy_from_slice(&5555u16.to_be_bytes());
        pkt[22..24].copy_from_slice(&DISCOVERY_PORT.to_be_bytes());
        pkt[24..26].copy_from_slice(&udp_len.to_be_bytes());
        pkt[28..28 + scan_bytes.len()].copy_from_slice(&scan_bytes);

        let device_info = r#"{"id":201,"result":{"sn":"abc","product_model":"Seestar S50"}}"#;
        let response = handle_discovery(&pkt, [10, 0, 0, 1], device_info).unwrap();

        // Verify response is a valid IP packet with the device info.
        assert!(response.len() > 28);
        assert_eq!(response[9], 17); // UDP
        assert_eq!(&response[12..16], &[10, 0, 0, 1]); // from Seestar IP
        assert_eq!(&response[16..20], &[10, 99, 0, 2]); // to client
        let payload = &response[28..];
        let parsed: serde_json::Value = serde_json::from_slice(payload).unwrap();
        assert_eq!(parsed["result"]["sn"], "abc");
    }

    // ── ICMP echo tests ──────────────────────────────────────────────────

    fn build_icmp_echo_request(src: [u8; 4], dst: [u8; 4]) -> Vec<u8> {
        let total_len: u16 = 20 + 8; // IP header + ICMP header (no payload)
        let mut pkt = vec![0u8; total_len as usize];
        pkt[0] = 0x45; // IPv4, IHL=5
        pkt[2..4].copy_from_slice(&total_len.to_be_bytes());
        pkt[8] = 64; // TTL
        pkt[9] = 1; // ICMP
        pkt[12..16].copy_from_slice(&src);
        pkt[16..20].copy_from_slice(&dst);
        // IP checksum
        let cksum = super::ip_checksum(&pkt[..20]);
        pkt[10..12].copy_from_slice(&cksum.to_be_bytes());
        // ICMP: type=8 (echo request), code=0, id=1, seq=1
        pkt[20] = 8;
        pkt[21] = 0;
        pkt[24..26].copy_from_slice(&1u16.to_be_bytes()); // id
        pkt[26..28].copy_from_slice(&1u16.to_be_bytes()); // seq
        // ICMP checksum
        let icmp_cksum = super::internet_checksum(&pkt[20..]);
        pkt[22..24].copy_from_slice(&icmp_cksum.to_be_bytes());
        pkt
    }

    #[test]
    fn icmp_echo_reply_basic() {
        let request = build_icmp_echo_request([10, 99, 0, 2], [192, 168, 42, 41]);
        let reply = handle_icmp_echo(&request, [192, 168, 42, 41]).unwrap();

        assert_eq!(reply.len(), request.len());
        assert_eq!(reply[9], 1); // ICMP
        // Src/dst swapped
        assert_eq!(&reply[12..16], &[192, 168, 42, 41]);
        assert_eq!(&reply[16..20], &[10, 99, 0, 2]);
        // Type 0 = echo reply
        assert_eq!(reply[20], 0);
    }

    #[test]
    fn icmp_echo_wrong_dst_returns_none() {
        let request = build_icmp_echo_request([10, 99, 0, 2], [8, 8, 8, 8]);
        assert!(handle_icmp_echo(&request, [192, 168, 42, 41]).is_none());
    }

    #[test]
    fn icmp_non_echo_returns_none() {
        let mut pkt = build_icmp_echo_request([10, 99, 0, 2], [192, 168, 42, 41]);
        pkt[20] = 3; // Type 3 = destination unreachable, not echo
        assert!(handle_icmp_echo(&pkt, [192, 168, 42, 41]).is_none());
    }

    #[test]
    fn tcp_packet_not_handled_as_icmp() {
        let mut pkt = vec![0u8; 40];
        pkt[0] = 0x45;
        pkt[9] = 6; // TCP
        assert!(handle_icmp_echo(&pkt, [192, 168, 42, 41]).is_none());
    }
}
