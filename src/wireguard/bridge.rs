//! Bridge between tunnel TCP streams and the proxy's control/imaging handlers.
//!
//! When the netstack accepts a TCP connection from the WireGuard tunnel,
//! we create a pair of local TCP connections (via localhost) and pipe data
//! between the tunnel stream and the local proxy port. This avoids
//! refactoring control/imaging to accept generic streams.

use super::netstack::TunnelTcpStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, error, info};

/// Bridge a tunnel TCP connection to a local proxy port.
///
/// Connects to `127.0.0.1:<local_port>` and pipes data bidirectionally
/// between the tunnel stream and the local TCP connection.
pub async fn bridge_to_local(mut tunnel: TunnelTcpStream, local_port: u16) {
    let local_addr = format!("127.0.0.1:{}", local_port);
    let local_stream = match TcpStream::connect(&local_addr).await {
        Ok(s) => s,
        Err(e) => {
            error!(
                "Failed to connect tunnel client to local proxy port {}: {}",
                local_port, e
            );
            return;
        }
    };

    let _ = local_stream.set_nodelay(true);
    info!(
        "Bridging WireGuard tunnel connection to local port {}",
        local_port
    );

    let (mut local_read, mut local_write) = local_stream.into_split();

    // Tunnel → Local proxy
    let to_tunnel_tx = tunnel.to_tunnel_tx.clone();
    let tunnel_to_local = tokio::spawn(async move {
        while let Some(data) = tunnel.from_tunnel_rx.recv().await {
            if local_write.write_all(&data).await.is_err() {
                break;
            }
            let _ = local_write.flush().await;
        }
        debug!("Tunnel -> local pipe closed");
    });

    // Local proxy → Tunnel
    let local_to_tunnel = tokio::spawn(async move {
        let mut buf = vec![0u8; 32768];
        loop {
            let n = match local_read.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            if to_tunnel_tx.send(buf[..n].to_vec()).await.is_err() {
                break;
            }
        }
        debug!("Local -> tunnel pipe closed");
    });

    let _ = tokio::join!(tunnel_to_local, local_to_tunnel);
    info!("WireGuard tunnel bridge closed for port {}", local_port);
}
