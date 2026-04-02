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
            let Ok((stream, _)) = listener.accept().await else { break };
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
        let Ok((stream, _)) = listener.accept().await else { return };
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
    tokio::spawn(seestar_proxy::control::run(proxy_addr, telescope_addr, None, None, None));
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
async fn read_json(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> serde_json::Value {
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

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
        telescope_addr,
        Some(recorder.clone()),
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

    let content =
        tokio::fs::read_to_string(record_dir.join("control.jsonl"))
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
    let record_dir =
        std::env::temp_dir().join(format!("seestar_dup_rec_{pid}_{n}"));

    let recorder = Arc::new(
        seestar_proxy::recorder::Recorder::new(&record_dir)
            .await
            .unwrap(),
    );

    let telescope_addr = start_echo_telescope().await;
    let proxy_addr = free_addr();
    tokio::spawn(seestar_proxy::control::run(
        proxy_addr,
        telescope_addr,
        Some(recorder.clone()),
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
    let result =
        tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf)).await;

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
            .write_all(
                format!("{{\"id\":{i},\"method\":\"fill\"}}\r\n").as_bytes(),
            )
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
