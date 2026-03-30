//! Port 4800 imaging proxy — fans out binary frames from one upstream
//! Seestar connection to all connected clients.
//!
//! The upstream sends an 80-byte header followed by `size` bytes of payload.
//! Each complete frame is broadcast to every connected client.

use crate::protocol::{FrameHeader, HEADER_SIZE};
use crate::recorder::Recorder;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tracing::{debug, error, info};

/// Run the imaging proxy.
pub async fn run(
    bind_addr: std::net::SocketAddr,
    upstream_addr: std::net::SocketAddr,
    recorder: Option<Arc<Recorder>>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(bind_addr).await?;
    info!("Imaging proxy listening on {}", bind_addr);

    // Broadcast channel for frames: (header, payload) as raw bytes.
    // Use a reasonably large capacity since frames can be several MB.
    let (frame_tx, _) = broadcast::channel::<Arc<Vec<u8>>>(32);

    // Connect to upstream telescope imaging port.
    let upstream = TcpStream::connect(upstream_addr).await?;
    info!("Connected to telescope imaging at {}", upstream_addr);

    // Spawn upstream reader task.
    let frame_tx_r = frame_tx.clone();
    tokio::spawn(upstream_reader_task(upstream, frame_tx_r, recorder));

    // Accept client connections.
    loop {
        let (client_stream, client_addr) = listener.accept().await?;
        info!("Imaging client connected: {}", client_addr);

        let frame_rx = frame_tx.subscribe();
        tokio::spawn(async move {
            if let Err(e) = handle_client(client_stream, frame_rx).await {
                debug!("Imaging client {} error: {}", client_addr, e);
            }
            info!("Imaging client disconnected: {}", client_addr);
        });
    }
}

/// Read frames from the upstream telescope and broadcast them.
async fn upstream_reader_task(
    mut upstream: TcpStream,
    frame_tx: broadcast::Sender<Arc<Vec<u8>>>,
    recorder: Option<Arc<Recorder>>,
) {
    let mut header_buf = [0u8; HEADER_SIZE];

    loop {
        // Read 80-byte header.
        if upstream.read_exact(&mut header_buf).await.is_err() {
            error!("Upstream imaging connection lost");
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
        if upstream.read_exact(&mut payload).await.is_err() {
            error!("Upstream imaging connection lost during payload read");
            break;
        }

        // Record if enabled.
        if let Some(recorder) = &recorder {
            recorder.record_frame(&header_buf, &payload).await;
        }

        // Build complete frame (header + payload) for broadcasting.
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

        // Broadcast to all connected clients. Ignore send errors
        // (no clients connected is fine).
        let _ = frame_tx.send(frame);
    }

    info!("Upstream imaging reader stopped");
}

/// Forward broadcast frames to a single client.
async fn handle_client(
    mut stream: TcpStream,
    mut frame_rx: broadcast::Receiver<Arc<Vec<u8>>>,
) -> anyhow::Result<()> {
    loop {
        let frame = frame_rx.recv().await?;
        if stream.write_all(&frame).await.is_err() {
            break;
        }
        let _ = stream.flush().await;
    }
    Ok(())
}
