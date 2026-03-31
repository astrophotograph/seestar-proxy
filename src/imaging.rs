//! Port 4800 imaging proxy — fans out binary frames from one upstream
//! Seestar connection to all connected clients.
//!
//! The upstream sends an 80-byte header followed by `size` bytes of payload.
//! Each complete frame is broadcast to every connected client.
//!
//! The imaging port also uses JSON-RPC `test_connection` heartbeats (sent
//! as text, not binary) to keep the connection alive.

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

    let (frame_tx, _) = broadcast::channel::<Arc<Vec<u8>>>(32);
    let upstream_started = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Accept client connections. Upstream connects lazily on first client.
    loop {
        let (client_stream, client_addr) = listener.accept().await?;
        let _ = client_stream.set_nodelay(true);
        info!("Imaging client connected: {}", client_addr);

        // Connect upstream on first imaging client.
        if !upstream_started.swap(true, std::sync::atomic::Ordering::Relaxed) {
            info!("First imaging client — connecting to telescope at {}...", upstream_addr);
            match TcpStream::connect(upstream_addr).await {
                Ok(upstream) => {
                    let _ = upstream.set_nodelay(true);
                    info!("Connected to telescope imaging at {}", upstream_addr);
                    let (upstream_reader, upstream_writer) = upstream.into_split();
                    let frame_tx_r = frame_tx.clone();
                    tokio::spawn(upstream_reader_task(upstream_reader, frame_tx_r, recorder.clone()));
                    tokio::spawn(imaging_heartbeat_task(upstream_writer));
                }
                Err(e) => {
                    error!("Failed to connect to telescope imaging: {}", e);
                }
            }
        }

        let frame_rx = frame_tx.subscribe();
        tokio::spawn(async move {
            if let Err(e) = handle_client(client_stream, frame_rx).await {
                debug!("Imaging client {} error: {}", client_addr, e);
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
            r#"{{"id":{},"method":"test_connection"}}"#,
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
    mut reader: tokio::net::tcp::OwnedReadHalf,
    frame_tx: broadcast::Sender<Arc<Vec<u8>>>,
    recorder: Option<Arc<Recorder>>,
) {
    let mut header_buf = [0u8; HEADER_SIZE];

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
