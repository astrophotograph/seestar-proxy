//! Port 4700 control proxy — multiplexes JSON-RPC between multiple clients
//! and a single upstream Seestar connection.
//!
//! - Connects to upstream lazily (when first client connects)
//! - Rewrites `id` fields to avoid collisions between clients
//! - Routes responses back to the originating client by mapped ID
//! - Broadcasts async events (no `id`) to all connected clients
//! - Sends heartbeats only after handshake completes

use crate::protocol;
use crate::recorder::Recorder;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, Mutex, Notify};
use tracing::{debug, error, info, warn};

/// A pending request awaiting a response from the telescope.
struct PendingRequest {
    client_tx: mpsc::Sender<String>,
    original_id: u64,
}

/// Shared state for the control proxy.
struct ControlState {
    pending: HashMap<u64, PendingRequest>,
}

/// Run the control proxy.
pub async fn run(
    bind_addr: std::net::SocketAddr,
    upstream_addr: std::net::SocketAddr,
    recorder: Option<Arc<Recorder>>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(bind_addr).await?;
    info!("Control proxy listening on {}", bind_addr);

    let next_id = Arc::new(AtomicU64::new(100_000));
    let (event_tx, _) = broadcast::channel::<String>(256);
    let state = Arc::new(Mutex::new(ControlState {
        pending: HashMap::new(),
    }));

    // Signal that the first response has arrived (handshake complete).
    let handshake_done = Arc::new(AtomicBool::new(false));

    // Upstream connection is established lazily — channel is created now,
    // but the writer/reader tasks start when the first client connects.
    let (upstream_tx, upstream_rx) = mpsc::channel::<String>(256);
    let upstream_started = Arc::new(Notify::new());

    // Spawn the upstream connection task (waits for first client).
    {
        let upstream_started = upstream_started.clone();
        let state = state.clone();
        let event_tx = event_tx.clone();
        let recorder = recorder.clone();
        let next_id = next_id.clone();
        let handshake_done = handshake_done.clone();
        let upstream_tx_hb = upstream_tx.clone();

        tokio::spawn(async move {
            // Wait until a client connects.
            upstream_started.notified().await;

            info!("First client connected — establishing upstream connection to {}...", upstream_addr);
            let upstream = match TcpStream::connect(upstream_addr).await {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to connect to telescope: {}", e);
                    return;
                }
            };
            let _ = upstream.set_nodelay(true);
            info!("Connected to telescope control at {}", upstream_addr);

            let (upstream_reader, mut upstream_writer) = upstream.into_split();

            // Spawn upstream reader.
            let state_r = state;
            let event_tx_r = event_tx;
            let recorder_r = recorder;
            let handshake_done_r = handshake_done.clone();
            tokio::spawn(upstream_reader_task(
                upstream_reader,
                state_r,
                event_tx_r,
                recorder_r,
                handshake_done_r,
            ));

            // Spawn upstream writer (reads from channel, writes to socket).
            tokio::spawn(async move {
                let mut rx = upstream_rx;
                while let Some(msg) = rx.recv().await {
                    debug!("-> telescope: {}", &msg[..msg.len().min(200)]);
                    let line = format!("{}\r\n", msg);
                    if let Err(e) = upstream_writer.write_all(line.as_bytes()).await {
                        error!("Upstream write error: {}", e);
                        break;
                    }
                }
                info!("Upstream control writer stopped");
            });

            // Spawn heartbeat task — waits for handshake before starting.
            tokio::spawn(async move {
                // Wait for the first successful response before heartbeating.
                while !handshake_done.load(Ordering::Relaxed) {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                info!("Handshake complete — starting heartbeat");

                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    let id = next_id.fetch_add(1, Ordering::Relaxed);
                    let msg = format!(r#"{{"id":{},"method":"test_connection"}}"#, id);
                    debug!("Heartbeat: id={}", id);
                    if upstream_tx_hb.send(msg).await.is_err() {
                        break;
                    }
                }
            });
        });
    }

    let upstream_started_once = Arc::new(AtomicBool::new(false));

    // Accept client connections.
    loop {
        let (client_stream, client_addr) = listener.accept().await?;
        let _ = client_stream.set_nodelay(true);
        info!("Control client connected: {}", client_addr);

        // Trigger upstream connection on first client.
        if !upstream_started_once.swap(true, Ordering::Relaxed) {
            upstream_started.notify_one();
            // Give the upstream connection a moment to establish.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        let upstream_tx = upstream_tx.clone();
        let event_rx = event_tx.subscribe();
        let state = state.clone();
        let next_id = next_id.clone();
        let recorder = recorder.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_client(
                client_stream,
                upstream_tx,
                event_rx,
                state,
                next_id,
                recorder,
            )
            .await
            {
                warn!("Control client {} error: {}", client_addr, e);
            }
            info!("Control client disconnected: {}", client_addr);
        });
    }
}

/// Handle a single client connection.
async fn handle_client(
    stream: TcpStream,
    upstream_tx: mpsc::Sender<String>,
    mut event_rx: broadcast::Receiver<String>,
    state: Arc<Mutex<ControlState>>,
    next_id: Arc<AtomicU64>,
    recorder: Option<Arc<Recorder>>,
) -> anyhow::Result<()> {
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let writer = Arc::new(Mutex::new(writer));

    let (response_tx, mut response_rx) = mpsc::channel::<String>(64);

    // Spawn task to forward events and responses to this client.
    let writer_c = writer.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(msg) = response_rx.recv() => {
                    let mut w = writer_c.lock().await;
                    let line = format!("{}\r\n", msg);
                    if w.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                    let _ = w.flush().await;
                }
                result = event_rx.recv() => {
                    match result {
                        Ok(msg) => {
                            let mut w = writer_c.lock().await;
                            let line = format!("{}\r\n", msg);
                            if w.write_all(line.as_bytes()).await.is_err() {
                                break;
                            }
                            let _ = w.flush().await;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("Client event receiver lagged by {} messages", n);
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    });

    // Read requests from the client.
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(recorder) = &recorder {
            recorder.record_control("client", trimmed).await;
        }

        let mut msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => {
                warn!("Invalid JSON from client: {}", &trimmed[..trimmed.len().min(100)]);
                continue;
            }
        };

        if let Some(original_id) = protocol::json_rpc_id(&msg) {
            let remapped_id = next_id.fetch_add(1, Ordering::Relaxed);
            protocol::set_json_rpc_id(&mut msg, remapped_id);

            let mut st = state.lock().await;
            st.pending.insert(
                remapped_id,
                PendingRequest {
                    client_tx: response_tx.clone(),
                    original_id,
                },
            );
            drop(st);

            info!(
                "Forwarding: method={} id {} -> {}",
                protocol::method_name(&msg).unwrap_or("?"),
                original_id,
                remapped_id
            );
        }

        let forwarded = serde_json::to_string(&msg)?;
        if upstream_tx.send(forwarded).await.is_err() {
            error!("Upstream channel closed — telescope connection lost");
            break;
        }
    }

    Ok(())
}

/// Read responses and events from the upstream telescope.
async fn upstream_reader_task(
    mut reader: tokio::net::tcp::OwnedReadHalf,
    state: Arc<Mutex<ControlState>>,
    event_tx: broadcast::Sender<String>,
    recorder: Option<Arc<Recorder>>,
    handshake_done: Arc<AtomicBool>,
) {
    info!("Upstream reader started, waiting for telescope messages...");

    let mut buf = Vec::with_capacity(64 * 1024);
    let mut tmp = [0u8; 32 * 1024];
    let mut total_bytes: u64 = 0;
    let mut total_messages: u64 = 0;

    loop {
        let n = match reader.read(&mut tmp).await {
            Ok(0) => {
                error!("Upstream telescope disconnected (EOF) after {} bytes, {} messages", total_bytes, total_messages);
                break;
            }
            Ok(n) => n,
            Err(e) => {
                error!("Upstream read error after {} bytes: {}", total_bytes, e);
                break;
            }
        };

        total_bytes += n as u64;
        buf.extend_from_slice(&tmp[..n]);
        let newline_count = tmp[..n].iter().filter(|&&b| b == b'\n').count();
        if newline_count > 0 || buf.len() > 1000 {
            info!(
                "Upstream: +{} bytes ({} newlines), buf={}, total={}",
                n, newline_count, buf.len(), total_bytes
            );
        }

        // Process all complete lines.
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes = buf[..pos].to_vec();
            buf.drain(..=pos);

            let trimmed = String::from_utf8_lossy(&line_bytes).trim().to_string();
            if trimmed.is_empty() {
                continue;
            }

            total_messages += 1;

            let method_hint = if trimmed.len() > 20 {
                // Quick peek at method for logging without full parse
                if let Some(start) = trimmed.find("\"method\"") {
                    &trimmed[start..trimmed.len().min(start + 50)]
                } else if let Some(start) = trimmed.find("\"Event\"") {
                    &trimmed[start..trimmed.len().min(start + 40)]
                } else {
                    &trimmed[..trimmed.len().min(60)]
                }
            } else {
                &trimmed
            };
            info!("<- telescope #{}: {}", total_messages, method_hint);

            if let Some(recorder) = &recorder {
                recorder.record_control("telescope", &trimmed).await;
            }

            // Signal that handshake is done after first response.
            if !handshake_done.load(Ordering::Relaxed) {
                handshake_done.store(true, Ordering::Relaxed);
                info!("First telescope response received — handshake complete");
            }

            let msg: Value = match serde_json::from_str(&trimmed) {
                Ok(v) => v,
                Err(e) => {
                    warn!(
                        "Invalid JSON from telescope: {} ({})",
                        e,
                        &trimmed[..trimmed.len().min(100)]
                    );
                    continue;
                }
            };

            if protocol::is_event(&msg) {
                debug!(
                    "Event: {}",
                    msg.get("Event").and_then(|v| v.as_str()).unwrap_or("?")
                );
                let _ = event_tx.send(trimmed);
            } else if let Some(remapped_id) = protocol::json_rpc_id(&msg) {
                let pending = {
                    let mut st = state.lock().await;
                    st.pending.remove(&remapped_id)
                };

                if let Some(pending) = pending {
                    let mut response = msg;
                    protocol::set_json_rpc_id(&mut response, pending.original_id);
                    let response_str = serde_json::to_string(&response).unwrap_or_default();

                    info!(
                        "Response routed: method={} id {} -> {}",
                        protocol::method_name(&response).unwrap_or("?"),
                        remapped_id,
                        pending.original_id
                    );

                    if pending.client_tx.try_send(response_str).is_err() {
                        warn!("Client channel full/closed for id {}", pending.original_id);
                    }
                } else {
                    debug!("Response for untracked id {} (heartbeat), discarding", remapped_id);
                }
            } else {
                debug!("Unknown message type, broadcasting");
                let _ = event_tx.send(trimmed);
            }
        }

        if buf.len() > 1_000_000 {
            warn!("Buffer overflow ({} bytes), clearing", buf.len());
            buf.clear();
        }
    }

    error!("Upstream control reader stopped — telescope connection lost");
}
