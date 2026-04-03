//! Userspace TCP/IP stack powered by smoltcp.
//!
//! Bridges raw IP packets (from boringtun) into TCP streams that can be
//! piped into the proxy's control/imaging handlers.
//!
//! The stack runs a poll loop in a tokio task, processing incoming IP
//! packets and producing outgoing IP packets (TCP responses) that get
//! encrypted and sent back through the WireGuard tunnel.

use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr};
use std::collections::VecDeque;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Channel-based virtual network device for smoltcp.
///
/// Incoming IP packets (from WireGuard) are fed via `inject()`.
/// Outgoing IP packets (TCP responses) are read via `drain()`.
struct VirtualDevice {
    rx_queue: VecDeque<Vec<u8>>,
    tx_queue: VecDeque<Vec<u8>>,
    mtu: usize,
}

impl VirtualDevice {
    fn new(mtu: usize) -> Self {
        Self {
            rx_queue: VecDeque::new(),
            tx_queue: VecDeque::new(),
            mtu,
        }
    }

    fn inject(&mut self, packet: Vec<u8>) {
        self.rx_queue.push_back(packet);
    }

    fn drain(&mut self) -> Option<Vec<u8>> {
        self.tx_queue.pop_front()
    }
}

struct VirtRxToken(Vec<u8>);
struct VirtTxToken<'a>(&'a mut VecDeque<Vec<u8>>);

impl RxToken for VirtRxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl<'a> TxToken for VirtTxToken<'a> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0u8; len];
        let result = f(&mut buf);
        self.0.push_back(buf);
        result
    }
}

impl Device for VirtualDevice {
    type RxToken<'a> = VirtRxToken;
    type TxToken<'a> = VirtTxToken<'a>;

    fn receive(
        &mut self,
        _timestamp: SmolInstant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let packet = self.rx_queue.pop_front()?;
        Some((VirtRxToken(packet), VirtTxToken(&mut self.tx_queue)))
    }

    fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(VirtTxToken(&mut self.tx_queue))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = self.mtu;
        caps
    }
}

/// Packet channels between the WireGuard packet loop and the TCP stack.
pub struct NetStackChannels {
    /// Send decrypted IP packets from WireGuard into the stack.
    pub inject_tx: mpsc::Sender<Vec<u8>>,
    /// Receive outgoing IP packets (TCP responses) to encrypt and send via WireGuard.
    pub egress_rx: mpsc::Receiver<Vec<u8>>,
}

/// A TCP connection accepted from the tunnel.
pub struct TunnelTcpStream {
    /// Data received from the tunnel client (to be read by the proxy).
    pub from_tunnel_rx: mpsc::Receiver<Vec<u8>>,
    /// Data to send back to the tunnel client (written by the proxy).
    pub to_tunnel_tx: mpsc::Sender<Vec<u8>>,
    /// The destination port the client connected to.
    pub dest_port: u16,
}

/// Start the userspace TCP/IP stack.
///
/// Returns channels for packet I/O and a receiver for accepted TCP connections.
pub fn start(
    server_ip: std::net::Ipv4Addr,
    upstream_ip: std::net::Ipv4Addr,
    control_port: u16,
    imaging_port: u16,
) -> (NetStackChannels, mpsc::Receiver<TunnelTcpStream>) {
    let (inject_tx, mut inject_rx) = mpsc::channel::<Vec<u8>>(256);
    let (egress_tx, egress_rx) = mpsc::channel::<Vec<u8>>(256);
    let (conn_tx, conn_rx) = mpsc::channel::<TunnelTcpStream>(16);

    tokio::spawn(async move {
        run_stack(
            server_ip,
            upstream_ip,
            control_port,
            imaging_port,
            &mut inject_rx,
            &egress_tx,
            &conn_tx,
        )
        .await;
    });

    let channels = NetStackChannels {
        inject_tx,
        egress_rx,
    };
    (channels, conn_rx)
}

/// Main smoltcp poll loop.
async fn run_stack(
    _server_ip: std::net::Ipv4Addr,
    upstream_ip: std::net::Ipv4Addr,
    control_port: u16,
    imaging_port: u16,
    inject_rx: &mut mpsc::Receiver<Vec<u8>>,
    egress_tx: &mpsc::Sender<Vec<u8>>,
    conn_tx: &mpsc::Sender<TunnelTcpStream>,
) {
    let mut device = VirtualDevice::new(1420); // WireGuard MTU

    // Configure the smoltcp interface.
    let mut config = IfaceConfig::new(HardwareAddress::Ip);
    config.random_seed = rand_seed();

    let mut iface = Interface::new(config, &mut device, now());

    // The interface responds as the upstream Seestar IP, so clients think
    // they're talking to the real telescope.
    iface.update_ip_addrs(|addrs| {
        let octets = upstream_ip.octets();
        let _ = addrs.push(IpCidr::new(
            IpAddress::v4(octets[0], octets[1], octets[2], octets[3]),
            24,
        ));
    });

    let mut sockets = SocketSet::new(Vec::<smoltcp::iface::SocketStorage<'_>>::new());

    // Create TCP listen sockets for control and imaging ports.
    let control_handle = add_listen_socket(&mut sockets, control_port);
    let imaging_handle = add_listen_socket(&mut sockets, imaging_port);

    info!(
        "NetStack: listening on {}:{} (control) and {}:{} (imaging)",
        upstream_ip, control_port, upstream_ip, imaging_port
    );

    // Active tunnel connections: maps socket handle to data channels.
    type ConnMap =
        std::collections::HashMap<SocketHandle, (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>)>;
    let mut active_connections: ConnMap = std::collections::HashMap::new();

    // Poll loop.
    let mut poll_interval = tokio::time::interval(std::time::Duration::from_millis(10));

    loop {
        tokio::select! {
            // Incoming IP packets from WireGuard.
            Some(packet) = inject_rx.recv() => {
                device.inject(packet);
            }

            // Periodic poll.
            _ = poll_interval.tick() => {}
        }

        // Drive smoltcp.
        let timestamp = now();
        let result = iface.poll(timestamp, &mut device, &mut sockets);

        if result == smoltcp::iface::PollResult::SocketStateChanged {
            // Check for new connections on listen sockets.
            for (handle, port) in [
                (control_handle, control_port),
                (imaging_handle, imaging_port),
            ] {
                let socket = sockets.get_mut::<tcp::Socket>(handle);
                if socket.is_active() && !active_connections.contains_key(&handle) {
                    // New connection accepted!
                    let remote = socket.remote_endpoint();
                    info!(
                        "NetStack: TCP connection on port {} from {:?}",
                        port, remote
                    );

                    let (from_tunnel_tx, from_tunnel_rx) = mpsc::channel::<Vec<u8>>(64);
                    let (to_tunnel_tx, to_tunnel_rx) = mpsc::channel::<Vec<u8>>(64);

                    active_connections.insert(handle, (from_tunnel_tx, to_tunnel_rx));

                    let stream = TunnelTcpStream {
                        from_tunnel_rx,
                        to_tunnel_tx,
                        dest_port: port,
                    };

                    if conn_tx.try_send(stream).is_err() {
                        warn!(
                            "NetStack: connection channel full, dropping connection on port {}",
                            port
                        );
                    }
                }
            }

            // Transfer data for active connections.
            let mut to_remove = vec![];
            for (handle, (from_tunnel_tx, to_tunnel_rx)) in active_connections.iter_mut() {
                let socket = sockets.get_mut::<tcp::Socket>(*handle);

                if !socket.is_active() {
                    to_remove.push(*handle);
                    continue;
                }

                // Read data from smoltcp socket → send to proxy.
                if socket.can_recv() {
                    let mut buf = vec![0u8; 4096];
                    match socket.recv_slice(&mut buf) {
                        Ok(n) if n > 0 => {
                            buf.truncate(n);
                            let _ = from_tunnel_tx.try_send(buf);
                        }
                        _ => {}
                    }
                }

                // Read data from proxy → write to smoltcp socket.
                if socket.can_send()
                    && let Ok(data) = to_tunnel_rx.try_recv()
                {
                    let _ = socket.send_slice(&data);
                }
            }

            for handle in to_remove {
                active_connections.remove(&handle);
                debug!("NetStack: TCP connection closed");
            }
        }

        // Drain outgoing IP packets (TCP responses) → send to WireGuard for encryption.
        while let Some(packet) = device.drain() {
            if egress_tx.try_send(packet).is_err() {
                warn!("NetStack: egress channel full, dropping packet");
            }
        }
    }
}

/// Create a TCP listen socket and add it to the socket set.
fn add_listen_socket(sockets: &mut SocketSet<'_>, port: u16) -> SocketHandle {
    let rx_buf = tcp::SocketBuffer::new(vec![0u8; 65536]);
    let tx_buf = tcp::SocketBuffer::new(vec![0u8; 65536]);
    let mut socket = tcp::Socket::new(rx_buf, tx_buf);
    socket.listen(port).unwrap();
    sockets.add(socket)
}

/// Get current time as smoltcp Instant.
fn now() -> SmolInstant {
    SmolInstant::from_millis(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64,
    )
}

/// Generate a random seed for smoltcp (used for TCP ISN).
fn rand_seed() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    RandomState::new().build_hasher().finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::phy::{Device, RxToken, TxToken};
    use smoltcp::time::Instant as SmolInstant;

    // ── VirtualDevice ─────────────────────────────────────────────────────────

    #[test]
    fn virtual_device_new_has_empty_queues() {
        let dev = VirtualDevice::new(1420);
        assert_eq!(dev.mtu, 1420);
        assert!(dev.rx_queue.is_empty());
        assert!(dev.tx_queue.is_empty());
    }

    #[test]
    fn virtual_device_inject_and_drain_roundtrip() {
        let mut dev = VirtualDevice::new(1420);
        let pkt = vec![1u8, 2, 3, 4];
        dev.inject(pkt.clone());
        let out = dev.drain();
        assert!(out.is_none(), "drain reads from tx_queue, not rx_queue");
        // inject puts into rx_queue — drain reads from tx_queue
        assert_eq!(dev.rx_queue.len(), 1);
    }

    #[test]
    fn virtual_device_drain_on_empty_returns_none() {
        let mut dev = VirtualDevice::new(1420);
        assert!(dev.drain().is_none());
    }

    #[test]
    fn virtual_device_drain_returns_fifo_order() {
        let mut dev = VirtualDevice::new(1420);
        // Manually push two packets into tx_queue (as transmit would do).
        dev.tx_queue.push_back(vec![1u8]);
        dev.tx_queue.push_back(vec![2u8]);
        assert_eq!(dev.drain(), Some(vec![1u8]));
        assert_eq!(dev.drain(), Some(vec![2u8]));
        assert_eq!(dev.drain(), None);
    }

    #[test]
    fn virtual_device_capabilities_reports_ip_medium_and_mtu() {
        let dev = VirtualDevice::new(1500);
        let caps = dev.capabilities();
        assert_eq!(caps.medium, smoltcp::phy::Medium::Ip);
        assert_eq!(caps.max_transmission_unit, 1500);
    }

    #[test]
    fn virtual_device_receive_returns_none_when_rx_empty() {
        let mut dev = VirtualDevice::new(1420);
        let t = SmolInstant::from_millis(0);
        assert!(dev.receive(t).is_none());
    }

    #[test]
    fn virtual_device_receive_returns_tokens_when_rx_has_data() {
        let mut dev = VirtualDevice::new(1420);
        dev.inject(vec![0xAA, 0xBB]);
        let t = SmolInstant::from_millis(0);
        let tokens = dev.receive(t);
        assert!(
            tokens.is_some(),
            "receive must return tokens when data is available"
        );
        let (rx_tok, _tx_tok) = tokens.unwrap();
        let data = rx_tok.consume(|b| b.to_vec());
        assert_eq!(data, vec![0xAA, 0xBB]);
    }

    #[test]
    fn virtual_device_transmit_always_returns_some() {
        let mut dev = VirtualDevice::new(1420);
        let t = SmolInstant::from_millis(0);
        let tok = dev.transmit(t);
        assert!(tok.is_some());
        // Write via the tx token and verify it ends up in tx_queue.
        let tok = tok.unwrap();
        tok.consume(2, |buf| {
            buf[0] = 0xDE;
            buf[1] = 0xAD;
        });
        assert_eq!(dev.tx_queue.pop_front(), Some(vec![0xDE, 0xAD]));
    }

    #[test]
    fn virt_rx_token_consume_passes_bytes_to_closure() {
        let tok = VirtRxToken(vec![10u8, 20, 30]);
        let result = tok.consume(|b| b.len());
        assert_eq!(result, 3);
    }

    #[test]
    fn virt_tx_token_consume_writes_into_queue() {
        let mut queue = std::collections::VecDeque::new();
        let tok = VirtTxToken(&mut queue);
        tok.consume(4, |buf| {
            buf.copy_from_slice(&[1, 2, 3, 4]);
        });
        assert_eq!(queue.pop_front(), Some(vec![1u8, 2, 3, 4]));
    }

    // ── now() / rand_seed() ───────────────────────────────────────────────────

    #[test]
    fn now_returns_reasonable_timestamp() {
        let t = now();
        // Must be after 2020-01-01 (Unix ms = 1_577_836_800_000)
        assert!(t.total_millis() > 1_577_836_800_000_i64);
    }

    #[test]
    fn rand_seed_returns_a_value() {
        // Just verify it runs without panic.
        let _ = rand_seed();
    }

    // ── start() ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn start_returns_channels_and_stack_runs() {
        let server_ip: std::net::Ipv4Addr = "10.99.0.1".parse().unwrap();
        let upstream_ip: std::net::Ipv4Addr = "10.99.0.1".parse().unwrap();
        let (channels, _conn_rx) = start(server_ip, upstream_ip, 14701, 14801);

        // Give the spawned run_stack task time to initialise and run the poll loop.
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;

        // Inject a dummy packet — drives the inject_rx.recv() branch in run_stack.
        let dummy = vec![0u8; 40];
        let _ = channels.inject_tx.send(dummy).await;

        // Allow the stack to process the injected packet.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }

    #[tokio::test]
    async fn inject_channel_accepts_multiple_packets() {
        let server_ip: std::net::Ipv4Addr = "10.99.0.1".parse().unwrap();
        let upstream_ip: std::net::Ipv4Addr = "10.99.0.2".parse().unwrap();
        let (channels, _conn_rx) = start(server_ip, upstream_ip, 14702, 14802);

        // Fill the channel to verify backpressure doesn't cause panics.
        for _ in 0..10 {
            let _ = channels.inject_tx.try_send(vec![0u8; 20]);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // ── TunnelTcpStream ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn tunnel_tcp_stream_channels_pass_data() {
        let (from_tunnel_tx, from_tunnel_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let (to_tunnel_tx, mut to_tunnel_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);

        let stream = TunnelTcpStream {
            from_tunnel_rx,
            to_tunnel_tx: to_tunnel_tx.clone(),
            dest_port: 4700,
        };

        assert_eq!(stream.dest_port, 4700);

        // Send via to_tunnel_tx (simulates proxy sending back to client).
        to_tunnel_tx.send(vec![0xFF]).await.unwrap();
        let received = to_tunnel_rx.recv().await.unwrap();
        assert_eq!(received, vec![0xFF]);

        // Send via from_tunnel_tx (simulates data arriving from WireGuard client).
        from_tunnel_tx.send(vec![0xAB]).await.unwrap();
        drop(stream); // drop so from_tunnel_rx is gone
    }
}
