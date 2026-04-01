//! Minimal DNS interceptor for the WireGuard tunnel.
//!
//! Intercepts DNS queries from tunnel clients, responds locally for
//! Seestar-related names, and forwards everything else to an upstream
//! DNS server.

use std::net::SocketAddr;
use tokio::net::UdpSocket;
use tracing::{debug, info};

/// DNS header is always 12 bytes.
const DNS_HEADER_LEN: usize = 12;

/// Extract the query name from a DNS question section.
/// DNS names are encoded as length-prefixed labels: [3]www[7]example[3]com[0]
fn parse_query_name(data: &[u8]) -> Option<(String, usize)> {
    if data.len() < DNS_HEADER_LEN + 1 {
        return None;
    }

    let mut name = String::new();
    let mut pos = DNS_HEADER_LEN;

    loop {
        if pos >= data.len() {
            return None;
        }
        let label_len = data[pos] as usize;
        if label_len == 0 {
            pos += 1; // Skip the null terminator.
            break;
        }
        if pos + 1 + label_len > data.len() {
            return None;
        }
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(&String::from_utf8_lossy(&data[pos + 1..pos + 1 + label_len]));
        pos += 1 + label_len;
    }

    // Skip QTYPE (2 bytes) and QCLASS (2 bytes).
    if pos + 4 > data.len() {
        return None;
    }
    let end = pos + 4;

    Some((name.to_lowercase(), end))
}

/// Check if a DNS query name matches Seestar-related patterns.
pub fn is_seestar_name(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("seestar")
        || n.starts_with("s30_")
        || n.starts_with("s50_")
        || n.starts_with("s30-")
        || n.starts_with("s50-")
        || n == "seestar.local"
}

/// Build a DNS A-record response for a query.
///
/// The response reuses the query's ID and question section,
/// adding a single A record pointing to the given IPv4 address.
pub fn build_dns_response(query: &[u8], ip: [u8; 4]) -> Option<Vec<u8>> {
    let (name, question_end) = parse_query_name(query)?;
    if query.len() < DNS_HEADER_LEN {
        return None;
    }

    let query_id = &query[0..2];
    let question_section = &query[DNS_HEADER_LEN..question_end];

    // Build response.
    let mut resp = Vec::with_capacity(question_end + 16);

    // Header: copy ID, set flags for standard response with no error.
    resp.extend_from_slice(query_id);
    resp.extend_from_slice(&[
        0x81, 0x80, // Flags: QR=1 (response), RD=1, RA=1
        0x00, 0x01, // QDCOUNT: 1
        0x00, 0x01, // ANCOUNT: 1
        0x00, 0x00, // NSCOUNT: 0
        0x00, 0x00, // ARCOUNT: 0
    ]);

    // Question section (copied from query).
    resp.extend_from_slice(question_section);

    // Answer section: A record.
    // Name pointer to the question name (offset 12 = 0xC00C).
    resp.extend_from_slice(&[0xC0, 0x0C]);
    resp.extend_from_slice(&[
        0x00, 0x01, // TYPE: A
        0x00, 0x01, // CLASS: IN
        0x00, 0x00, 0x00, 0x3C, // TTL: 60 seconds
        0x00, 0x04, // RDLENGTH: 4
    ]);
    resp.extend_from_slice(&ip); // RDATA: IPv4 address

    debug!("DNS: responding to '{}' with {}.{}.{}.{}", name, ip[0], ip[1], ip[2], ip[3]);

    Some(resp)
}

/// Handle a DNS query from the tunnel.
///
/// If the query is for a Seestar-related name, returns a local response.
/// Otherwise, forwards to the upstream DNS server and returns the response.
///
/// `ip_packet` is the full IPv4 packet containing the UDP DNS query.
/// Returns `Some(response_ip_packet)` if handled.
pub fn handle_dns_query(
    ip_packet: &[u8],
    upstream_ip: [u8; 4],
) -> Option<Vec<u8>> {
    if ip_packet.len() < 28 {
        return None;
    }

    let proto = ip_packet[9];
    if proto != 17 {
        return None; // Not UDP
    }

    let ihl = (ip_packet[0] & 0x0f) as usize * 4;
    if ip_packet.len() < ihl + 8 {
        return None;
    }

    let udp = &ip_packet[ihl..];
    let dst_port = u16::from_be_bytes([udp[2], udp[3]]);

    if dst_port != 53 {
        return None; // Not DNS
    }

    let src_ip = [ip_packet[12], ip_packet[13], ip_packet[14], ip_packet[15]];
    let src_port = u16::from_be_bytes([udp[0], udp[1]]);
    let udp_len = u16::from_be_bytes([udp[4], udp[5]]) as usize;

    if udp.len() < udp_len || udp_len < 8 {
        return None;
    }

    let dns_payload = &udp[8..udp_len];

    // Parse the query name.
    let (name, _) = parse_query_name(dns_payload)?;

    if !is_seestar_name(&name) {
        return None; // Not a Seestar query — let it pass through.
    }

    info!("DNS: intercepting query for '{}' — responding with Seestar IP", name);

    // Build DNS response.
    let dns_response = build_dns_response(dns_payload, upstream_ip)?;

    // Wrap in a UDP/IP packet back to the client.
    let dst_ip = [ip_packet[16], ip_packet[17], ip_packet[18], ip_packet[19]]; // DNS server IP
    let response_packet = super::tunnel_discovery::build_discovery_response(
        dst_ip,     // FROM: the DNS server IP the client queried
        src_ip,     // TO: the client
        src_port,   // TO: the client's source port
        &dns_response,
    );

    Some(response_packet)
}

/// Forward a DNS query to an upstream server and return the response.
/// This is used for non-Seestar queries to keep the phone's DNS working.
pub async fn forward_dns_query(
    dns_payload: &[u8],
    upstream_dns: SocketAddr,
) -> Option<Vec<u8>> {
    let socket = UdpSocket::bind("0.0.0.0:0").await.ok()?;
    socket.send_to(dns_payload, upstream_dns).await.ok()?;

    let mut buf = vec![0u8; 4096];
    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        socket.recv_from(&mut buf),
    ).await {
        Ok(Ok((n, _))) => Some(buf[..n].to_vec()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_query_name() {
        // Build a DNS query for "seestar.local"
        let mut pkt = vec![0u8; 12]; // header
        // Question: [7]seestar[5]local[0] + QTYPE(A) + QCLASS(IN)
        pkt.extend_from_slice(&[7]);
        pkt.extend_from_slice(b"seestar");
        pkt.extend_from_slice(&[5]);
        pkt.extend_from_slice(b"local");
        pkt.push(0); // terminator
        pkt.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]); // A, IN

        let (name, end) = parse_query_name(&pkt).unwrap();
        assert_eq!(name, "seestar.local");
        assert_eq!(end, pkt.len());
    }

    #[test]
    fn is_seestar_name_matches() {
        assert!(is_seestar_name("seestar.local"));
        assert!(is_seestar_name("S30_cfcf05c4.local"));
        assert!(is_seestar_name("s50_abcdef.local"));
        assert!(is_seestar_name("my-seestar.example.com"));
        assert!(!is_seestar_name("google.com"));
        assert!(!is_seestar_name("apple.com"));
    }

    #[test]
    fn build_dns_response_valid() {
        let mut query = vec![
            0x12, 0x34, // ID
            0x01, 0x00, // Flags: standard query
            0x00, 0x01, // QDCOUNT: 1
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // other counts
        ];
        // Question: seestar.local A IN
        query.extend_from_slice(&[7]);
        query.extend_from_slice(b"seestar");
        query.extend_from_slice(&[5]);
        query.extend_from_slice(b"local");
        query.push(0);
        query.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]);

        let resp = build_dns_response(&query, [192, 168, 42, 41]).unwrap();

        // Check ID matches.
        assert_eq!(&resp[0..2], &[0x12, 0x34]);
        // Check QR flag set (response).
        assert_eq!(resp[2] & 0x80, 0x80);
        // Check ANCOUNT = 1.
        assert_eq!(&resp[6..8], &[0x00, 0x01]);
        // Check answer has the IP at the end.
        let ip_start = resp.len() - 4;
        assert_eq!(&resp[ip_start..], &[192, 168, 42, 41]);
    }

    #[test]
    fn handle_dns_query_intercepts_seestar() {
        // Build an IP/UDP/DNS packet querying seestar.local
        let mut dns = vec![
            0xAB, 0xCD, // ID
            0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        dns.extend_from_slice(&[7]);
        dns.extend_from_slice(b"seestar");
        dns.extend_from_slice(&[5]);
        dns.extend_from_slice(b"local");
        dns.push(0);
        dns.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]);

        let udp_len: u16 = 8 + dns.len() as u16;
        let total_len: u16 = 20 + udp_len;
        let mut pkt = vec![0u8; total_len as usize];
        pkt[0] = 0x45;
        pkt[2..4].copy_from_slice(&total_len.to_be_bytes());
        pkt[9] = 17; // UDP
        pkt[12..16].copy_from_slice(&[10, 99, 0, 2]); // client
        pkt[16..20].copy_from_slice(&[1, 1, 1, 1]);   // DNS server
        pkt[20..22].copy_from_slice(&12345u16.to_be_bytes()); // src port
        pkt[22..24].copy_from_slice(&53u16.to_be_bytes()); // dst port = DNS
        pkt[24..26].copy_from_slice(&udp_len.to_be_bytes());
        pkt[28..28 + dns.len()].copy_from_slice(&dns);

        let response = handle_dns_query(&pkt, [192, 168, 42, 41]).unwrap();
        // Should be a valid IP packet back to the client.
        assert!(response.len() > 28);
        assert_eq!(response[9], 17); // UDP
        assert_eq!(&response[12..16], &[1, 1, 1, 1]); // from DNS server
        assert_eq!(&response[16..20], &[10, 99, 0, 2]); // to client
    }

    #[test]
    fn handle_dns_query_ignores_non_seestar() {
        let mut dns = vec![
            0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        dns.extend_from_slice(&[6]);
        dns.extend_from_slice(b"google");
        dns.extend_from_slice(&[3]);
        dns.extend_from_slice(b"com");
        dns.push(0);
        dns.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]);

        let udp_len: u16 = 8 + dns.len() as u16;
        let total_len: u16 = 20 + udp_len;
        let mut pkt = vec![0u8; total_len as usize];
        pkt[0] = 0x45;
        pkt[2..4].copy_from_slice(&total_len.to_be_bytes());
        pkt[9] = 17;
        pkt[12..16].copy_from_slice(&[10, 99, 0, 2]);
        pkt[16..20].copy_from_slice(&[1, 1, 1, 1]);
        pkt[20..22].copy_from_slice(&12345u16.to_be_bytes());
        pkt[22..24].copy_from_slice(&53u16.to_be_bytes());
        pkt[24..26].copy_from_slice(&udp_len.to_be_bytes());
        pkt[28..28 + dns.len()].copy_from_slice(&dns);

        assert!(handle_dns_query(&pkt, [192, 168, 42, 41]).is_none());
    }
}
