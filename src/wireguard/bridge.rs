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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wireguard::netstack::TunnelTcpStream;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    fn make_tunnel_stream(
        dest_port: u16,
    ) -> (
        TunnelTcpStream,
        tokio::sync::mpsc::Sender<Vec<u8>>,
        tokio::sync::mpsc::Receiver<Vec<u8>>,
    ) {
        let (from_tunnel_tx, from_tunnel_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let (to_tunnel_tx, to_tunnel_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let stream = TunnelTcpStream {
            from_tunnel_rx,
            to_tunnel_tx,
            dest_port,
        };
        (stream, from_tunnel_tx, to_tunnel_rx)
    }

    #[tokio::test]
    async fn bridge_to_local_fails_gracefully_when_port_not_listening() {
        // Port 1 is privileged and never listening — bridge should log an error and return.
        let (stream, _from_tx, _to_rx) = make_tunnel_stream(1);
        // Should complete without panic (error is logged internally).
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            bridge_to_local(stream, 1),
        )
        .await
        .expect("bridge must return promptly on connection failure");
    }

    #[tokio::test]
    async fn bridge_forwards_data_from_tunnel_to_local() {
        // Spin up a local TCP listener.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let (stream, from_tunnel_tx, _to_rx) = make_tunnel_stream(port);

        // Accept and read from the local side in a separate task.
        let accept_task = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 64];
            let n = conn.read(&mut buf).await.unwrap();
            buf[..n].to_vec()
        });

        // Start the bridge and send data from the tunnel side.
        let bridge_task = tokio::spawn(bridge_to_local(stream, port));

        // Give the bridge a moment to connect.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        from_tunnel_tx.send(b"hello".to_vec()).await.unwrap();

        // Close the sender so the bridge eventually terminates.
        drop(from_tunnel_tx);

        let received = tokio::time::timeout(std::time::Duration::from_secs(2), accept_task)
            .await
            .expect("timed out waiting for data")
            .unwrap();

        assert_eq!(received, b"hello");
        bridge_task.abort();
    }

    #[tokio::test]
    async fn bridge_forwards_data_from_local_to_tunnel() {
        use tokio::io::AsyncWriteExt;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let (stream, _from_tx, mut to_rx) = make_tunnel_stream(port);

        // Accept connection and write data back to the tunnel.
        let accept_task = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            conn.write_all(b"world").await.unwrap();
            // Keep connection open until bridge is done.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });

        let bridge_task = tokio::spawn(bridge_to_local(stream, port));

        // Wait for data to arrive on the tunnel side.
        let received = tokio::time::timeout(std::time::Duration::from_secs(2), to_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");

        assert_eq!(received, b"world");

        bridge_task.abort();
        accept_task.abort();
    }
}
