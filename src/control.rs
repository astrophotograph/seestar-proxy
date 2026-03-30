//! Port 4700 control proxy — multiplexes JSON-RPC between multiple clients
//! and a single upstream Seestar connection.
//!
//! - Rewrites `id` fields to avoid collisions between clients
//! - Routes responses back to the originating client by mapped ID
//! - Broadcasts async events (no `id`) to all connected clients

use crate::protocol;
use crate::recorder::Recorder;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{debug, error, info, warn};

/// A pending request awaiting a response from the telescope.
struct PendingRequest {
    /// Client that sent this request.
    client_tx: mpsc::Sender<String>,
    /// Original ID from the client (to restore in the response).
    original_id: u64,
}

/// Shared state for the control proxy.
struct ControlState {
    /// Map from remapped ID → pending request info.
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

    // Global ID counter for remapping (avoids collisions across clients).
    let next_id = Arc::new(AtomicU64::new(10_000));

    // Channel for sending requests to the upstream writer.
    let (upstream_tx, upstream_rx) = mpsc::channel::<String>(256);

    // Broadcast channel for events from the telescope to all clients.
    let (event_tx, _) = broadcast::channel::<String>(256);

    // Shared pending-request map.
    let state = Arc::new(Mutex::new(ControlState {
        pending: HashMap::new(),
    }));

    // Connect to upstream telescope.
    let upstream = TcpStream::connect(upstream_addr).await?;
    info!("Connected to telescope control at {}", upstream_addr);
    let (upstream_reader, upstream_writer) = upstream.into_split();

    // Spawn upstream writer task.
    let recorder_w = recorder.clone();
    tokio::spawn(upstream_writer_task(upstream_writer, upstream_rx, recorder_w));

    // Spawn upstream reader task.
    let state_r = state.clone();
    let event_tx_r = event_tx.clone();
    let recorder_r = recorder.clone();
    tokio::spawn(upstream_reader_task(
        upstream_reader,
        state_r,
        event_tx_r,
        recorder_r,
    ));

    // Accept client connections.
    loop {
        let (client_stream, client_addr) = listener.accept().await?;
        info!("Control client connected: {}", client_addr);

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

    // Per-client channel for receiving responses.
    let (response_tx, mut response_rx) = mpsc::channel::<String>(64);

    // Spawn task to forward events and responses to this client.
    let writer_c = writer.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                // Responses addressed to this client.
                Some(msg) = response_rx.recv() => {
                    let mut w = writer_c.lock().await;
                    if w.write_all(msg.as_bytes()).await.is_err() {
                        break;
                    }
                    if w.write_all(b"\r\n").await.is_err() {
                        break;
                    }
                    let _ = w.flush().await;
                }
                // Broadcast events from the telescope.
                Ok(msg) = event_rx.recv() => {
                    let mut w = writer_c.lock().await;
                    if w.write_all(msg.as_bytes()).await.is_err() {
                        break;
                    }
                    if w.write_all(b"\r\n").await.is_err() {
                        break;
                    }
                    let _ = w.flush().await;
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
            break; // Client disconnected.
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(recorder) = &recorder {
            recorder.record_control("client", trimmed).await;
        }

        // Parse JSON-RPC request.
        let mut msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => {
                warn!("Invalid JSON from client: {}", &trimmed[..trimmed.len().min(100)]);
                continue;
            }
        };

        // Remap the ID to avoid collisions.
        if let Some(original_id) = protocol::json_rpc_id(&msg) {
            let remapped_id = next_id.fetch_add(1, Ordering::Relaxed);
            protocol::set_json_rpc_id(&mut msg, remapped_id);

            // Register the pending request.
            let mut st = state.lock().await;
            st.pending.insert(
                remapped_id,
                PendingRequest {
                    client_tx: response_tx.clone(),
                    original_id,
                },
            );
            drop(st);

            debug!(
                "Forwarding request: method={} id={} -> {}",
                protocol::method_name(&msg).unwrap_or("?"),
                original_id,
                remapped_id
            );
        }

        // Forward to upstream.
        let forwarded = serde_json::to_string(&msg)?;
        upstream_tx.send(forwarded).await?;
    }

    Ok(())
}

/// Write requests to the upstream telescope connection.
async fn upstream_writer_task(
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    mut rx: mpsc::Receiver<String>,
    recorder: Option<Arc<Recorder>>,
) {
    while let Some(msg) = rx.recv().await {
        if let Some(recorder) = &recorder {
            recorder.record_control("client", &msg).await;
        }
        if writer.write_all(msg.as_bytes()).await.is_err() {
            break;
        }
        if writer.write_all(b"\r\n").await.is_err() {
            break;
        }
        let _ = writer.flush().await;
    }
    info!("Upstream control writer stopped");
}

/// Read responses and events from the upstream telescope.
async fn upstream_reader_task(
    reader: tokio::net::tcp::OwnedReadHalf,
    state: Arc<Mutex<ControlState>>,
    event_tx: broadcast::Sender<String>,
    recorder: Option<Arc<Recorder>>,
) {
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // Telescope disconnected
            Ok(_) => {}
            Err(e) => {
                error!("Upstream read error: {}", e);
                break;
            }
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(recorder) = &recorder {
            recorder.record_control("telescope", trimmed).await;
        }

        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if protocol::is_event(&msg) {
            // Broadcast event to all clients.
            let _ = event_tx.send(trimmed.to_string());
        } else if let Some(remapped_id) = protocol::json_rpc_id(&msg) {
            // Route response to the correct client.
            let mut st = state.lock().await;
            if let Some(pending) = st.pending.remove(&remapped_id) {
                // Restore the original client ID.
                let mut response = msg;
                protocol::set_json_rpc_id(&mut response, pending.original_id);
                let response_str = serde_json::to_string(&response).unwrap_or_default();
                let _ = pending.client_tx.send(response_str).await;
            } else {
                debug!("Response for unknown id {}, broadcasting", remapped_id);
                let _ = event_tx.send(trimmed.to_string());
            }
        }
    }
    info!("Upstream control reader stopped");
}
