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
