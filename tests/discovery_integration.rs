/// Integration tests for the discovery bridge (`discovery::run`).
///
/// The bridge hardcodes UDP port 4720, so these tests use two separate loopback
/// addresses:
///   - 127.0.0.1 → mock telescope (listens on :4720 to answer probe)
///   - 127.0.0.2 → discovery proxy (binds :4720 to serve clients)
///
/// Both addresses are valid loopback on Linux and macOS. Tests that cannot
/// bind the required ports are skipped rather than failing.
use std::net::IpAddr;
use std::time::Duration;
use tokio::net::UdpSocket;

const DISCOVERY_PORT: u16 = 4720;

/// Attempt to bind a UDP socket; return None and print a skip message if the
/// port is already in use (common in shared CI environments).
async fn try_bind_udp(addr: &str) -> Option<UdpSocket> {
    match UdpSocket::bind(addr).await {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("SKIP: could not bind {addr}: {e}");
            None
        }
    }
}

/// Build a minimal device-info JSON that a real Seestar would send in
/// response to a scan_iscope probe.
fn telescope_device_info(ip: &str) -> serde_json::Value {
    serde_json::json!({
        "id": 201,
        "result": {
            "product_model": "Seestar S50",
            "sn": "TEST001",
            "ip": ip,
            "tcp_client_num": 0
        }
    })
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Spawn a mock telescope UDP server on 127.0.0.1:4720.
/// It responds once to the first scan_iscope probe it receives, then idles.
/// Returns None if the port is unavailable.
async fn start_mock_telescope(response_ip: &'static str) -> Option<()> {
    let sock = try_bind_udp(&format!("127.0.0.1:{DISCOVERY_PORT}")).await?;

    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        let Ok((n, src)) = sock.recv_from(&mut buf).await else {
            return;
        };

        // Respond only to scan_iscope probes.
        if let Ok(req) = serde_json::from_slice::<serde_json::Value>(&buf[..n]) {
            if req.get("method").and_then(|v| v.as_str()) == Some("scan_iscope") {
                let resp = telescope_device_info(response_ip);
                let _ = sock.send_to(&serde_json::to_vec(&resp).unwrap(), src).await;
            }
        }
        // Keep socket alive so the proxy doesn't get a reset during startup.
        tokio::time::sleep(Duration::from_secs(10)).await;
    });

    Some(())
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Full end-to-end test of the discovery bridge.
///
/// Verifies:
///   1. A client `scan_iscope` request receives a response.
///   2. The proxy substitutes its own address as the UDP source.
///   3. Non-`scan_iscope` messages are silently ignored.
///   4. Configured device-info fields are forwarded correctly.
///
/// Uses a pre-configured identity (`telescope_sn`) to skip the upstream probe
/// so the test is self-contained and fast.  The upstream probe path is covered
/// separately by the `probe_upstream_skips_malformed_then_succeeds` unit test.
///
/// Requires 127.0.0.2 to be a usable loopback address.  On macOS this alias
/// is not present by default; the test skips automatically when unavailable.
#[tokio::test]
async fn discovery_bridge_end_to_end() {
    // Check that 127.0.0.2 is a usable loopback address before proceeding.
    // On macOS this alias is not present by default, so we skip rather than fail.
    if tokio::net::UdpSocket::bind("127.0.0.2:0").await.is_err() {
        eprintln!(
            "SKIP discovery_bridge_end_to_end: 127.0.0.2 is not available. \
             On macOS run: sudo ifconfig lo0 alias 127.0.0.2"
        );
        return;
    }

    // Start the discovery bridge on 127.0.0.2:4720 with a pre-configured
    // identity so it binds immediately without probing upstream.
    let bind_addr: IpAddr = "127.0.0.2".parse().unwrap();
    let upstream_addr: IpAddr = "127.0.0.1".parse().unwrap();
    tokio::spawn(seestar_proxy::discovery::run(
        bind_addr,
        upstream_addr,
        4700,
        Some("TEST001".to_string()),
        Some("Seestar S50".to_string()),
        None,
    ));

    // Wait for the bridge to complete its startup probe and bind its port.
    // probe_upstream has a 5 s internal timeout; with a live mock it completes
    // in milliseconds, so 500 ms is ample.
    let proxy_sock_addr = format!("127.0.0.2:{DISCOVERY_PORT}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        // Attempt a dummy send to see whether the proxy port is open.
        let probe = serde_json::to_vec(&serde_json::json!({
            "id": 201, "method": "scan_iscope", "name": "ping", "ip": "0.0.0.0"
        }))
        .unwrap();
        let tmp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let _ = tmp.send_to(&probe, &proxy_sock_addr as &str).await;

        let mut buf = [0u8; 4096];
        let ready = tokio::time::timeout(Duration::from_millis(50), tmp.recv_from(&mut buf))
            .await
            .is_ok();
        if ready {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "discovery bridge never became ready"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // ── Test 1: address substitution ──────────────────────────────────────────
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let scan_req = serde_json::to_vec(&serde_json::json!({
        "id": 201,
        "method": "scan_iscope",
        "name": "test_client",
        "ip": "0.0.0.0"
    }))
    .unwrap();
    client
        .send_to(&scan_req, &proxy_sock_addr as &str)
        .await
        .unwrap();

    let mut buf = [0u8; 4096];
    let (n, src) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
        .await
        .expect("timed out waiting for discovery response")
        .unwrap();

    let response: serde_json::Value = serde_json::from_slice(&buf[..n]).unwrap();

    // The response must come from the proxy, not from the telescope directly.
    // The app uses the UDP source IP to determine where to connect, so the
    // payload "ip" field is not modified — only the source address matters.
    assert_eq!(src.ip().to_string(), "127.0.0.2");

    // ── Test 2: non-scan_iscope messages are ignored ───────────────────────────
    // Send a request with a different method; there must be no response within
    // a short timeout.
    let other_req = serde_json::to_vec(&serde_json::json!({
        "id": 1, "method": "get_device_state"
    }))
    .unwrap();
    client
        .send_to(&other_req, &proxy_sock_addr as &str)
        .await
        .unwrap();

    let silence =
        tokio::time::timeout(Duration::from_millis(200), client.recv_from(&mut buf)).await;
    assert!(
        silence.is_err(),
        "bridge must not respond to non-scan_iscope requests"
    );

    // ── Test 3: device info fields forwarded ─────────────────────────────────
    // The other fields from the telescope's device info should be present.
    assert_eq!(response["result"]["product_model"], "Seestar S50");
    assert_eq!(response["result"]["sn"], "TEST001");
}

/// When the upstream probe and its TCP fallback both fail (nothing listening),
/// `run()` must not error: it serves the minimal "proxy" fallback identity.
/// Pins which fields are present and absent versus a successful probe or a
/// configured identity.
///
/// Real time: `probe_upstream` has a hard-coded 5 s UDP timeout, so this test
/// takes ~5 s. Loopback only (127.0.0.3 proxy, 127.0.0.4 silent upstream).
#[tokio::test]
async fn discovery_probe_timeout_serves_minimal_fallback_response() {
    if tokio::net::UdpSocket::bind("127.0.0.3:0").await.is_err()
        || tokio::net::UdpSocket::bind("127.0.0.4:0").await.is_err()
    {
        eprintln!("SKIP: 127.0.0.3/127.0.0.4 not available");
        return;
    }

    let bind_addr: IpAddr = "127.0.0.3".parse().unwrap();
    let upstream_addr: IpAddr = "127.0.0.4".parse().unwrap(); // nothing on :4720

    let handle = tokio::spawn(seestar_proxy::discovery::run(
        bind_addr,
        upstream_addr,
        4700,
        None,
        None,
        None,
    ));

    // Poll until the bridge answers (after the 5 s probe timeout), bounded at 15 s.
    let proxy_sock_addr = format!("127.0.0.3:{DISCOVERY_PORT}");
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let req = serde_json::to_vec(&serde_json::json!({
        "id": 201, "method": "scan_iscope", "name": "testclient", "ip": "0.0.0.0"
    }))
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut buf = [0u8; 4096];
    let (n, src) = loop {
        assert!(
            !handle.is_finished(),
            "run() must not exit when the probe times out"
        );
        client
            .send_to(&req, proxy_sock_addr.as_str())
            .await
            .unwrap();
        if let Ok(r) =
            tokio::time::timeout(Duration::from_millis(250), client.recv_from(&mut buf)).await
        {
            break r.unwrap();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no fallback discovery response within 15 s"
        );
    };

    let response: serde_json::Value = serde_json::from_slice(&buf[..n]).unwrap();
    assert_eq!(src.ip().to_string(), "127.0.0.3");

    // Envelope.
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["method"], "scan_iscope");
    assert_eq!(response["id"], 201);
    assert_eq!(response["code"], 0);
    assert_eq!(response["Timestamp"], "0");

    // Present: minimal placeholder identity, with ip patched to the proxy.
    let result = response["result"].as_object().expect("result object");
    assert_eq!(result["product_model"], "Seestar (via proxy)");
    assert_eq!(result["sn"], "proxy");
    assert_eq!(result["ssid"], "Seestar_proxy");
    assert_eq!(result["is_verified"], true);
    assert_eq!(result["tcp_client_num"], 0);
    assert_eq!(result["ip"], "127.0.0.3");

    // Absent: fields only a real/configured identity carries.
    for key in ["bssid", "serc", "can_star_mode_sel_cam"] {
        assert!(!result.contains_key(key), "fallback must not include {key}");
    }
    assert_eq!(result.len(), 6, "unexpected extra fields: {result:?}");

    handle.abort();
}

/// When `telescope_sn` is provided, `run()` must skip the upstream probe and
/// immediately serve the configured identity to clients.
///
/// Uses 127.0.0.5:4720 to avoid conflicts with other tests.
#[tokio::test]
async fn discovery_configured_identity_skips_probe_and_serves_correct_response() {
    if tokio::net::UdpSocket::bind("127.0.0.5:0").await.is_err() {
        eprintln!("SKIP: 127.0.0.5 not available");
        return;
    }

    let bind_addr: IpAddr = "127.0.0.5".parse().unwrap();
    // Point upstream at an address with nothing listening — if the probe
    // fires it would time out, making the test slow.  With telescope_sn set
    // the probe must be skipped entirely.
    let upstream_addr: IpAddr = "127.0.0.6".parse().unwrap();

    tokio::spawn(seestar_proxy::discovery::run(
        bind_addr,
        upstream_addr,
        4700,
        Some("TESTSN01".to_string()),
        Some("Seestar S50".to_string()),
        None,
    ));

    // With configured identity there is no probe delay — the bridge starts
    // immediately.  Give it 200 ms to bind and be ready.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let req = serde_json::to_vec(&serde_json::json!({
        "id": 201, "method": "scan_iscope", "name": "testclient", "ip": "0.0.0.0"
    }))
    .unwrap();
    client
        .send_to(&req, format!("127.0.0.5:{DISCOVERY_PORT}").as_str())
        .await
        .unwrap();

    let mut buf = [0u8; 4096];
    let (n, src) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
        .await
        .expect("timed out waiting for configured-identity discovery response")
        .unwrap();

    let response: serde_json::Value = serde_json::from_slice(&buf[..n]).unwrap();

    // Response must come from the proxy's bind address.
    assert_eq!(src.ip().to_string(), "127.0.0.5");
    // Configured SN and derived SSID must be present.
    assert_eq!(response["result"]["sn"], "TESTSN01");
    assert_eq!(response["result"]["ssid"], "S50_TESTSN01");
    assert_eq!(response["result"]["product_model"], "Seestar S50");
    // IP must be substituted to the proxy's bind address.
    assert_eq!(response["result"]["ip"], "127.0.0.5");
}

/// Verify that `discovery::run` with an unspecified bind address (0.0.0.0)
/// does NOT insert an "ip" field into the substituted response, leaving
/// clients to rely on the UDP source address instead.
///
/// This test can only run if no other process holds port 4720 on the wildcard
/// interface, so it is marked `#[ignore]` to avoid CI flakiness.  Run it
/// explicitly with `cargo test -- --ignored` when port 4720 is free.
#[tokio::test]
#[ignore = "requires exclusive use of 0.0.0.0:4720"]
async fn discovery_unspecified_bind_omits_ip_substitution() {
    // Mock telescope on 127.0.0.1:4720 responding to the startup probe.
    let Some(()) = start_mock_telescope("127.0.0.1").await else {
        return;
    };
    tokio::time::sleep(Duration::from_millis(20)).await;

    let bind_addr: IpAddr = "0.0.0.0".parse().unwrap();
    let upstream_addr: IpAddr = "127.0.0.1".parse().unwrap();
    tokio::spawn(seestar_proxy::discovery::run(
        bind_addr,
        upstream_addr,
        4700,
        None,
        None,
        None,
    ));

    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let req = serde_json::to_vec(&serde_json::json!({
        "id": 201, "method": "scan_iscope", "name": "test", "ip": "0.0.0.0"
    }))
    .unwrap();
    client.send_to(&req, "127.0.0.1:4720").await.unwrap();

    let mut buf = [0u8; 4096];
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
        .await
        .expect("timed out")
        .unwrap();

    let response: serde_json::Value = serde_json::from_slice(&buf[..n]).unwrap();
    // With 0.0.0.0 bind, the proxy must NOT overwrite the ip field.
    assert_eq!(
        response["result"]["ip"], "127.0.0.1",
        "ip field should remain the telescope's original address"
    );
}
