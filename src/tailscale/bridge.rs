//! TCP bridge: Tailscale connection ↔ localhost proxy port.

use tokio::net::TcpStream;
use tracing::{debug, error};

/// Bridge a Tailscale-accepted connection to a local proxy port.
///
/// Converts the blocking `std::net::TcpStream` from libtailscale to a
/// tokio async stream, connects to the local proxy port, and copies
/// data bidirectionally until either side closes.
pub async fn bridge_to_local(stream: std::net::TcpStream, local_port: u16) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "unknown".into());

    // Convert blocking stream to async.
    if let Err(e) = stream.set_nonblocking(true) {
        error!("Tailscale bridge: failed to set nonblocking: {}", e);
        return;
    }
    let ts_stream = match TcpStream::from_std(stream) {
        Ok(s) => s,
        Err(e) => {
            error!("Tailscale bridge: failed to convert stream: {}", e);
            return;
        }
    };

    // Connect to the local proxy port.
    let local_addr = format!("127.0.0.1:{}", local_port);
    let local_stream = match TcpStream::connect(&local_addr).await {
        Ok(s) => s,
        Err(e) => {
            error!(
                "Tailscale bridge: failed to connect to {}: {}",
                local_addr, e
            );
            return;
        }
    };

    debug!("Tailscale bridge: {} → 127.0.0.1:{}", peer, local_port);

    let (mut ts_read, mut ts_write) = ts_stream.into_split();
    let (mut local_read, mut local_write) = local_stream.into_split();

    let ts_to_local = tokio::io::copy(&mut ts_read, &mut local_write);
    let local_to_ts = tokio::io::copy(&mut local_read, &mut ts_write);

    tokio::select! {
        r = ts_to_local => {
            if let Err(e) = r { debug!("Tailscale→local closed: {}", e); }
        }
        r = local_to_ts => {
            if let Err(e) = r { debug!("Local→Tailscale closed: {}", e); }
        }
    }

    debug!("Tailscale bridge: {} closed", peer);
}
