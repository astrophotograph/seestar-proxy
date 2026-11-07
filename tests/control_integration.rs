/// Integration tests for the control proxy (`control::run` + `handle_client`).
///
/// Each test spins up:
///   1. A mock telescope TCP server (simulates port 4700 on a real Seestar)
///   2. The control proxy via `control::run()`
///   3. One or more test clients that send JSON-RPC requests
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

// ── helpers ───────────────────────────────────────────────────────────────────

/// Bind on :0, record the assigned port, drop the listener, return the address.
/// There is a tiny TOCTOU window but it is negligible on loopback.
fn free_addr() -> SocketAddr {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap()
}

/// Poll `addr` until a TCP connection succeeds or `timeout` elapses.
async fn wait_for_tcp(addr: SocketAddr, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if TcpStream::connect(addr).await.is_ok() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "port {addr} never became ready"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Spawn a mock telescope that echoes each JSON-RPC request back as
/// `{"id": <same>, "code": 0, "result": null}`.
/// Returns the address the mock is listening on.
async fn start_echo_telescope() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        // Accept connections in a loop so multiple clients can connect
        // (the proxy itself opens one persistent connection).
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                let mut reader = BufReader::new(reader);
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        break;
                    }
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                        let resp = serde_json::json!({"id": v["id"], "code": 0, "result": null});
                        let _ = writer
                            .write_all(serde_json::to_string(&resp).unwrap().as_bytes())
                            .await;
                        let _ = writer.write_all(b"\r\n").await;
                        let _ = writer.flush().await;
                    }
                }
            });
        }
    });

    addr
}

/// Spawn a mock telescope that emits an event in response to each request,
/// followed by the normal response. This lets tests control *when* the event
/// is emitted: once clients are subscribed, a request from any one of them
/// triggers the broadcast to all of them.
async fn start_event_on_request_telescope(event_json: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                break;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                // First emit the event (will be broadcast to all clients)…
                let _ = writer.write_all(event_json.as_bytes()).await;
                let _ = writer.write_all(b"\r\n").await;
                // …then the targeted response.
                let resp = serde_json::json!({"id": v["id"], "code": 0});
                let _ = writer
                    .write_all(serde_json::to_string(&resp).unwrap().as_bytes())
                    .await;
                let _ = writer.write_all(b"\r\n").await;
                let _ = writer.flush().await;
            }
        }
    });

    addr
}

/// Start the control proxy and wait until its listen port is open.
async fn start_proxy(telescope_addr: SocketAddr) -> SocketAddr {
    let proxy_addr = free_addr();
    tokio::spawn(seestar_proxy::control::run(
        proxy_addr,
        Some(telescope_addr),
        false,
        None,
        None,
        None,
        None,
    ));
    wait_for_tcp(proxy_addr, Duration::from_secs(2)).await;
    proxy_addr
}

/// Connect a client and return split (reader, writer).
async fn connect_client(
    addr: SocketAddr,
) -> (
    BufReader<tokio::net::tcp::OwnedReadHalf>,
    tokio::net::tcp::OwnedWriteHalf,
) {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (r, w) = stream.into_split();
    (BufReader::new(r), w)
}

/// Read one `\r\n`-terminated line and parse it as JSON.
async fn read_json(reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> serde_json::Value {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for line")
        .expect("read error");
    serde_json::from_str(line.trim()).expect("invalid JSON")
}

/// A telescope that accepts connections, drains all incoming data, but never
/// sends any responses. Used to fill the proxy's pending-request map.
async fn start_silent_telescope() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            // Keep both halves alive so the proxy doesn't see EOF and reconnect.
            let (mut r, w) = stream.into_split();
            tokio::spawn(async move {
                let _keep_alive = w;
                let mut buf = [0u8; 4096];
                while r.read(&mut buf).await.unwrap_or(0) > 0 {}
            });
        }
    });
    addr
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn single_client_request_response() {
    let telescope_addr = start_echo_telescope().await;
    let proxy_addr = start_proxy(telescope_addr).await;

    let (mut reader, mut writer) = connect_client(proxy_addr).await;

    writer
        .write_all(b"{\"id\":1,\"method\":\"get_device_state\"}\r\n")
        .await
        .unwrap();

    let v = read_json(&mut reader).await;
    assert_eq!(v["id"], 1, "original id must be preserved for the client");
    assert_eq!(v["code"], 0);
}

#[tokio::test]
async fn id_remapping_is_transparent_to_client() {
    // The proxy internally remaps IDs to 10000+; the client should always
    // see its own original IDs in responses.
    let telescope_addr = start_echo_telescope().await;
    let proxy_addr = start_proxy(telescope_addr).await;

    let (mut reader, mut writer) = connect_client(proxy_addr).await;

    for original_id in [42u64, 1, 9999, 0] {
        let req = serde_json::json!({"id": original_id, "method": "test"});
        writer
            .write_all(serde_json::to_string(&req).unwrap().as_bytes())
            .await
            .unwrap();
        writer.write_all(b"\r\n").await.unwrap();

        let v = read_json(&mut reader).await;
        assert_eq!(
            v["id"], original_id,
            "id {original_id} not restored in response"
        );
    }
}

#[tokio::test]
async fn multiple_clients_receive_correct_responses() {
    // Two clients send requests with the same ID (1). Each must get back only
    // its own response with id=1, not the other client's response.
    let telescope_addr = start_echo_telescope().await;
    let proxy_addr = start_proxy(telescope_addr).await;

    let (mut r_a, mut w_a) = connect_client(proxy_addr).await;
    let (mut r_b, mut w_b) = connect_client(proxy_addr).await;

    // Both clients use id=1 — the proxy must demultiplex correctly.
    w_a.write_all(b"{\"id\":1,\"method\":\"alpha\"}\r\n")
        .await
        .unwrap();
    w_b.write_all(b"{\"id\":1,\"method\":\"beta\"}\r\n")
        .await
        .unwrap();

    let va = read_json(&mut r_a).await;
    let vb = read_json(&mut r_b).await;

    assert_eq!(va["id"], 1);
    assert_eq!(vb["id"], 1);
    assert_eq!(va["code"], 0);
    assert_eq!(vb["code"], 0);
}

#[tokio::test]
async fn event_broadcast_to_all_clients() {
    let event = r#"{"Event":"PiStatus","temp":45.0}"#;
    // The telescope emits the event only when it receives a request, so we
    // can guarantee both clients are subscribed before the event is emitted.
    let telescope_addr = start_event_on_request_telescope(event).await;
    let proxy_addr = start_proxy(telescope_addr).await;

    let (mut r_a, mut w_a) = connect_client(proxy_addr).await;
    let (mut r_b, _w_b) = connect_client(proxy_addr).await;

    // Small pause to let both client handler tasks subscribe to the broadcast
    // channel before the event is emitted.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Trigger the event by sending a request from client A.
    w_a.write_all(b"{\"id\":1,\"method\":\"trigger\"}\r\n")
        .await
        .unwrap();

    // Client B never sent a request, so the first message it receives must be
    // the broadcast event.
    let vb = read_json(&mut r_b).await;
    assert_eq!(vb["Event"], "PiStatus");
    assert_eq!(vb["temp"], 45.0);

    // Client A receives either the event or the response first (order depends
    // on select! scheduling); both must eventually arrive.
    let va1 = read_json(&mut r_a).await;
    let va2 = read_json(&mut r_a).await;
    let has_event = va1.get("Event").is_some() || va2.get("Event").is_some();
    let has_response = va1.get("code").is_some() || va2.get("code").is_some();
    assert!(has_event, "client A must receive the event");
    assert!(has_response, "client A must receive the response");
}

#[tokio::test]
async fn proxy_handles_multiple_sequential_requests_from_same_client() {
    let telescope_addr = start_echo_telescope().await;
    let proxy_addr = start_proxy(telescope_addr).await;

    let (mut reader, mut writer) = connect_client(proxy_addr).await;

    for id in 1u64..=5 {
        let req = serde_json::json!({"id": id, "method": "ping"});
        writer
            .write_all(serde_json::to_string(&req).unwrap().as_bytes())
            .await
            .unwrap();
        writer.write_all(b"\r\n").await.unwrap();

        let v = read_json(&mut reader).await;
        assert_eq!(v["id"], id);
    }
}

#[tokio::test]
async fn proxy_ignores_invalid_json_from_client() {
    let telescope_addr = start_echo_telescope().await;
    let proxy_addr = start_proxy(telescope_addr).await;

    let (mut reader, mut writer) = connect_client(proxy_addr).await;

    // Send garbage, then a valid request — the proxy should skip the garbage
    // and process the valid request normally.
    writer.write_all(b"not valid json\r\n").await.unwrap();
    writer
        .write_all(b"{\"id\":7,\"method\":\"test\"}\r\n")
        .await
        .unwrap();

    let v = read_json(&mut reader).await;
    assert_eq!(v["id"], 7);
}

#[tokio::test]
async fn proxy_records_control_traffic() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let record_dir = std::env::temp_dir().join(format!("seestar_ctrl_rec_{pid}_{id}"));

    let recorder = Arc::new(
        seestar_proxy::recorder::Recorder::new(&record_dir)
            .await
            .unwrap(),
    );

    let telescope_addr = start_echo_telescope().await;
    let proxy_addr = free_addr();
    tokio::spawn(seestar_proxy::control::run(
        proxy_addr,
        Some(telescope_addr),
        false,
        Some(recorder.clone()),
        None,
        None,
        None,
    ));
    wait_for_tcp(proxy_addr, Duration::from_secs(2)).await;

    let (mut reader, mut writer) = connect_client(proxy_addr).await;
    writer
        .write_all(b"{\"id\":1,\"method\":\"test\"}\r\n")
        .await
        .unwrap();
    let _ = read_json(&mut reader).await;

    // Give the async record writes a moment to flush.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let content = tokio::fs::read_to_string(record_dir.join("control.jsonl"))
        .await
        .unwrap();
    assert!(
        !content.is_empty(),
        "control.jsonl should contain recorded messages"
    );
    // Every line must be valid JSON with a timestamp and direction.
    for line in content.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(v["timestamp"].is_number());
        assert!(v["direction"].is_string());
    }
}

// ── Bug regression tests ──────────────────────────────────────────────────────

/// Bug 1 — double recording: upstream_writer_task re-records every client
/// message (with the remapped id), so each request appears twice in
/// control.jsonl. One request + one response should produce exactly 2 lines.
#[tokio::test]
async fn recorder_logs_each_message_exactly_once() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let record_dir = std::env::temp_dir().join(format!("seestar_dup_rec_{pid}_{n}"));

    let recorder = Arc::new(
        seestar_proxy::recorder::Recorder::new(&record_dir)
            .await
            .unwrap(),
    );

    let telescope_addr = start_echo_telescope().await;
    let proxy_addr = free_addr();
    tokio::spawn(seestar_proxy::control::run(
        proxy_addr,
        Some(telescope_addr),
        false,
        Some(recorder.clone()),
        None,
        None,
        None,
    ));
    wait_for_tcp(proxy_addr, Duration::from_secs(2)).await;

    let (mut reader, mut writer) = connect_client(proxy_addr).await;
    writer
        .write_all(b"{\"id\":1,\"method\":\"test\"}\r\n")
        .await
        .unwrap();
    let _ = read_json(&mut reader).await; // wait for response
    tokio::time::sleep(Duration::from_millis(50)).await;

    let content = tokio::fs::read_to_string(record_dir.join("control.jsonl"))
        .await
        .unwrap();
    let lines: Vec<&str> = content.lines().collect();

    // One client request + one telescope response = exactly 2 entries.
    // Bug: currently produces 3 (client logged once in handle_client +
    // once in upstream_writer_task, then telescope response = 3 total).
    assert_eq!(
        lines.len(),
        2,
        "expected exactly 2 log entries (1 client + 1 telescope), got {}:\n{}",
        lines.len(),
        content
    );
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    let last: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(first["direction"], "client");
    assert_eq!(last["direction"], "telescope");
}

/// Bug 2 — negative / non-u64 JSON-RPC ids: json_rpc_id() uses as_u64()
/// which returns None for negative integers, so no pending entry is
/// registered and the telescope's response is silently dropped.
#[tokio::test]
async fn proxy_handles_negative_request_id() {
    let telescope_addr = start_echo_telescope().await;
    let proxy_addr = start_proxy(telescope_addr).await;

    let (mut reader, mut writer) = connect_client(proxy_addr).await;
    // Negative id is valid per JSON-RPC 2.0
    writer
        .write_all(b"{\"id\":-1,\"method\":\"test\"}\r\n")
        .await
        .unwrap();

    // Bug: response is dropped; this times out.
    let v = read_json(&mut reader).await;
    assert_eq!(v["id"], -1i64, "response must echo the negative id back");
    assert_eq!(v["code"], 0);
}

/// Bug 6 — unbounded BufReader reads: read_line() accumulates an entire line
/// before returning. A client sending a multi-megabyte line without a newline
/// causes unbounded memory growth. After the fix the proxy must close the
/// connection when the line length limit is exceeded.
#[tokio::test]
async fn oversized_line_closes_connection() {
    let telescope_addr = start_echo_telescope().await;
    let proxy_addr = start_proxy(telescope_addr).await;

    let mut stream = TcpStream::connect(proxy_addr).await.unwrap();

    // Send 2 MB + newline — exceeds any reasonable single-line limit.
    let mut big: Vec<u8> = vec![b'X'; 2 * 1024 * 1024];
    big.push(b'\n');
    let _ = stream.write_all(&big).await;
    let _ = stream.flush().await;

    // The proxy should close the connection.
    // Bug: proxy parses invalid JSON, logs a warning, and keeps the
    // connection open → read() blocks → we time out.
    let mut buf = [0u8; 1];
    let result = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf)).await;

    match result {
        Ok(Ok(0)) | Ok(Err(_)) => {} // EOF or reset — expected after fix
        Ok(Ok(n)) => panic!("proxy should disconnect, but sent {n} bytes"),
        Err(_) => panic!("proxy did not close connection after oversized line"),
    }
}

/// Bug 7 — unbounded pending-request map: unanswered requests accumulate
/// forever. After the fix, once the limit is reached the proxy returns an
/// immediate error response rather than queuing the request.
#[tokio::test]
async fn pending_map_rejects_overflow() {
    // The telescope accepts the connection and drains writes but never replies,
    // so pending entries pile up.
    const LIMIT: usize = 1_024; // must match MAX_PENDING_REQUESTS in control.rs

    let telescope_addr = start_silent_telescope().await;
    let proxy_addr = start_proxy(telescope_addr).await;

    let (mut reader, mut writer) = connect_client(proxy_addr).await;

    // Fill the map to capacity.
    for i in 0..LIMIT {
        writer
            .write_all(format!("{{\"id\":{i},\"method\":\"fill\"}}\r\n").as_bytes())
            .await
            .unwrap();
    }
    // Give the proxy time to process all 1024 requests through the pending map.
    // Each request involves channel sends which may briefly block under pressure.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // This request must exceed the limit.
    writer
        .write_all(b"{\"id\":99999,\"method\":\"overflow\"}\r\n")
        .await
        .unwrap();

    // Bug: proxy queues the request like all others; no response ever arrives.
    let v = tokio::time::timeout(Duration::from_secs(3), read_json(&mut reader))
        .await
        .expect("proxy should send an immediate error when the pending map is full");

    assert_eq!(v["id"], 99999i64, "error response must echo the request id");
    // Non-zero code signals an error.
    let code = v
        .get("code")
        .and_then(|c| c.as_i64())
        .expect("error response must have a 'code' field");
    assert_ne!(code, 0, "code must be non-zero to signal an error");
}

/// A notification message (no id field) is forwarded to the telescope without
/// registering a pending entry. The telescope should receive it; the client
/// gets no response (notifications have no reply).
#[tokio::test]
async fn notification_without_id_is_forwarded_to_telescope() {
    // Use a telescope that captures what it receives.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let telescope_addr = listener.local_addr().unwrap();
    let (received_tx, mut received_rx) = tokio::sync::mpsc::channel::<String>(8);

    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let (reader, _writer) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                break;
            }
            let trimmed = line.trim().to_string();
            if !trimmed.is_empty() {
                let _ = received_tx.send(trimmed).await;
            }
        }
    });

    let proxy_addr = start_proxy(telescope_addr).await;
    let (_reader, mut writer) = connect_client(proxy_addr).await;

    // Send a notification (no id field).
    writer
        .write_all(b"{\"method\":\"notify_something\",\"params\":[]}\r\n")
        .await
        .unwrap();

    let received = tokio::time::timeout(Duration::from_secs(2), received_rx.recv())
        .await
        .expect("timed out — notification was not forwarded to telescope")
        .expect("channel closed");

    let v: serde_json::Value = serde_json::from_str(&received).unwrap();
    assert_eq!(v["method"], "notify_something");
    assert!(
        v.get("id").is_none(),
        "notification must have no id field after forwarding"
    );
}

/// A client that sends a request with a string id (valid JSON-RPC) gets the
/// original string id echoed back in the response.
#[tokio::test]
async fn string_id_is_preserved_in_response() {
    let telescope_addr = start_echo_telescope().await;
    let proxy_addr = start_proxy(telescope_addr).await;

    let (mut reader, mut writer) = connect_client(proxy_addr).await;
    writer
        .write_all(b"{\"id\":\"my-string-id\",\"method\":\"test\"}\r\n")
        .await
        .unwrap();

    let v = read_json(&mut reader).await;
    assert_eq!(v["id"], "my-string-id", "string id must be preserved");
    assert_eq!(v["code"], 0);
}

/// A client that disconnects abruptly (without sending a request) should not
/// crash the proxy — subsequent clients must still work.
#[tokio::test]
async fn proxy_survives_abrupt_client_disconnect() {
    let telescope_addr = start_echo_telescope().await;
    let proxy_addr = start_proxy(telescope_addr).await;

    // Connect and immediately drop without sending anything.
    let rude_client = TcpStream::connect(proxy_addr).await.unwrap();
    drop(rude_client);
    tokio::time::sleep(Duration::from_millis(50)).await;

    // A normal client must still work.
    let (mut reader, mut writer) = connect_client(proxy_addr).await;
    writer
        .write_all(b"{\"id\":1,\"method\":\"after_disconnect\"}\r\n")
        .await
        .unwrap();
    let v = read_json(&mut reader).await;
    assert_eq!(v["id"], 1);
    assert_eq!(v["code"], 0);
}

/// Bug regression: TOCTOU in pending-map overflow check.
///
/// The old code checked `pending.len() >= MAX_PENDING_REQUESTS` under one
/// lock, released it, then inserted under a second lock.  Two concurrent
/// clients could each pass the check and then both insert, collectively
/// exceeding the limit.  The fix collapses the check and insert into a
/// single critical section so the limit is always enforced exactly.
///
/// This test fills the map to capacity from one client, then verifies that
/// a second concurrent client also gets the error response immediately —
/// demonstrating the limit is checked atomically, not optimistically.
#[tokio::test]
async fn concurrent_clients_cannot_exceed_pending_map_limit() {
    const LIMIT: usize = 1_024; // must match MAX_PENDING_REQUESTS

    let telescope_addr = start_silent_telescope().await;
    let proxy_addr = start_proxy(telescope_addr).await;

    // Client A fills the map to capacity.
    let (mut r_a, mut w_a) = connect_client(proxy_addr).await;
    for i in 0..LIMIT {
        w_a.write_all(format!("{{\"id\":{i},\"method\":\"fill\"}}\r\n").as_bytes())
            .await
            .unwrap();
    }
    // Wait long enough for all requests to land in the pending map.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Client B now tries to send a request concurrently with client A's map
    // being at capacity.  It must receive an immediate error (not time out).
    let (mut r_b, mut w_b) = connect_client(proxy_addr).await;
    w_b.write_all(b"{\"id\":88888,\"method\":\"concurrent_overflow\"}\r\n")
        .await
        .unwrap();

    let vb = tokio::time::timeout(Duration::from_secs(3), read_json(&mut r_b))
        .await
        .expect("second client must receive immediate error when map is full");

    assert_eq!(vb["id"], 88888i64);
    let code_b = vb["code"].as_i64().expect("must have code");
    assert_ne!(code_b, 0, "error code must be non-zero");

    // Client A's overflow request (one more than LIMIT) must also be rejected.
    w_a.write_all(b"{\"id\":99999,\"method\":\"overflow\"}\r\n")
        .await
        .unwrap();
    let va = tokio::time::timeout(Duration::from_secs(3), read_json(&mut r_a))
        .await
        .expect("first client must receive immediate error when map is full");

    assert_eq!(va["id"], 99999i64);
    assert_ne!(va["code"].as_i64().unwrap_or(0), 0);
}

// ── reconnect helpers ─────────────────────────────────────────────────────────

/// One accepted proxy→telescope connection. Dropping it closes the socket,
/// which the proxy sees as the telescope dying.
struct TelescopeConn {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
}

impl TelescopeConn {
    /// Read the next line, parsed as JSON. `None` on timeout or EOF.
    async fn next_line(&mut self, timeout: Duration) -> Option<serde_json::Value> {
        let mut line = String::new();
        loop {
            line.clear();
            let n = tokio::time::timeout(timeout, self.reader.read_line(&mut line))
                .await
                .ok()?
                .ok()?;
            if n == 0 {
                return None;
            }
            if !line.trim().is_empty() {
                return serde_json::from_str(line.trim()).ok();
            }
        }
    }

    /// Next client request, skipping heartbeat `test_connection` messages.
    async fn next_request(&mut self) -> serde_json::Value {
        loop {
            let v = self
                .next_line(Duration::from_secs(10))
                .await
                .expect("telescope connection closed or timed out waiting for a request");
            if v["method"] != "test_connection" {
                return v;
            }
        }
    }

    async fn send(&mut self, v: serde_json::Value) {
        let line = format!("{}\r\n", serde_json::to_string(&v).unwrap());
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.flush().await.unwrap();
    }
}

/// A mock telescope whose listener outlives each connection: every accepted
/// connection is handed to the test, which can close it and then wait for the
/// proxy to reconnect.
struct ReconnectTelescope {
    addr: SocketAddr,
    conns: tokio::sync::mpsc::UnboundedReceiver<TelescopeConn>,
}

impl ReconnectTelescope {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, conns) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (r, writer) = stream.into_split();
                let conn = TelescopeConn {
                    reader: BufReader::new(r),
                    writer,
                };
                if tx.send(conn).is_err() {
                    break;
                }
            }
        });
        Self { addr, conns }
    }

    /// Wait for the proxy to (re)connect. The proxy waits 5 s between attempts.
    async fn accept(&mut self) -> TelescopeConn {
        tokio::time::timeout(Duration::from_secs(15), self.conns.recv())
            .await
            .expect("proxy never (re)connected to the telescope")
            .expect("listener task ended")
    }
}

async fn start_proxy_with_metrics(
    telescope_addr: SocketAddr,
) -> (SocketAddr, Arc<seestar_proxy::metrics::Metrics>) {
    let proxy_addr = free_addr();
    let metrics = seestar_proxy::metrics::Metrics::new();
    tokio::spawn(seestar_proxy::control::run(
        proxy_addr,
        Some(telescope_addr),
        false,
        None,
        Some(metrics.clone()),
        None,
        None,
    ));
    wait_for_tcp(proxy_addr, Duration::from_secs(2)).await;
    (proxy_addr, metrics)
}

// ── reconnect tests ───────────────────────────────────────────────────────────

#[tokio::test]
async fn upstream_drop_with_inflight_requests_returns_drain_error_to_each_client() {
    let mut telescope = ReconnectTelescope::start().await;
    let proxy_addr = start_proxy(telescope.addr).await;

    let (mut r_a, mut w_a) = connect_client(proxy_addr).await;
    let (mut r_b, mut w_b) = connect_client(proxy_addr).await;
    w_a.write_all(b"{\"id\":11,\"method\":\"slow\"}\r\n")
        .await
        .unwrap();
    w_b.write_all(b"{\"id\":22,\"method\":\"slow\"}\r\n")
        .await
        .unwrap();

    // Telescope receives both requests, never answers, then dies.
    let mut conn = telescope.accept().await;
    conn.next_request().await;
    conn.next_request().await;
    drop(conn);

    // Drain happens once the proxy has reconnected.
    let _conn2 = telescope.accept().await;
    let a = read_json(&mut r_a).await;
    let b = read_json(&mut r_b).await;
    let drain =
        |id: i64| serde_json::json!({"id": id, "code": -1, "error": "telescope reconnecting"});
    assert_eq!(a, drain(11));
    assert_eq!(b, drain(22));
}

#[tokio::test]
async fn request_after_reconnect_is_answered_with_original_client_id() {
    let mut telescope = ReconnectTelescope::start().await;
    let proxy_addr = start_proxy(telescope.addr).await;
    let (mut r, mut w) = connect_client(proxy_addr).await;

    w.write_all(b"{\"id\":1,\"method\":\"first\"}\r\n")
        .await
        .unwrap();
    let mut conn = telescope.accept().await;
    conn.next_request().await;
    drop(conn);

    let mut conn2 = telescope.accept().await;
    // The in-flight request is drained with an error first.
    assert_eq!(read_json(&mut r).await["code"], -1);

    w.write_all(b"{\"id\":7,\"method\":\"second\"}\r\n")
        .await
        .unwrap();
    let req = conn2.next_request().await;
    assert_eq!(req["method"], "second");
    assert_ne!(req["id"], 7, "proxy must remap the client id upstream");
    conn2
        .send(serde_json::json!({"id": req["id"], "code": 0, "result": "ok"}))
        .await;

    let resp = read_json(&mut r).await;
    assert_eq!(resp["id"], 7);
    assert_eq!(resp["result"], "ok");
}

#[tokio::test]
async fn reconnect_resets_transient_telescope_state_but_keeps_battery_and_temp() {
    let mut telescope = ReconnectTelescope::start().await;
    let (proxy_addr, metrics) = start_proxy_with_metrics(telescope.addr).await;
    let (mut r, mut w) = connect_client(proxy_addr).await;

    // The request proves the client is subscribed before events are emitted,
    // and doubles as the in-flight request whose drain error marks the reset.
    w.write_all(b"{\"id\":1,\"method\":\"slow\"}\r\n")
        .await
        .unwrap();
    let mut conn = telescope.accept().await;
    conn.next_request().await;
    conn.send(serde_json::json!({"Event": "Stack", "count": 5}))
        .await;
    conn.send(serde_json::json!({"Event": "AutoGoto"})).await;
    conn.send(serde_json::json!({"Event": "PiStatus", "temp": 45.0, "battery_capacity": 80}))
        .await;
    for _ in 0..3 {
        read_json(&mut r).await;
    }
    let before = metrics.telescope_status();
    assert!(
        before.is_stacking && before.is_goto,
        "precondition: flags set"
    );
    assert_eq!(before.stack_count, 5);
    drop(conn);

    let _conn2 = telescope.accept().await;
    // reset_telescope_state runs before the pending map is drained.
    assert_eq!(read_json(&mut r).await["code"], -1);

    let after = metrics.telescope_status();
    assert!(!after.is_stacking);
    assert!(!after.is_goto);
    assert!(!after.is_homing);
    assert_eq!(after.stack_count, 0);
    assert_eq!(after.temperature, Some(45.0));
    assert_eq!(after.battery, Some(80));
}

#[tokio::test]
async fn heartbeat_after_reconnect_waits_for_first_telescope_message() {
    let mut telescope = ReconnectTelescope::start().await;
    let proxy_addr = start_proxy(telescope.addr).await;
    let (mut r, mut w) = connect_client(proxy_addr).await;

    // Complete the handshake on the first connection, then kill it.
    w.write_all(b"{\"id\":1,\"method\":\"hello\"}\r\n")
        .await
        .unwrap();
    let mut conn = telescope.accept().await;
    conn.next_request().await;
    conn.send(serde_json::json!({"Event": "PiStatus", "temp": 1.0}))
        .await;
    read_json(&mut r).await; // proxy has processed it => handshake_done is set
    drop(conn);

    // While the link was down, one heartbeat tick fired with the handshake
    // still marked done; that message stays queued and is flushed first on the
    // new connection. Any heartbeat beyond it means the gate did not re-arm.
    let mut conn2 = telescope.accept().await;
    let mut heartbeats = 0;
    // Fixed observation window (> one 5 s heartbeat interval) so a missing gate
    // fails the assertion instead of looping forever.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(7);
    while let Some(left) = deadline.checked_duration_since(tokio::time::Instant::now()) {
        let Some(v) = conn2.next_line(left).await else {
            break;
        };
        if v["method"] == "test_connection" {
            heartbeats += 1;
        }
    }
    assert!(
        heartbeats <= 1,
        "heartbeats sent before the telescope's first message: {heartbeats}"
    );

    // Once the telescope speaks, the heartbeat resumes.
    conn2
        .send(serde_json::json!({"Event": "PiStatus", "temp": 2.0}))
        .await;
    let v = conn2
        .next_line(Duration::from_secs(7))
        .await
        .expect("heartbeat should resume after the first telescope message");
    assert_eq!(v["method"], "test_connection");
}
