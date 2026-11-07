/// Loader tests for `ReplaySession::load` against a vendored real capture
/// (`tests/data/session_s50_fw670/`), so CI always exercises real telescope bytes.
use seestar_proxy::replay::ReplaySession;
use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/session_s50_fw670")
}

/// Count jsonl lines by direction: (telescope, total non-empty).
fn count_lines() -> (usize, usize) {
    let text = std::fs::read_to_string(fixture_dir().join("control.jsonl")).unwrap();
    let mut telescope = 0;
    let mut total = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        total += 1;
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        if v["direction"] == "telescope" {
            telescope += 1;
        }
    }
    (telescope, total)
}

#[test]
fn replay_session_load_vendored_session_succeeds() {
    assert!(ReplaySession::load(&fixture_dir()).is_ok());
}

#[test]
fn replay_session_load_vendored_session_keeps_only_telescope_messages() {
    let session = ReplaySession::load(&fixture_dir()).unwrap();
    let (telescope, _) = count_lines();
    assert!(telescope > 0);
    assert_eq!(session.control_messages.len(), telescope);
}

#[test]
fn replay_session_load_vendored_session_has_no_frames() {
    let session = ReplaySession::load(&fixture_dir()).unwrap();
    assert!(session.frame_paths.is_empty());
}

#[test]
fn replay_session_load_vendored_manifest_count_matches_jsonl_lines() {
    let text = std::fs::read_to_string(fixture_dir().join("manifest.json")).unwrap();
    let manifest: serde_json::Value = serde_json::from_str(&text).unwrap();
    let (_, total) = count_lines();
    assert_eq!(
        manifest["control_message_count"].as_u64().unwrap() as usize,
        total
    );
}

// ── record → replay round trip ───────────────────────────────────────────────

mod round_trip {
    use seestar_proxy::control;
    use seestar_proxy::metrics::{Metrics, TelescopeStatusSnapshot};
    use seestar_proxy::recorder::Recorder;
    use seestar_proxy::replay::ReplaySession;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    const EVENT_PI: &str =
        r#"{"Event":"PiStatus","Timestamp":"1","battery_capacity":87,"temp":21.5}"#;
    const EVENT_STACK: &str = r#"{"Event":"Stack","Timestamp":"2","count":3}"#;

    fn free_addr() -> SocketAddr {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
    }

    async fn wait_for_tcp(addr: SocketAddr) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while TcpStream::connect(addr).await.is_err() {
            assert!(tokio::time::Instant::now() < deadline, "{addr} not ready");
            tokio::task::yield_now().await;
        }
    }

    /// Mock telescope: for each request it sends two events, then the response.
    async fn start_scripted_telescope() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (r, mut w) = stream.into_split();
            let mut r = BufReader::new(r);
            let mut line = String::new();
            while r.read_line(&mut line).await.unwrap_or(0) > 0 {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                    let resp = serde_json::json!({"id": v["id"], "code": 0, "result": null});
                    let out = format!("{EVENT_PI}\r\n{EVENT_STACK}\r\n{resp}\r\n");
                    let _ = w.write_all(out.as_bytes()).await;
                    let _ = w.flush().await;
                }
                line.clear();
            }
        });
        addr
    }

    async fn read_lines(
        reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
        n: usize,
    ) -> Vec<String> {
        let mut out = Vec::new();
        for _ in 0..n {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
                .await
                .expect("timed out waiting for line")
                .expect("read error");
            out.push(line.trim().to_string());
        }
        out
    }

    /// Record one live exchange; returns the session dir guard and live snapshot.
    async fn record_live() -> (tempfile::TempDir, TelescopeStatusSnapshot) {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Arc::new(Recorder::new(dir.path()).await.unwrap());
        let metrics = Metrics::new();
        let telescope = start_scripted_telescope().await;
        let proxy = free_addr();
        tokio::spawn(control::run(
            proxy,
            Some(telescope),
            false,
            Some(recorder.clone()),
            Some(metrics.clone()),
            None,
            None,
        ));
        wait_for_tcp(proxy).await;

        let stream = TcpStream::connect(proxy).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut r = BufReader::new(r);
        w.write_all(b"{\"id\":1,\"method\":\"get_device_state\"}\r\n")
            .await
            .unwrap();
        // Two broadcast events plus the routed response.
        let live = read_lines(&mut r, 3).await;
        assert!(live.iter().any(|l| l.contains("PiStatus")));
        recorder.finalize().await;
        (dir, metrics.telescope_status())
    }

    #[tokio::test]
    async fn record_then_replay_session_keeps_telescope_messages_in_order() {
        let (dir, _) = record_live().await;
        let session = ReplaySession::load(dir.path()).unwrap();
        let raws: Vec<&str> = session
            .control_messages
            .iter()
            .map(|e| e.raw.as_str())
            .collect();
        assert_eq!(raws.len(), 3);
        assert_eq!(raws[0], EVENT_PI);
        assert_eq!(raws[1], EVENT_STACK);
        assert!(raws[2].contains("\"code\":0"));
    }

    /// Pins CURRENT behaviour, which diverges from the ideal "replays identically":
    /// (1) replay broadcasts at spawn, so a client that connects afterwards misses
    ///     messages recorded with ~0 inter-message delta;
    /// (2) `replay_control` never calls `Metrics::update_event`, so the telescope
    ///     status snapshot stays at its default instead of matching the live run.
    /// Event counters do match. Flip the two divergence asserts when fixed.
    #[tokio::test]
    async fn replay_of_recorded_session_pins_late_client_and_state_divergence() {
        let (dir, live_state) = record_live().await;
        assert_eq!(live_state.battery, Some(87), "live run must populate state");
        let session = Arc::new(ReplaySession::load(dir.path()).unwrap());
        let total = session.control_messages.len();

        let metrics = Metrics::new();
        let proxy = free_addr();
        tokio::spawn(control::run(
            proxy,
            None,
            false,
            None,
            Some(metrics.clone()),
            None,
            Some(session),
        ));
        // Wait (no sleeps) until the whole recording has been replayed.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while metrics.control_rx.load(Ordering::Relaxed) < total as u64 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "replay never finished"
            );
            tokio::task::yield_now().await;
        }
        assert_eq!(metrics.control_events.load(Ordering::Relaxed), 2);

        let stream = TcpStream::connect(proxy).await.unwrap();
        let (r, _w) = stream.into_split();
        let mut r = BufReader::new(r);
        let mut line = String::new();
        let late = tokio::time::timeout(Duration::from_millis(200), r.read_line(&mut line)).await;
        assert!(late.is_err(), "late client unexpectedly received {line:?}");

        assert_eq!(metrics.telescope_status().battery, None);
        assert!(!metrics.telescope_status().is_stacking);
    }
}
