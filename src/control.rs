//! Port 4700 control proxy — multiplexes JSON-RPC between multiple clients
//! and a single upstream Seestar connection.
//!
//! - Connects to upstream lazily (when first client connects)
//! - Rewrites `id` fields to avoid collisions between clients
//! - Routes responses back to the originating client by mapped ID
//! - Broadcasts async events (no `id`) to all connected clients
//! - Sends heartbeats only after handshake completes

use crate::hooks::{HookAction, HookEngine};
use crate::metrics::Metrics;
use crate::protocol;
use crate::recorder::Recorder;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Notify, broadcast, mpsc, oneshot};
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
    client_tx: mpsc::Sender<String>,
    /// Original ID from the client (to restore in the response).
    /// Stored as a raw Value so that any JSON-RPC id type is supported
    /// (positive/negative integers, strings, …).
    original_id: serde_json::Value,
}

/// Shared state for the control proxy.
struct ControlState {
    pending: HashMap<u64, PendingRequest>,
}

/// Run the control proxy.
pub async fn run(
    bind_addr: std::net::SocketAddr,
    upstream_addr: Option<std::net::SocketAddr>,
    transparent: bool,
    recorder: Option<Arc<Recorder>>,
    metrics: Option<Arc<Metrics>>,
    hooks: Option<Arc<HookEngine>>,
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

    // In transparent mode, the upstream address is resolved from
    // SO_ORIGINAL_DST on the first client connection.
    let resolved_upstream: Arc<tokio::sync::OnceCell<std::net::SocketAddr>> =
        Arc::new(tokio::sync::OnceCell::new());
    if let Some(addr) = upstream_addr {
        let _ = resolved_upstream.set(addr);
    }

    // Give hook scripts access to the upstream channel immediately so they
    // can inject auth messages (e.g. authenticate.lua) when the first client connects.
    if let Some(h) = &hooks {
        h.set_upstream_tx(upstream_tx.clone());
    }

    // Spawn the upstream connection task (waits for first client).
    {
        let upstream_started = upstream_started.clone();
        let state = state.clone();
        let event_tx = event_tx.clone();
        let recorder = recorder.clone();
        let next_id = next_id.clone();
        let handshake_done = handshake_done.clone();
        let upstream_tx_hb = upstream_tx.clone();
        let metrics_up = metrics.clone();
        let hooks_up = hooks.clone();
        let resolved_upstream = resolved_upstream.clone();

        tokio::spawn(async move {
            // Wait until a client connects before starting upstream work.
            upstream_started.notified().await;

            // Get the upstream address (set by config or by first transparent client).
            let upstream_addr = *resolved_upstream
                .get_or_init(|| async {
                    error!("BUG: upstream address not resolved before upstream task started");
                    std::net::SocketAddr::new(
                        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                        4700,
                    )
                })
                .await;

            // Heartbeat: spawned once, skips iterations while handshake pending.
            // Checks the flag each cycle so it re-gates correctly after reconnects.
            let handshake_done_hb = handshake_done.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    if !handshake_done_hb.load(Ordering::Relaxed) {
                        continue;
                    }
                    let id = next_id.fetch_add(1, Ordering::Relaxed);
                    let msg = format!(
                        r#"{{"id":{},"method":"test_connection","params":"verify"}}"#,
                        id
                    );
                    debug!("Heartbeat: id={}", id);
                    if upstream_tx_hb.send(msg).await.is_err() {
                        break;
                    }
                }
            });

            // Take ownership of the receiver so it stays alive across reconnects.
            let mut rx = upstream_rx;

            loop {
                // Connect with retry until the telescope is reachable.
                info!("Connecting to telescope control at {}...", upstream_addr);
                let upstream = loop {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        TcpStream::connect(upstream_addr),
                    )
                    .await
                    {
                        Ok(Ok(s)) => break s,
                        Ok(Err(e)) => error!("Failed to connect to telescope control: {}", e),
                        Err(_) => error!(
                            "Timed out connecting to telescope control at {}",
                            upstream_addr
                        ),
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                };

                let _ = upstream.set_nodelay(true);
                info!("Connected to telescope control at {}", upstream_addr);

                // Reset per-connection state.
                handshake_done.store(false, Ordering::Relaxed);
                if let Some(m) = &metrics_up {
                    m.reset_telescope_state();
                }
                {
                    // Return errors to any clients waiting on in-flight requests
                    // from the previous connection — they will never be answered.
                    let mut st = state.lock().await;
                    for (_, pending) in st.pending.drain() {
                        let err = serde_json::json!({
                            "id": pending.original_id,
                            "code": -1,
                            "error": "telescope reconnecting"
                        });
                        let _ = pending
                            .client_tx
                            .try_send(serde_json::to_string(&err).unwrap_or_default());
                    }
                }

                if let Some(h) = &hooks_up {
                    h.on_upstream_connect(&upstream_addr.to_string()).await;
                }

                let (upstream_reader, mut upstream_writer) = upstream.into_split();

                // Notify this write loop when the reader exits.
                let (reader_dead_tx, reader_dead_rx) = oneshot::channel::<()>();

                tokio::spawn(upstream_reader_task(
                    upstream_reader,
                    state.clone(),
                    event_tx.clone(),
                    recorder.clone(),
                    handshake_done.clone(),
                    metrics_up.clone(),
                    hooks_up.clone(),
                    reader_dead_tx,
                ));

                // Write loop: forward messages from clients to the telescope,
                // stop when the reader signals the connection died.
                let mut reader_dead_rx = reader_dead_rx;
                loop {
                    tokio::select! {
                        msg = rx.recv() => {
                            match msg {
                                None => return, // all client senders dropped; shut down
                                Some(msg) => {
                                    debug!("-> telescope: {}", &msg[..msg.len().min(200)]);
                                    let line = format!("{}\r\n", msg);
                                    if let Err(e) = upstream_writer.write_all(line.as_bytes()).await {
                                        error!("Upstream write error: {}", e);
                                        break;
                                    }
                                }
                            }
                        }
                        _ = &mut reader_dead_rx => {
                            break; // reader exited; reconnect
                        }
                    }
                }

                warn!("Telescope control connection lost, reconnecting in 5s...");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        });
    }

    let upstream_started_once = Arc::new(AtomicBool::new(false));

    // Accept client connections.
    loop {
        let (client_stream, client_addr) = listener.accept().await?;
        let _ = client_stream.set_nodelay(true);
        info!("Control client connected: {}", client_addr);

        // In transparent mode, resolve upstream from SO_ORIGINAL_DST on first client.
        #[cfg(unix)]
        if transparent && resolved_upstream.get().is_none() {
            use std::os::unix::io::AsRawFd;
            if let Some(orig) = crate::transparent::get_original_dst(client_stream.as_raw_fd()) {
                info!(
                    "Transparent mode: resolved upstream from SO_ORIGINAL_DST: {}",
                    orig
                );
                let _ = resolved_upstream.set(orig);
            } else {
                warn!("Transparent mode: SO_ORIGINAL_DST not available on this connection");
            }
        }

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
        let metrics_c = metrics.clone();
        let hooks_c = hooks.clone();

        if let Some(m) = &metrics {
            m.control_clients.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(h) = &hooks {
            h.on_client_connect(&client_addr.to_string(), "control")
                .await;
        }
        tokio::spawn(async move {
            if let Err(e) = handle_client(
                client_stream,
                upstream_tx,
                event_rx,
                state,
                next_id,
                recorder,
                metrics_c.clone(),
                hooks_c.clone(),
            )
            .await
            {
                warn!("Control client {} error: {}", client_addr, e);
            }
            if let Some(m) = &metrics_c {
                m.control_clients.fetch_sub(1, Ordering::Relaxed);
            }
            if let Some(h) = &hooks_c {
                h.on_client_disconnect(&client_addr.to_string(), "control")
                    .await;
            }
            info!("Control client disconnected: {}", client_addr);
        });
    }
}

/// Handle a single client connection.
#[allow(clippy::too_many_arguments)]
async fn handle_client(
    stream: TcpStream,
    upstream_tx: mpsc::Sender<String>,
    mut event_rx: broadcast::Receiver<String>,
    state: Arc<Mutex<ControlState>>,
    next_id: Arc<AtomicU64>,
    recorder: Option<Arc<Recorder>>,
    metrics: Option<Arc<Metrics>>,
    hooks: Option<Arc<HookEngine>>,
) -> anyhow::Result<()> {
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let writer = Arc::new(Mutex::new(writer));

    let (response_tx, mut response_rx) = mpsc::channel::<String>(64);

    // Spawn task to forward events and responses to this client.
    // The handle is kept so we can abort the task when the read loop exits,
    // which drops the write half and sends FIN to the client promptly.
    let writer_c = writer.clone();
    let writer_task = tokio::spawn(async move {
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
    let mut line_buf = Vec::new();
    loop {
        line_buf.clear();
        // Read until newline, with a size limit to prevent unbounded allocation.
        let n = read_line_limited(&mut reader, &mut line_buf, MAX_LINE_BYTES).await?;
        if n == 0 {
            break;
        }

        // Reject oversized lines before any further processing.
        if line_buf.len() > MAX_LINE_BYTES {
            warn!(
                "Client sent oversized line ({} bytes), disconnecting",
                line_buf.len()
            );
            break;
        }

        let line = String::from_utf8_lossy(&line_buf);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Record once here (pre-remap); upstream writer must NOT
        // record again so each client message appears exactly once.
        if let Some(recorder) = &recorder {
            recorder.record_control("client", trimmed).await;
        }
        if let Some(m) = &metrics {
            m.control_tx.fetch_add(1, Ordering::Relaxed);
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                let method = crate::protocol::method_name(&v).unwrap_or("?");
                m.push_log_with_payload("ctrl-tx", method.to_string(), Some(trimmed.to_string()));
            }
        }

        let mut msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => {
                warn!(
                    "Invalid JSON from client: {}",
                    &trimmed[..trimmed.len().min(100)]
                );
                continue;
            }
        };

        // Run request hook before forwarding.
        if let Some(h) = &hooks {
            match h.on_request(&msg).await {
                HookAction::Forward => {}
                HookAction::Block => {
                    debug!(
                        "Hook blocked request: {}",
                        &trimmed[..trimmed.len().min(100)]
                    );
                    continue;
                }
                HookAction::Modify(new_json) => match serde_json::from_str(&new_json) {
                    Ok(v) => msg = v,
                    Err(e) => warn!("Hook returned invalid JSON: {}", e),
                },
                HookAction::Reply(reply_json) => {
                    debug!(
                        "Hook synthetic reply: {}",
                        &reply_json[..reply_json.len().min(100)]
                    );
                    let _ = response_tx.send(reply_json).await;
                    continue;
                }
            }
        }

        // Use get_id() which accepts any non-null JSON value (including
        // negative integers and strings) rather than json_rpc_id() which only
        // handles u64 values.
        if let Some(original_id) = protocol::get_id(&msg).cloned() {
            let remapped_id = next_id.fetch_add(1, Ordering::Relaxed);
            protocol::set_json_rpc_id(&mut msg, remapped_id);

            // Serialise before acquiring the lock so a serialisation
            // failure does not leave an orphaned pending entry.
            let forwarded = match serde_json::to_string(&msg) {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to serialise request: {}", e);
                    continue;
                }
            };

            // Enforce the pending-map limit and register the entry under a
            // single lock acquisition.  Splitting the check and insert across
            // two lock grabs (the original ordering) created a TOCTOU race:
            // concurrent clients could each pass the size check individually
            // and then all insert, collectively exceeding MAX_PENDING_REQUESTS.
            {
                let mut st = state.lock().await;
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
                st.pending.insert(
                    remapped_id,
                    PendingRequest {
                        client_tx: response_tx.clone(),
                        original_id,
                    },
                );
            }

            info!(
                "Forwarding: method={} id {:?} -> {}",
                protocol::method_name(&msg).unwrap_or("?"),
                protocol::get_id(&msg),
                remapped_id
            );

            if upstream_tx.send(forwarded).await.is_err() {
                error!("Upstream channel closed — telescope connection lost");
                // The message was never delivered, so the pending entry will
                // never receive a response.  Remove it now rather than leaking
                // it until the next reconnect cycle.
                let mut st = state.lock().await;
                st.pending.remove(&remapped_id);
                break;
            }
        } else {
            // No id field — forward as-is (notification).
            let forwarded = serde_json::to_string(&msg)?;
            if upstream_tx.send(forwarded).await.is_err() {
                error!("Upstream channel closed — telescope connection lost");
                break;
            }
        }
    }

    // Abort the writer task so the write half is dropped and the peer sees EOF.
    writer_task.abort();
    Ok(())
}

/// Read a line (up to `\n`) from reader into `buf`, stopping at `limit` bytes.
/// Returns the number of bytes read (0 = EOF).
async fn read_line_limited(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    buf: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<usize> {
    use tokio::io::AsyncBufReadExt;
    let mut total = 0usize;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(total);
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&available[..=pos]);
            total += pos + 1;
            reader.consume(pos + 1);
            return Ok(total);
        }
        let len = available.len();
        buf.extend_from_slice(available);
        total += len;
        reader.consume(len);
        if total > limit {
            return Ok(total);
        }
    }
}

/// Read responses and events from the upstream telescope.
#[allow(clippy::too_many_arguments)]
async fn upstream_reader_task(
    mut reader: tokio::net::tcp::OwnedReadHalf,
    state: Arc<Mutex<ControlState>>,
    event_tx: broadcast::Sender<String>,
    recorder: Option<Arc<Recorder>>,
    handshake_done: Arc<AtomicBool>,
    metrics: Option<Arc<Metrics>>,
    hooks: Option<Arc<HookEngine>>,
    _done_tx: oneshot::Sender<()>,
) {
    info!("Upstream reader started, waiting for telescope messages...");
    if let Some(m) = &metrics {
        m.upstream_control_up.store(true, Ordering::Relaxed);
    }

    let mut buf = Vec::with_capacity(64 * 1024);
    let mut tmp = [0u8; 32 * 1024];
    let mut total_bytes: u64 = 0;
    let mut total_messages: u64 = 0;

    loop {
        let n = match reader.read(&mut tmp).await {
            Ok(0) => {
                error!(
                    "Upstream telescope disconnected (EOF) after {} bytes, {} messages",
                    total_bytes, total_messages
                );
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
                n,
                newline_count,
                buf.len(),
                total_bytes
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

            // Update telescope state in hook engine for state-aware scripts.
            if let Some(h) = &hooks
                && protocol::is_event(&msg)
            {
                h.update_telescope_state(&msg).await;
            }

            if protocol::is_event(&msg) {
                let event_name = msg.get("Event").and_then(|v| v.as_str()).unwrap_or("?");
                debug!("Event: {}", event_name);
                if let Some(m) = &metrics {
                    m.control_events.fetch_add(1, Ordering::Relaxed);
                    m.push_log_with_payload(
                        "ctrl-evt",
                        event_name.to_string(),
                        Some(trimmed.clone()),
                    );
                    m.update_event(event_name, &msg);
                }

                // Run event hook.
                let should_forward = if let Some(h) = &hooks {
                    match h.on_event(&msg).await {
                        HookAction::Forward => true,
                        HookAction::Block => {
                            debug!("Hook blocked event: {}", event_name);
                            false
                        }
                        HookAction::Modify(_) | HookAction::Reply(_) => true, // Can't modify events meaningfully
                    }
                } else {
                    true
                };
                if should_forward {
                    let _ = event_tx.send(trimmed);
                }
            } else if let Some(remapped_id) = protocol::json_rpc_id(&msg) {
                if let Some(m) = &metrics {
                    m.control_rx.fetch_add(1, Ordering::Relaxed);
                }
                let pending = {
                    let mut st = state.lock().await;
                    st.pending.remove(&remapped_id)
                };

                if let Some(pending) = pending {
                    // Restore the original client ID (which may be any JSON type).
                    let mut response = msg;
                    protocol::set_json_rpc_id_value(&mut response, pending.original_id.clone());
                    let response_str = serde_json::to_string(&response).unwrap_or_default();

                    let method = protocol::method_name(&response).unwrap_or("?");
                    info!(
                        "Response routed: method={} id {} -> {:?}",
                        method, remapped_id, pending.original_id
                    );
                    if let Some(m) = &metrics {
                        m.push_log_with_payload(
                            "ctrl-rx",
                            method.to_string(),
                            Some(response_str.clone()),
                        );
                        m.update_response(&response);
                    }

                    if pending.client_tx.send(response_str).await.is_err() {
                        warn!(
                            "Client disconnected before response could be delivered for id {:?}",
                            pending.original_id
                        );
                    }
                } else {
                    debug!(
                        "Response for untracked id {}, routing via on_response hook",
                        remapped_id
                    );
                    let should_broadcast = if let Some(h) = &hooks {
                        match h.on_response(&msg).await {
                            HookAction::Forward => true,
                            HookAction::Block => false,
                            HookAction::Modify(_) | HookAction::Reply(_) => true,
                        }
                    } else {
                        true
                    };
                    if should_broadcast {
                        let _ = event_tx.send(trimmed);
                    }
                }
            } else {
                debug!("Unknown message type, broadcasting");
                let _ = event_tx.send(trimmed);
            }
        }

        if buf.len() > 1_000_000 {
            // A partial line this large is almost certainly malformed data.
            // Clearing and continuing would cause the tail of this oversized
            // message to be parsed as a new (invalid) JSON line.  Disconnect
            // instead so the reconnect loop can clean up pending requests and
            // start fresh.
            error!(
                "Buffer overflow ({} bytes), disconnecting upstream",
                buf.len()
            );
            break;
        }
    }

    if let Some(m) = &metrics {
        m.upstream_control_up.store(false, Ordering::Relaxed);
    }
    error!("Upstream control reader stopped — telescope connection lost");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::net::{TcpListener, TcpStream};

    /// Returns (writer_end, reader_half): write to writer_end, read from reader_half.
    async fn loopback_read_half() -> (TcpStream, tokio::net::tcp::OwnedReadHalf) {
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
        let handshake_done = Arc::new(AtomicBool::new(false));

        let task = tokio::spawn(upstream_reader_task(
            reader,
            state.clone(),
            event_tx,
            None,
            handshake_done,
            None,
            None,
            oneshot::channel().0,
        ));

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
        let handshake_done = Arc::new(AtomicBool::new(false));

        tokio::spawn(upstream_reader_task(
            reader,
            state,
            event_tx,
            None,
            handshake_done,
            None,
            None,
            oneshot::channel().0,
        ));

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
                m.insert(
                    10001,
                    PendingRequest {
                        client_tx: tx_a,
                        original_id: serde_json::Value::from(1u64),
                    },
                );
                m.insert(
                    10002,
                    PendingRequest {
                        client_tx: tx_b,
                        original_id: serde_json::Value::from(2u64),
                    },
                );
                m
            },
        }));
        let handshake_done = Arc::new(AtomicBool::new(false));

        tokio::spawn(upstream_reader_task(
            reader,
            state,
            event_tx,
            None,
            handshake_done,
            None,
            None,
            oneshot::channel().0,
        ));

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
        let state = Arc::new(Mutex::new(ControlState {
            pending: HashMap::new(),
        }));
        let (event_tx, mut event_rx) = broadcast::channel::<String>(16);
        let handshake_done = Arc::new(AtomicBool::new(false));

        tokio::spawn(upstream_reader_task(
            reader,
            state,
            event_tx,
            None,
            handshake_done,
            None,
            None,
            oneshot::channel().0,
        ));

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
        let state = Arc::new(Mutex::new(ControlState {
            pending: HashMap::new(),
        }));
        let (event_tx, mut event_rx) = broadcast::channel::<String>(16);
        let handshake_done = Arc::new(AtomicBool::new(false));

        tokio::spawn(upstream_reader_task(
            reader,
            state,
            event_tx,
            None,
            handshake_done,
            None,
            None,
            oneshot::channel().0,
        ));

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
        let state = Arc::new(Mutex::new(ControlState {
            pending: HashMap::new(),
        }));
        let (event_tx, mut event_rx) = broadcast::channel::<String>(16);
        let handshake_done = Arc::new(AtomicBool::new(false));

        tokio::spawn(upstream_reader_task(
            reader,
            state,
            event_tx,
            None,
            handshake_done,
            None,
            None,
            oneshot::channel().0,
        ));

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
        let state = Arc::new(Mutex::new(ControlState {
            pending: HashMap::new(),
        }));
        let (event_tx, mut event_rx) = broadcast::channel::<String>(16);
        let handshake_done = Arc::new(AtomicBool::new(false));

        tokio::spawn(upstream_reader_task(
            reader,
            state,
            event_tx,
            None,
            handshake_done,
            None,
            None,
            oneshot::channel().0,
        ));

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

    /// A message from the telescope that has neither an Event key nor a valid
    /// integer id falls through to the "unknown message type" broadcast branch.
    #[tokio::test]
    async fn unknown_message_type_is_broadcast_to_all_clients() {
        let (mut mock_telescope, reader) = loopback_read_half().await;
        let state = Arc::new(Mutex::new(ControlState {
            pending: HashMap::new(),
        }));
        let (event_tx, mut event_rx) = broadcast::channel::<String>(16);
        let handshake_done = Arc::new(AtomicBool::new(false));

        tokio::spawn(upstream_reader_task(
            reader,
            state,
            event_tx,
            None,
            handshake_done,
            None,
            None,
            oneshot::channel().0,
        ));

        // A message with no id and no Event key — hits the "unknown" else branch.
        mock_telescope
            .write_all(b"{\"some\":\"data\",\"no_id\":true}\r\n")
            .await
            .unwrap();

        let msg = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("timed out")
            .unwrap();

        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["some"], "data");
    }

    /// When upstream_reader_task receives multiple chunks without a newline,
    /// it must buffer them and only process the message when the newline arrives.
    #[tokio::test]
    async fn upstream_reader_reassembles_chunked_messages() {
        let (mut mock_telescope, reader) = loopback_read_half().await;
        let state = Arc::new(Mutex::new(ControlState {
            pending: HashMap::new(),
        }));
        let (event_tx, mut event_rx) = broadcast::channel::<String>(16);
        let handshake_done = Arc::new(AtomicBool::new(false));

        tokio::spawn(upstream_reader_task(
            reader,
            state,
            event_tx,
            None,
            handshake_done,
            None,
            None,
            oneshot::channel().0,
        ));

        // Send the event in two separate writes without flushing between them.
        mock_telescope
            .write_all(b"{\"Event\":\"Partial")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        mock_telescope.write_all(b"Data\"}\r\n").await.unwrap();

        let msg = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("timed out")
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["Event"], "PartialData");
    }

    /// Bug regression: try_send silently dropped responses when the client's
    /// response channel was full.  With send().await the task blocks until the
    /// channel has space, so the response is always delivered.
    #[tokio::test]
    async fn full_response_channel_does_not_drop_response() {
        // Capacity-1 channel, pre-filled so send().await must block until drained.
        let (client_tx, mut client_rx) = mpsc::channel::<String>(1);
        client_tx.try_send("placeholder".to_string()).unwrap();

        let (mut mock_telescope, reader) = loopback_read_half().await;
        let (event_tx, _) = broadcast::channel::<String>(16);
        let state = pending_state(10000, client_tx, 42);
        let handshake_done = Arc::new(AtomicBool::new(false));

        tokio::spawn(upstream_reader_task(
            reader,
            state,
            event_tx,
            None,
            handshake_done,
            None,
            None,
            oneshot::channel().0,
        ));

        // Trigger the task to try delivering a response to the full channel.
        mock_telescope
            .write_all(b"{\"id\":10000,\"code\":0}\r\n")
            .await
            .unwrap();

        // Give the task time to receive the message and block on send().await.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Drain the placeholder — this unblocks the send().await in the task.
        let first = client_rx.recv().await.unwrap();
        assert_eq!(first, "placeholder");

        // The actual response must now arrive (was not dropped).
        let response = tokio::time::timeout(Duration::from_secs(1), client_rx.recv())
            .await
            .expect("response was not delivered after channel drained — was it dropped?")
            .expect("channel closed unexpectedly");

        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["id"], 42, "original id must be restored");
        assert_eq!(v["code"], 0);
    }

    /// Bug regression: when upstream_tx.send() failed after a pending entry was
    /// already inserted, the entry leaked in the map until the next reconnect.
    /// Now the entry is removed before breaking out of the read loop.
    #[tokio::test]
    async fn pending_entry_removed_when_upstream_send_fails() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client_stream = TcpStream::connect(addr).await.unwrap();
        let (mut test_side, _) = listener.accept().await.unwrap();

        // Drop the receiver so every upstream send() returns Err immediately.
        let (upstream_tx, upstream_rx) = mpsc::channel::<String>(1);
        drop(upstream_rx);

        let (_event_tx, event_rx) = broadcast::channel::<String>(16);
        let state = Arc::new(Mutex::new(ControlState {
            pending: HashMap::new(),
        }));
        let next_id = Arc::new(AtomicU64::new(100_000));

        let state_c = state.clone();
        let task = tokio::spawn(handle_client(
            client_stream,
            upstream_tx,
            event_rx,
            state_c,
            next_id,
            None,
            None,
            None,
        ));

        // Send a request with an id so it gets inserted into the pending map
        // before the send fails.
        test_side
            .write_all(b"{\"id\":1,\"method\":\"test\",\"params\":\"verify\"}\r\n")
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("handle_client did not exit after upstream send failure")
            .unwrap() // JoinError
            .unwrap(); // anyhow::Error

        let st = state.lock().await;
        assert!(
            st.pending.is_empty(),
            "pending entry must be removed when upstream send fails, got {} entries",
            st.pending.len()
        );
    }

    /// Bug regression: on buffer overflow the old code called buf.clear() and
    /// continued, which caused the tail of the oversized message to be parsed as
    /// a new (malformed) JSON line on the next newline.  The fix breaks out of
    /// the loop so a clean reconnect can happen instead.
    #[tokio::test]
    async fn buffer_overflow_disconnects_upstream() {
        let (mut mock_telescope, reader) = loopback_read_half().await;
        let state = Arc::new(Mutex::new(ControlState {
            pending: HashMap::new(),
        }));
        let (event_tx, _) = broadcast::channel::<String>(16);
        let handshake_done = Arc::new(AtomicBool::new(false));

        let task = tokio::spawn(upstream_reader_task(
            reader,
            state,
            event_tx,
            None,
            handshake_done,
            None,
            None,
            oneshot::channel().0,
        ));

        // Send more than 1 MB without any newline — saturates the internal buffer.
        let junk = vec![b'x'; 1_100_000];
        mock_telescope.write_all(&junk).await.unwrap();
        mock_telescope.flush().await.unwrap();

        // Task must exit cleanly rather than clearing and continuing.
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("upstream_reader_task did not disconnect on buffer overflow")
            .unwrap();
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
