//! Port 4800 imaging proxy — fans out binary frames from one upstream
//! Seestar connection to all connected clients.
//!
//! The upstream sends an 80-byte header followed by `size` bytes of payload.
//! Each complete frame is broadcast to every connected client.
//!
//! Traffic on the imaging port is mostly downstream (telescope → clients), but
//! clients also send JSON-RPC text commands upstream — a `test_connection`
//! heartbeat to keep the socket alive, and an imaging-start command issued
//! alongside the one on the control port. All such client lines, plus the
//! proxy's own heartbeat, are funnelled through a single mpsc channel to one
//! upstream writer so they are never interleaved mid-line.

use crate::control::{MAX_LINE_BYTES, read_line_limited};
use crate::metrics::Metrics;
use crate::protocol::{FrameHeader, HEADER_SIZE};
use crate::recorder::Recorder;
use crate::replay::ReplaySession;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, error, info, warn};

/// Run the imaging proxy.
pub async fn run(
    bind_addr: std::net::SocketAddr,
    upstream_addr: Option<std::net::SocketAddr>,
    transparent: bool,
    recorder: Option<Arc<Recorder>>,
    metrics: Option<Arc<Metrics>>,
    replay: Option<Arc<ReplaySession>>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(bind_addr).await?;
    info!("Imaging proxy listening on {}", bind_addr);

    let (frame_tx, _) = broadcast::channel::<Arc<Vec<u8>>>(32);

    // Client → telescope command channel. Every client handler and the
    // heartbeat task send complete JSON lines here; a single upstream writer
    // drains it so concurrent senders never interleave mid-line. Created once
    // and owned across reconnects (mirrors the control proxy).
    let (upstream_tx, upstream_rx) = mpsc::channel::<String>(256);

    // In transparent mode, the upstream address may not be known yet.
    let resolved_upstream: Arc<tokio::sync::OnceCell<std::net::SocketAddr>> =
        Arc::new(tokio::sync::OnceCell::new());
    if let Some(addr) = upstream_addr {
        let _ = resolved_upstream.set(addr);
    }

    if let Some(session) = replay {
        // ─── Replay mode ────────────────────────────────────────────────
        // Drain client commands so their `upstream_tx` doesn't see a closed
        // channel (there is no telescope to forward to during replay).
        tokio::spawn(async move {
            let mut rx = upstream_rx;
            while let Some(msg) = rx.recv().await {
                debug!("Replay imaging drain: {}", &msg[..msg.len().min(120)]);
            }
        });

        let frame_tx_r = frame_tx.clone();
        let metrics_r = metrics.clone();
        tokio::spawn(async move {
            crate::replay::replay_imaging(&session, &frame_tx_r, metrics_r.as_deref()).await;
        });
    } else {
        // ─── Normal upstream connection ──────────────────────────────────
        let frame_tx = frame_tx.clone();
        let recorder = recorder.clone();
        let metrics = metrics.clone();
        let resolved_upstream = resolved_upstream.clone();
        let hb_tx = upstream_tx.clone();
        let mut upstream_rx = upstream_rx;
        tokio::spawn(async move {
            // Heartbeat: spawned once, feeds the shared command channel.
            tokio::spawn(imaging_heartbeat_task(hb_tx));

            // Wait for the upstream address to be resolved.
            let upstream_addr = loop {
                if let Some(&addr) = resolved_upstream.get() {
                    break addr;
                }
                info!("Imaging: waiting for upstream address (transparent mode)...");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            };

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
                        let (upstream_reader, mut upstream_writer) = upstream.into_split();

                        // The reader task signals (by dropping this sender) when
                        // the connection dies, so the writer loop stops and reconnects.
                        let (reader_dead_tx, mut reader_dead_rx) = oneshot::channel::<()>();
                        tokio::spawn(upstream_reader_task(
                            upstream_reader,
                            frame_tx.clone(),
                            recorder.clone(),
                            metrics.clone(),
                            reader_dead_tx,
                        ));

                        // Forward queued commands to the telescope until the
                        // reader dies or all client senders are dropped.
                        loop {
                            tokio::select! {
                                msg = upstream_rx.recv() => match msg {
                                    None => return, // all senders dropped; shut down
                                    Some(msg) => {
                                        debug!("Imaging -> telescope: {}", &msg[..msg.len().min(200)]);
                                        let line = format!("{}\r\n", msg);
                                        if let Err(e) = upstream_writer.write_all(line.as_bytes()).await {
                                            error!("Imaging upstream write error: {}", e);
                                            break;
                                        }
                                        let _ = upstream_writer.flush().await;
                                    }
                                },
                                _ = &mut reader_dead_rx => break, // reader exited; reconnect
                            }
                        }
                        warn!("Telescope imaging connection lost");
                    }
                    Ok(Err(e)) => error!("Failed to connect to telescope imaging: {}", e),
                    Err(_) => error!(
                        "Timed out connecting to telescope imaging at {}",
                        upstream_addr
                    ),
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

        // In transparent mode, resolve upstream from SO_ORIGINAL_DST.
        #[cfg(unix)]
        if transparent && resolved_upstream.get().is_none() {
            use std::os::unix::io::AsRawFd;
            if let Some(orig) = crate::transparent::get_original_dst(client_stream.as_raw_fd()) {
                info!(
                    "Imaging transparent mode: resolved upstream from SO_ORIGINAL_DST: {}",
                    orig
                );
                let _ = resolved_upstream.set(orig);
            }
        }

        let frame_rx = frame_tx.subscribe();
        let upstream_tx_c = upstream_tx.clone();
        let metrics_c = metrics.clone();
        if let Some(m) = &metrics {
            m.imaging_clients.fetch_add(1, Ordering::Relaxed);
        }
        tokio::spawn(async move {
            if let Err(e) =
                handle_client(client_stream, frame_rx, upstream_tx_c, metrics_c.clone()).await
            {
                debug!("Imaging client {} error: {}", client_addr, e);
            }
            if let Some(m) = &metrics_c {
                m.imaging_clients.fetch_sub(1, Ordering::Relaxed);
            }
            info!("Imaging client disconnected: {}", client_addr);
        });
    }
}

/// Send periodic heartbeats on the imaging connection via the shared upstream
/// command channel. The imaging port accepts JSON-RPC text for control commands.
async fn imaging_heartbeat_task(upstream_tx: mpsc::Sender<String>) {
    let mut id: u64 = 1;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let msg = format!(
            r#"{{"id":{},"method":"test_connection","params":"verify"}}"#,
            id
        );
        id += 1;
        debug!("Imaging heartbeat: {}", msg);
        if upstream_tx.send(msg).await.is_err() {
            // Receiver gone — the upstream task has shut down.
            break;
        }
    }
    info!("Imaging heartbeat task stopped");
}

/// Read frames from the upstream telescope and broadcast them.
///
/// `_done_tx` is dropped when this task returns, which signals the writer loop
/// (via its paired receiver) that the connection has died and must reconnect.
async fn upstream_reader_task(
    mut reader: impl AsyncReadExt + Unpin,
    frame_tx: broadcast::Sender<Arc<Vec<u8>>>,
    recorder: Option<Arc<Recorder>>,
    metrics: Option<Arc<Metrics>>,
    _done_tx: oneshot::Sender<()>,
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
            m.imaging_bytes
                .fetch_add((HEADER_SIZE + payload.len()) as u64, Ordering::Relaxed);
            // Log every 30th frame to avoid flooding the traffic log.
            frame_count += 1;
            if frame_count % 30 == 1 {
                let kind = match header.id {
                    20 => "view",
                    21 => "preview",
                    23 => "stack",
                    _ => "frame",
                };
                let summary = if header.is_image() {
                    format!(
                        "{} {}x{} ({:.1} KB)",
                        kind,
                        header.width,
                        header.height,
                        header.size as f64 / 1024.0
                    )
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

/// Handle a single imaging client: fan broadcast frames out to it (in a
/// background task) while reading JSON-RPC command lines from it and forwarding
/// them to the telescope via the shared upstream channel.
async fn handle_client(
    stream: TcpStream,
    mut frame_rx: broadcast::Receiver<Arc<Vec<u8>>>,
    upstream_tx: mpsc::Sender<String>,
    metrics: Option<Arc<Metrics>>,
) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();

    // Frame fan-out task: telescope → this client. Kept so we can abort it when
    // the read loop ends, dropping the write half so the client sees EOF.
    let frame_task = tokio::spawn(async move {
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
            if writer.write_all(&frame).await.is_err() {
                break;
            }
            let _ = writer.flush().await;
        }
    });

    // Read loop: this client → telescope. Forward each complete JSON line.
    let mut reader = BufReader::new(reader);
    let mut line_buf = Vec::new();
    loop {
        line_buf.clear();
        let n = read_line_limited(&mut reader, &mut line_buf, MAX_LINE_BYTES).await?;
        if n == 0 {
            break; // client closed the connection
        }
        if line_buf.len() > MAX_LINE_BYTES {
            warn!(
                "Imaging client sent oversized line ({} bytes), disconnecting",
                line_buf.len()
            );
            break;
        }

        let trimmed = String::from_utf8_lossy(&line_buf).trim().to_string();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(m) = &metrics {
            let method = serde_json::from_str::<serde_json::Value>(&trimmed)
                .ok()
                .and_then(|v| crate::protocol::method_name(&v).map(str::to_string))
                .unwrap_or_else(|| "?".to_string());
            m.push_log_with_payload("img-tx", method, Some(trimmed.clone()));
        }

        if upstream_tx.send(trimmed).await.is_err() {
            error!("Imaging upstream channel closed — telescope connection lost");
            break;
        }
    }

    // Drop the write half so the client promptly sees EOF.
    frame_task.abort();
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

        tokio::spawn(upstream_reader_task(
            server,
            frame_tx,
            None,
            None,
            oneshot::channel().0,
        ));

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

        let task = tokio::spawn(upstream_reader_task(
            server,
            frame_tx,
            None,
            None,
            oneshot::channel().0,
        ));

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

        tokio::spawn(upstream_reader_task(
            server,
            frame_tx,
            None,
            None,
            oneshot::channel().0,
        ));

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

        tokio::spawn(upstream_reader_task(
            server,
            frame_tx,
            None,
            None,
            oneshot::channel().0,
        ));

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

        let (upstream_tx, _upstream_rx) = mpsc::channel::<String>(8);
        let task = tokio::spawn(handle_client(client_stream, lagged_rx, upstream_tx, None));

        // Give handle_client a moment to encounter the Lagged error.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Send a fresh frame after handle_client is running.
        let good = Arc::new(vec![0xBBu8; 4]);
        let _ = frame_tx.send(good);

        // Bug: handle_client exits on Lagged, server_side gets EOF, read fails.
        // Fix: handle_client recovers from Lagged and delivers subsequent frames.
        let mut buf = vec![0u8; 4];
        let result =
            tokio::time::timeout(Duration::from_secs(1), server_side.read_exact(&mut buf)).await;

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
        let (upstream_tx, _upstream_rx) = mpsc::channel::<String>(8);
        tokio::spawn(handle_client(client_conn, frame_rx, upstream_tx, None));

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

    // ── handle_client: client → upstream forwarding ───────────────────────────

    #[tokio::test]
    async fn handle_client_forwards_client_line_upstream() {
        let (mut client, server) = loopback_pair().await;
        let (_frame_tx, frame_rx) = broadcast::channel::<Arc<Vec<u8>>>(16);
        let (upstream_tx, mut upstream_rx) = mpsc::channel::<String>(16);

        tokio::spawn(handle_client(server, frame_rx, upstream_tx, None));

        client
            .write_all(b"{\"id\":1,\"method\":\"begin_streaming\"}\r\n")
            .await
            .unwrap();
        client.flush().await.unwrap();

        let forwarded = tokio::time::timeout(Duration::from_secs(1), upstream_rx.recv())
            .await
            .expect("timed out waiting for forwarded command")
            .expect("upstream channel closed");

        let v: serde_json::Value = serde_json::from_str(&forwarded).unwrap();
        assert_eq!(v["method"], "begin_streaming");
        assert_eq!(v["id"], 1);
    }

    #[tokio::test]
    async fn handle_client_skips_empty_client_lines() {
        let (mut client, server) = loopback_pair().await;
        let (_frame_tx, frame_rx) = broadcast::channel::<Arc<Vec<u8>>>(16);
        let (upstream_tx, mut upstream_rx) = mpsc::channel::<String>(16);

        tokio::spawn(handle_client(server, frame_rx, upstream_tx, None));

        // Blank lines before a real command — only the real one is forwarded.
        client
            .write_all(b"\r\n   \r\n{\"id\":2,\"method\":\"get_view_state\"}\r\n")
            .await
            .unwrap();
        client.flush().await.unwrap();

        let forwarded = tokio::time::timeout(Duration::from_secs(1), upstream_rx.recv())
            .await
            .expect("timed out")
            .expect("upstream channel closed");
        let v: serde_json::Value = serde_json::from_str(&forwarded).unwrap();
        assert_eq!(v["method"], "get_view_state");
    }

    #[tokio::test]
    async fn handle_client_returns_when_client_disconnects() {
        let (client, server) = loopback_pair().await;
        let (_frame_tx, frame_rx) = broadcast::channel::<Arc<Vec<u8>>>(16);
        let (upstream_tx, _upstream_rx) = mpsc::channel::<String>(16);

        let task = tokio::spawn(handle_client(server, frame_rx, upstream_tx, None));

        // Client goes away — the read loop hits EOF and the handler returns.
        drop(client);

        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("handle_client did not exit after client disconnect")
            .unwrap()
            .unwrap();
    }
}
