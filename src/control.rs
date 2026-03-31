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

/// Maximum number of requests that may be simultaneously awaiting telescope
/// responses. Once this limit is reached new requests receive an immediate
/// error response rather than being queued indefinitely.
pub const MAX_PENDING_REQUESTS: usize = 1_024;

/// Maximum byte length of a single JSON-RPC line accepted from a client.
/// Lines exceeding this limit cause the client connection to be closed.
pub const MAX_LINE_BYTES: usize = 1_048_576; // 1 MiB

/// A pending request awaiting a response from the telescope.
struct PendingRequest {
    /// Client that sent this request.
    client_tx: mpsc::Sender<String>,
    /// Original ID from the client (to restore in the response).
    /// Stored as a raw Value so that any JSON-RPC id type is supported
    /// (positive/negative integers, strings, …).
    original_id: serde_json::Value,
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

    // Connect to upstream telescope (with timeout).
    let upstream = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        TcpStream::connect(upstream_addr),
    )
    .await
    .map_err(|_| anyhow::anyhow!("timed out connecting to control at {}", upstream_addr))??;
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
    // The handle is kept so we can abort the task when the read loop exits,
    // which drops the write half and sends FIN to the client promptly.
    let writer_c = writer.clone();
    let writer_task = tokio::spawn(async move {
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

        // Fix 6: reject oversized lines before any further processing.
        if line.len() > MAX_LINE_BYTES {
            warn!(
                "Client sent oversized line ({} bytes), disconnecting",
                line.len()
            );
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Fix 1: record once here (pre-remap); upstream_writer_task must NOT
        // record again so each client message appears exactly once.
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

        // Fix 2: use get_id() which accepts any non-null JSON value (including
        // negative integers and strings) rather than json_rpc_id() which only
        // handles u64 values.
        if let Some(original_id) = protocol::get_id(&msg).cloned() {
            // Fix 7: enforce a pending-map size limit. When full, return an
            // error immediately rather than queuing the request indefinitely.
            {
                let st = state.lock().await;
                if st.pending.len() >= MAX_PENDING_REQUESTS {
                    let err = serde_json::json!({
                        "id": original_id,
                        "code": -32000,
                        "error": "too many pending requests"
                    });
                    let _ = response_tx
                        .send(serde_json::to_string(&err).unwrap_or_default())
                        .await;
                    continue;
                }
            }

            let remapped_id = next_id.fetch_add(1, Ordering::Relaxed);
            protocol::set_json_rpc_id(&mut msg, remapped_id);

            // Fix 3: serialise before acquiring the lock so a serialisation
            // failure does not leave an orphaned pending entry.
            let forwarded = match serde_json::to_string(&msg) {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to serialise request: {}", e);
                    continue;
                }
            };

            // Register the pending request.
            {
                let mut st = state.lock().await;
                st.pending.insert(
                    remapped_id,
                    PendingRequest {
                        client_tx: response_tx.clone(),
                        original_id,
                    },
                );
            }

            debug!(
                "Forwarding request: method={} id={:?} -> {}",
                protocol::method_name(&msg).unwrap_or("?"),
                protocol::get_id(&msg),
                remapped_id
            );

            upstream_tx.send(forwarded).await?;
        } else {
            // No id field — forward as-is (notification).
            let forwarded = serde_json::to_string(&msg)?;
            upstream_tx.send(forwarded).await?;
        }
    }

    // Abort the writer task so the write half is dropped and the peer sees EOF.
    writer_task.abort();
    Ok(())
}

/// Write requests to the upstream telescope connection.
///
/// Recording is intentionally omitted here: `handle_client` already records
/// each message once (before ID remapping). Recording a second time here
/// would log every client message twice with an inconsistent remapped ID.
async fn upstream_writer_task(
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    mut rx: mpsc::Receiver<String>,
    _recorder: Option<Arc<Recorder>>,
) {
    while let Some(msg) = rx.recv().await {
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
                // Restore the original client ID (which may be any JSON type).
                let mut response = msg;
                protocol::set_json_rpc_id_value(&mut response, pending.original_id);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::net::{TcpListener, TcpStream};

    /// Returns (writer_end, reader_half): write to writer_end, read from reader_half.
    async fn loopback_read_half()
    -> (TcpStream, tokio::net::tcp::OwnedReadHalf) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let writer_end = TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let (reader, _) = server.into_split();
        (writer_end, reader)
    }

    fn pending_state(
        remapped_id: u64,
        client_tx: mpsc::Sender<String>,
        original_id: u64,
    ) -> Arc<Mutex<ControlState>> {
        let mut pending = HashMap::new();
        pending.insert(
            remapped_id,
            PendingRequest {
                client_tx,
                original_id: serde_json::Value::from(original_id),
            },
        );
        Arc::new(Mutex::new(ControlState { pending }))
    }

    // ── upstream_reader_task ──────────────────────────────────────────────────

    #[tokio::test]
    async fn routes_response_to_originating_client() {
        let (mut mock_telescope, reader) = loopback_read_half().await;
        let (client_tx, mut client_rx) = mpsc::channel::<String>(16);
        let (event_tx, _) = broadcast::channel::<String>(16);
        let state = pending_state(10000, client_tx, 42);

        let task = tokio::spawn(upstream_reader_task(reader, state.clone(), event_tx, None));

        mock_telescope
            .write_all(b"{\"id\":10000,\"code\":0,\"result\":null}\r\n")
            .await
            .unwrap();

        let received = tokio::time::timeout(Duration::from_secs(1), client_rx.recv())
            .await
            .expect("timed out waiting for response")
            .expect("channel closed");

        let v: serde_json::Value = serde_json::from_str(&received).unwrap();
        assert_eq!(v["id"], 42, "original id must be restored");
        assert_eq!(v["code"], 0);

        // Pending entry must be consumed after routing
        let st = state.lock().await;
        assert!(!st.pending.contains_key(&10000));

        task.abort();
    }

    #[tokio::test]
    async fn restores_original_id_in_response() {
        let (mut mock_telescope, reader) = loopback_read_half().await;
        let (client_tx, mut client_rx) = mpsc::channel::<String>(16);
        let (event_tx, _) = broadcast::channel::<String>(16);
        // original id 7, remapped to 99999
        let state = pending_state(99999, client_tx, 7);

        tokio::spawn(upstream_reader_task(reader, state, event_tx, None));

        mock_telescope
            .write_all(b"{\"id\":99999,\"code\":0,\"result\":{\"foo\":\"bar\"}}\r\n")
            .await
            .unwrap();

        let received = tokio::time::timeout(Duration::from_secs(1), client_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&received).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["result"]["foo"], "bar");
    }

    #[tokio::test]
    async fn routes_multiple_pending_requests_independently() {
        let (mut mock_telescope, reader) = loopback_read_half().await;
        let (tx_a, mut rx_a) = mpsc::channel::<String>(16);
        let (tx_b, mut rx_b) = mpsc::channel::<String>(16);
        let (event_tx, _) = broadcast::channel::<String>(16);

        let state = Arc::new(Mutex::new(ControlState {
            pending: {
                let mut m = HashMap::new();
                m.insert(10001, PendingRequest {
                    client_tx: tx_a,
                    original_id: serde_json::Value::from(1u64),
                });
                m.insert(10002, PendingRequest {
                    client_tx: tx_b,
                    original_id: serde_json::Value::from(2u64),
                });
                m
            },
        }));

        tokio::spawn(upstream_reader_task(reader, state, event_tx, None));

        // Send responses out of order
        mock_telescope
            .write_all(b"{\"id\":10002,\"code\":0}\r\n")
            .await
            .unwrap();
        mock_telescope
            .write_all(b"{\"id\":10001,\"code\":0}\r\n")
            .await
            .unwrap();

        let b = tokio::time::timeout(Duration::from_secs(1), rx_b.recv())
            .await
            .unwrap()
            .unwrap();
        let a = tokio::time::timeout(Duration::from_secs(1), rx_a.recv())
            .await
            .unwrap()
            .unwrap();

        let va: serde_json::Value = serde_json::from_str(&a).unwrap();
        let vb: serde_json::Value = serde_json::from_str(&b).unwrap();
        assert_eq!(va["id"], 1);
        assert_eq!(vb["id"], 2);
    }

    #[tokio::test]
    async fn broadcasts_event_to_all_clients() {
        let (mut mock_telescope, reader) = loopback_read_half().await;
        let state = Arc::new(Mutex::new(ControlState { pending: HashMap::new() }));
        let (event_tx, mut event_rx) = broadcast::channel::<String>(16);

        tokio::spawn(upstream_reader_task(reader, state, event_tx, None));

        mock_telescope
            .write_all(b"{\"Event\":\"PiStatus\",\"temp\":42.0}\r\n")
            .await
            .unwrap();

        let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("timed out")
            .unwrap();

        let v: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(v["Event"], "PiStatus");
        assert_eq!(v["temp"], 42.0);
    }

    #[tokio::test]
    async fn broadcasts_response_with_unknown_id() {
        // A response whose remapped id is not in the pending map gets broadcast
        // as a fallback so clients are not silently dropped.
        let (mut mock_telescope, reader) = loopback_read_half().await;
        let state = Arc::new(Mutex::new(ControlState { pending: HashMap::new() }));
        let (event_tx, mut event_rx) = broadcast::channel::<String>(16);

        tokio::spawn(upstream_reader_task(reader, state, event_tx, None));

        mock_telescope
            .write_all(b"{\"id\":55555,\"code\":0}\r\n")
            .await
            .unwrap();

        let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("timed out")
            .unwrap();

        let v: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(v["id"], 55555);
    }

    #[tokio::test]
    async fn skips_invalid_json_lines() {
        let (mut mock_telescope, reader) = loopback_read_half().await;
        let state = Arc::new(Mutex::new(ControlState { pending: HashMap::new() }));
        let (event_tx, mut event_rx) = broadcast::channel::<String>(16);

        tokio::spawn(upstream_reader_task(reader, state, event_tx, None));

        // Invalid JSON followed by a valid event — the valid one should arrive
        mock_telescope
            .write_all(b"not json at all\r\n")
            .await
            .unwrap();
        mock_telescope
            .write_all(b"{\"Event\":\"Heartbeat\"}\r\n")
            .await
            .unwrap();

        let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("timed out")
            .unwrap();

        let v: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(v["Event"], "Heartbeat");
    }

    #[tokio::test]
    async fn skips_empty_lines() {
        let (mut mock_telescope, reader) = loopback_read_half().await;
        let state = Arc::new(Mutex::new(ControlState { pending: HashMap::new() }));
        let (event_tx, mut event_rx) = broadcast::channel::<String>(16);

        tokio::spawn(upstream_reader_task(reader, state, event_tx, None));

        mock_telescope.write_all(b"\r\n  \r\n").await.unwrap();
        mock_telescope
            .write_all(b"{\"Event\":\"Ready\"}\r\n")
            .await
            .unwrap();

        let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("timed out")
            .unwrap();

        let v: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(v["Event"], "Ready");
    }

    // ── ID counter ────────────────────────────────────────────────────────────

    #[test]
    fn id_counter_starts_at_10000_and_increments() {
        let counter = Arc::new(AtomicU64::new(10_000));
        let first = counter.fetch_add(1, Ordering::Relaxed);
        let second = counter.fetch_add(1, Ordering::Relaxed);
        let third = counter.fetch_add(1, Ordering::Relaxed);
        assert_eq!(first, 10_000);
        assert_eq!(second, 10_001);
        assert_eq!(third, 10_002);
    }
}
