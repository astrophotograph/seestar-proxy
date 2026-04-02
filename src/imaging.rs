//! Port 4800 imaging proxy — fans out binary frames from one upstream
//! Seestar connection to all connected clients.
//!
//! The upstream sends an 80-byte header followed by `size` bytes of payload.
//! Each complete frame is broadcast to every connected client.
//!
//! The imaging port also uses JSON-RPC `test_connection` heartbeats (sent
//! as text, not binary) to keep the connection alive.

use crate::metrics::Metrics;
use crate::protocol::{FrameHeader, HEADER_SIZE};
use crate::recorder::Recorder;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

/// Run the imaging proxy.
pub async fn run(
    bind_addr: std::net::SocketAddr,
    upstream_addr: std::net::SocketAddr,
    recorder: Option<Arc<Recorder>>,
    metrics: Option<Arc<Metrics>>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(bind_addr).await?;
    info!("Imaging proxy listening on {}", bind_addr);

    let (frame_tx, _) = broadcast::channel::<Arc<Vec<u8>>>(32);

    // Upstream imaging connection with reconnect loop.
    {
        let frame_tx = frame_tx.clone();
        let recorder = recorder.clone();
        let metrics = metrics.clone();
        tokio::spawn(async move {
            loop {
                info!("Connecting to telescope imaging at {}...", upstream_addr);
                match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    TcpStream::connect(upstream_addr),
                )
                .await
                {
                    Ok(Ok(upstream)) => {
                        let _ = upstream.set_nodelay(true);
                        info!("Connected to telescope imaging at {}", upstream_addr);
                        let (upstream_reader, upstream_writer) = upstream.into_split();
                        // Run reader and heartbeat concurrently; reconnect when either stops.
                        tokio::select! {
                            _ = upstream_reader_task(upstream_reader, frame_tx.clone(), recorder.clone(), metrics.clone()) => {}
                            _ = imaging_heartbeat_task(upstream_writer) => {}
                        }
                        warn!("Telescope imaging connection lost");
                    }
                    Ok(Err(e)) => error!("Failed to connect to telescope imaging: {}", e),
                    Err(_) => error!("Timed out connecting to telescope imaging at {}", upstream_addr),
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        });
    }

    // Accept client connections.
    loop {
        let (client_stream, client_addr) = listener.accept().await?;
        let _ = client_stream.set_nodelay(true);
        info!("Imaging client connected: {}", client_addr);

        let frame_rx = frame_tx.subscribe();
        let metrics_c = metrics.clone();
        if let Some(m) = &metrics {
            m.imaging_clients.fetch_add(1, Ordering::Relaxed);
        }
        tokio::spawn(async move {
            if let Err(e) = handle_client(client_stream, frame_rx).await {
                debug!("Imaging client {} error: {}", client_addr, e);
            }
            if let Some(m) = &metrics_c {
                m.imaging_clients.fetch_sub(1, Ordering::Relaxed);
            }
            info!("Imaging client disconnected: {}", client_addr);
        });
    }
}

/// Send periodic heartbeats on the imaging connection.
async fn imaging_heartbeat_task(mut writer: tokio::net::tcp::OwnedWriteHalf) {
    let mut id: u64 = 1;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let msg = format!(
            r#"{{"id":{},"method":"test_connection","params":"verify"}}"#,
            id
        );
        id += 1;
        debug!("Imaging heartbeat: {}", msg);
        // The imaging port accepts JSON-RPC text for control commands.
        let line = format!("{}\r\n", msg);
        if writer.write_all(line.as_bytes()).await.is_err() {
            error!("Imaging heartbeat write failed");
            break;
        }
        if writer.flush().await.is_err() {
            error!("Imaging heartbeat flush failed");
            break;
        }
    }
    info!("Imaging heartbeat task stopped");
}

/// Read frames from the upstream telescope and broadcast them.
async fn upstream_reader_task(
    mut reader: impl AsyncReadExt + Unpin,
    frame_tx: broadcast::Sender<Arc<Vec<u8>>>,
    recorder: Option<Arc<Recorder>>,
    metrics: Option<Arc<Metrics>>,
) {
    if let Some(m) = &metrics {
        m.upstream_imaging_up.store(true, Ordering::Relaxed);
    }
    let mut header_buf = [0u8; HEADER_SIZE];
    let mut frame_count: u64 = 0;

    loop {
        // Read 80-byte header.
        if let Err(e) = reader.read_exact(&mut header_buf).await {
            error!("Upstream imaging connection lost: {}", e);
            break;
        }

        let header = FrameHeader::parse(&header_buf);

        // Sanity check size.
        if header.size > 50_000_000 {
            error!("Unreasonable frame size: {}", header.size);
            break;
        }

        // Read payload.
        let mut payload = vec![0u8; header.size as usize];
        if let Err(e) = reader.read_exact(&mut payload).await {
            error!("Upstream imaging read error during payload: {}", e);
            break;
        }

        // Record if enabled.
        if let Some(recorder) = &recorder {
            recorder.record_frame(&header_buf, &payload).await;
        }

        if let Some(m) = &metrics {
            m.imaging_frames.fetch_add(1, Ordering::Relaxed);
            m.imaging_bytes.fetch_add((HEADER_SIZE + payload.len()) as u64, Ordering::Relaxed);
            // Log every 30th frame to avoid flooding the traffic log.
            frame_count += 1;
            if frame_count % 30 == 1 {
                let kind = match header.id {
                    20 => "view",
                    21 => "preview",
                    23 => "stack",
                    _  => "frame",
                };
                let summary = if header.is_image() {
                    format!("{} {}x{} ({:.1} KB)", kind, header.width, header.height, header.size as f64 / 1024.0)
                } else {
                    format!("{} ({} bytes)", kind, header.size)
                };
                m.push_log("img", summary);
            }
        }

        // Build complete frame for broadcasting.
        let mut frame = Vec::with_capacity(HEADER_SIZE + payload.len());
        frame.extend_from_slice(&header_buf);
        frame.extend_from_slice(&payload);
        let frame = Arc::new(frame);

        if header.is_image() {
            debug!(
                "Frame: id={} {}x{} ({} bytes)",
                header.id, header.width, header.height, header.size
            );
        }

        let _ = frame_tx.send(frame);
    }

    if let Some(m) = &metrics {
        m.upstream_imaging_up.store(false, Ordering::Relaxed);
    }
    info!("Upstream imaging reader stopped");
}

/// Forward broadcast frames to a single client.
async fn handle_client(
    mut stream: TcpStream,
    mut frame_rx: broadcast::Receiver<Arc<Vec<u8>>>,
) -> anyhow::Result<()> {
    loop {
        let frame = match frame_rx.recv().await {
            Ok(f) => f,
            // The client fell behind and missed some frames. Skip them and
            // continue — dropping frames is preferable to dropping the client.
            Err(broadcast::error::RecvError::Lagged(n)) => {
                debug!("Imaging client lagged, skipped {} frames", n);
                continue;
            }
            // The sender was dropped; no more frames will arrive.
            Err(broadcast::error::RecvError::Closed) => break,
        };
        if stream.write_all(&frame).await.is_err() {
            break;
        }
        let _ = stream.flush().await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::HEADER_SIZE;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// Returns (writer_end, server_stream): write to writer_end,
    /// pass server_stream to upstream_reader_task.
    async fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let writer_end = TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        (writer_end, server)
    }

    fn make_header(size: u32, id: u8, width: u16, height: u16) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        buf[6..10].copy_from_slice(&size.to_be_bytes());
        buf[15] = id;
        buf[16..18].copy_from_slice(&width.to_be_bytes());
        buf[18..20].copy_from_slice(&height.to_be_bytes());
        buf
    }

    // ── upstream_reader_task ──────────────────────────────────────────────────

    #[tokio::test]
    async fn broadcasts_frame_to_multiple_subscribers() {
        let (mut mock_telescope, server) = loopback_pair().await;
        let (frame_tx, _) = broadcast::channel::<Arc<Vec<u8>>>(16);

        let mut rx1 = frame_tx.subscribe();
        let mut rx2 = frame_tx.subscribe();

        tokio::spawn(upstream_reader_task(server, frame_tx, None, None));

        let header = make_header(5, 0, 0, 0);
        mock_telescope.write_all(&header).await.unwrap();
        mock_telescope.write_all(b"hello").await.unwrap();
        mock_telescope.flush().await.unwrap();

        let f1 = tokio::time::timeout(Duration::from_secs(1), rx1.recv())
            .await
            .unwrap()
            .unwrap();
        let f2 = tokio::time::timeout(Duration::from_secs(1), rx2.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(f1.len(), HEADER_SIZE + 5);
        assert_eq!(f2.len(), HEADER_SIZE + 5);
        assert_eq!(&f1[HEADER_SIZE..], b"hello");
        assert_eq!(&f2[HEADER_SIZE..], b"hello");
        // Both receivers get the exact same Arc allocation
        assert!(Arc::ptr_eq(&f1, &f2));
    }

    #[tokio::test]
    async fn stops_on_oversized_frame() {
        let (mut mock_telescope, server) = loopback_pair().await;
        let (frame_tx, _) = broadcast::channel::<Arc<Vec<u8>>>(16);

        let task = tokio::spawn(upstream_reader_task(server, frame_tx, None, None));

        // Claim payload is 60 MB — exceeds the 50 MB sanity limit
        let header = make_header(60_000_000, 20, 1920, 1080);
        mock_telescope.write_all(&header).await.unwrap();
        mock_telescope.flush().await.unwrap();

        // Task should exit cleanly rather than trying to allocate 60 MB
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("task did not stop after oversized frame")
            .unwrap();
    }

    #[tokio::test]
    async fn frame_contains_header_and_payload() {
        let (mut mock_telescope, server) = loopback_pair().await;
        let (frame_tx, _) = broadcast::channel::<Arc<Vec<u8>>>(16);
        let mut rx = frame_tx.subscribe();

        tokio::spawn(upstream_reader_task(server, frame_tx, None, None));

        let header = make_header(4, 21, 640, 480);
        let payload = [0x01u8, 0x02, 0x03, 0x04];
        mock_telescope.write_all(&header).await.unwrap();
        mock_telescope.write_all(&payload).await.unwrap();
        mock_telescope.flush().await.unwrap();

        let frame = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(frame.len(), HEADER_SIZE + 4);
        assert_eq!(&frame[..HEADER_SIZE], &header[..]);
        assert_eq!(&frame[HEADER_SIZE..], &payload[..]);
    }

    #[tokio::test]
    async fn sends_multiple_frames_in_sequence() {
        let (mut mock_telescope, server) = loopback_pair().await;
        let (frame_tx, _) = broadcast::channel::<Arc<Vec<u8>>>(16);
        let mut rx = frame_tx.subscribe();

        tokio::spawn(upstream_reader_task(server, frame_tx, None, None));

        for i in 0u8..3 {
            let header = make_header(1, 21, 10, 10);
            mock_telescope.write_all(&header).await.unwrap();
            mock_telescope.write_all(&[i]).await.unwrap();
        }
        mock_telescope.flush().await.unwrap();

        for expected_byte in 0u8..3 {
            let frame = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(frame[HEADER_SIZE], expected_byte);
        }
    }

    // ── handle_client ─────────────────────────────────────────────────────────

    /// Bug 4 — lagged receiver silently disconnects client: handle_client uses
    /// `frame_rx.recv().await?` which propagates RecvError::Lagged as a fatal
    /// error, dropping the TCP connection. A slow client that briefly falls
    /// behind should instead skip missed frames and stay connected.
    #[tokio::test]
    async fn lagged_receiver_does_not_disconnect_client() {
        // Small capacity so lag is easy to trigger.
        let (frame_tx, _sentinel) = broadcast::channel::<Arc<Vec<u8>>>(2);
        let lagged_rx = frame_tx.subscribe();

        // Overfill the channel (3 sends into capacity-2) without consuming,
        // so lagged_rx is already lagged before handle_client starts.
        let filler = Arc::new(vec![0xAAu8; 4]);
        for _ in 0..3 {
            let _ = frame_tx.send(filler.clone());
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client_stream = TcpStream::connect(addr).await.unwrap();
        let (mut server_side, _) = listener.accept().await.unwrap();

        let task = tokio::spawn(handle_client(client_stream, lagged_rx));

        // Give handle_client a moment to encounter the Lagged error.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Send a fresh frame after handle_client is running.
        let good = Arc::new(vec![0xBBu8; 4]);
        let _ = frame_tx.send(good);

        // Bug: handle_client exits on Lagged, server_side gets EOF, read fails.
        // Fix: handle_client recovers from Lagged and delivers subsequent frames.
        let mut buf = vec![0u8; 4];
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            server_side.read_exact(&mut buf),
        )
        .await;

        assert!(
            result.is_ok_and(|r| r.is_ok()),
            "client must stay connected after a Lagged event"
        );

        task.abort();
    }

    #[tokio::test]
    async fn handle_client_forwards_frames_to_tcp_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (frame_tx, frame_rx) = broadcast::channel::<Arc<Vec<u8>>>(16);

        // Spawn the client handler connecting to our listener
        let client_conn = TcpStream::connect(addr).await.unwrap();
        tokio::spawn(handle_client(client_conn, frame_rx));

        let (mut server_side, _) = listener.accept().await.unwrap();

        let data = Arc::new(vec![0xCAu8; 32]);
        frame_tx.send(data.clone()).unwrap();

        let mut buf = vec![0u8; 32];
        tokio::time::timeout(Duration::from_secs(1), server_side.read_exact(&mut buf))
            .await
            .expect("timed out reading frame from client handler")
            .unwrap();

        assert_eq!(buf, vec![0xCA; 32]);
    }
}
