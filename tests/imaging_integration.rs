/// Integration tests for the imaging proxy (`imaging::run`).
///
/// Each test spins up:
///   1. A mock telescope TCP server
///   2. The imaging proxy via `imaging::run()`
///   3. One or more test clients that read the broadcast frames
///
/// The telescope sends frames only after a trigger signal is sent by the test,
/// which guarantees all clients are subscribed to the broadcast channel before
/// any frames arrive.
use seestar_proxy::protocol::HEADER_SIZE;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

// ── helpers ───────────────────────────────────────────────────────────────────

fn free_addr() -> SocketAddr {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap()
}

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

fn make_frame(payload_size: u32, id: u8, width: u16, height: u16, payload_byte: u8) -> Vec<u8> {
    let mut header = [0u8; HEADER_SIZE];
    header[6..10].copy_from_slice(&payload_size.to_be_bytes());
    header[15] = id;
    header[16..18].copy_from_slice(&width.to_be_bytes());
    header[18..20].copy_from_slice(&height.to_be_bytes());

    let mut frame = header.to_vec();
    frame.extend(vec![payload_byte; payload_size as usize]);
    frame
}

/// Spawn a mock telescope that waits for a trigger before sending frames.
///
/// Returns `(telescope_addr, trigger_tx)`. Call `trigger_tx.send(frames)`
/// once all proxy clients are connected and subscribed — the telescope will
/// then send all frames in order.
async fn start_triggered_telescope() -> (SocketAddr, oneshot::Sender<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<Vec<Vec<u8>>>();

    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else { return };
        // Wait until the test signals which frames to send.
        if let Ok(frames) = rx.await {
            for frame in frames {
                if stream.write_all(&frame).await.is_err() {
                    break;
                }
            }
            let _ = stream.flush().await;
        }
        // Keep alive so the proxy doesn't see EOF prematurely.
        tokio::time::sleep(Duration::from_secs(10)).await;
    });

    (addr, tx)
}

async fn start_proxy(telescope_addr: SocketAddr) -> SocketAddr {
    let proxy_addr = free_addr();
    tokio::spawn(seestar_proxy::imaging::run(proxy_addr, telescope_addr, None, None));
    wait_for_tcp(proxy_addr, Duration::from_secs(2)).await;
    proxy_addr
}

/// Read exactly `len` bytes from `stream` with a timeout.
async fn read_exact_timeout(stream: &mut TcpStream, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut buf))
        .await
        .expect("timed out reading frame")
        .expect("read error");
    buf
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn single_client_receives_frame() {
    let (telescope_addr, trigger) = start_triggered_telescope().await;
    let proxy_addr = start_proxy(telescope_addr).await;

    let frame = make_frame(8, 21, 640, 480, 0xAB);
    let total_len = frame.len();

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    // Pause briefly so the client's receive task is registered in the proxy.
    tokio::time::sleep(Duration::from_millis(20)).await;

    trigger.send(vec![frame.clone()]).unwrap();

    let received = read_exact_timeout(&mut client, total_len).await;
    assert_eq!(received, frame);
}

#[tokio::test]
async fn multiple_clients_all_receive_same_frame() {
    let (telescope_addr, trigger) = start_triggered_telescope().await;
    let proxy_addr = start_proxy(telescope_addr).await;

    let frame = make_frame(16, 21, 100, 100, 0xCC);
    let total_len = frame.len();

    let mut c1 = TcpStream::connect(proxy_addr).await.unwrap();
    let mut c2 = TcpStream::connect(proxy_addr).await.unwrap();
    let mut c3 = TcpStream::connect(proxy_addr).await.unwrap();

    // Wait for all three client handler tasks to subscribe to the broadcast.
    tokio::time::sleep(Duration::from_millis(20)).await;

    trigger.send(vec![frame.clone()]).unwrap();

    let (r1, r2, r3) = tokio::join!(
        read_exact_timeout(&mut c1, total_len),
        read_exact_timeout(&mut c2, total_len),
        read_exact_timeout(&mut c3, total_len),
    );

    assert_eq!(r1, frame);
    assert_eq!(r2, frame);
    assert_eq!(r3, frame);
}

#[tokio::test]
async fn frame_header_bytes_are_preserved() {
    let payload_size = 4u32;
    let id = 23u8; // stacked image
    let width = 1920u16;
    let height = 1080u16;

    let (telescope_addr, trigger) = start_triggered_telescope().await;
    let proxy_addr = start_proxy(telescope_addr).await;

    let frame = make_frame(payload_size, id, width, height, 0xFF);
    let total_len = frame.len();

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    trigger.send(vec![frame.clone()]).unwrap();

    let received = read_exact_timeout(&mut client, total_len).await;

    let recv_size = u32::from_be_bytes(received[6..10].try_into().unwrap());
    let recv_id = received[15];
    let recv_width = u16::from_be_bytes(received[16..18].try_into().unwrap());
    let recv_height = u16::from_be_bytes(received[18..20].try_into().unwrap());

    assert_eq!(recv_size, payload_size);
    assert_eq!(recv_id, id);
    assert_eq!(recv_width, width);
    assert_eq!(recv_height, height);
    assert!(received[HEADER_SIZE..].iter().all(|&b| b == 0xFF));
}

#[tokio::test]
async fn multiple_sequential_frames_arrive_in_order() {
    let (telescope_addr, trigger) = start_triggered_telescope().await;
    let proxy_addr = start_proxy(telescope_addr).await;

    let frames: Vec<Vec<u8>> = (0u8..4).map(|i| make_frame(4, 21, 10, 10, i)).collect();
    let total_len = HEADER_SIZE + 4;

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    trigger.send(frames).unwrap();

    for expected_byte in 0u8..4 {
        let received = read_exact_timeout(&mut client, total_len).await;
        assert!(
            received[HEADER_SIZE..].iter().all(|&b| b == expected_byte),
            "frame {expected_byte} had wrong payload"
        );
    }
}

#[tokio::test]
async fn proxy_records_image_frames() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let record_dir = std::env::temp_dir().join(format!("seestar_img_rec_{pid}_{id}"));

    let recorder = Arc::new(
        seestar_proxy::recorder::Recorder::new(&record_dir)
            .await
            .unwrap(),
    );

    let (telescope_addr, trigger) = start_triggered_telescope().await;
    let proxy_addr = free_addr();
    tokio::spawn(seestar_proxy::imaging::run(
        proxy_addr,
        telescope_addr,
        Some(recorder.clone()),
        None,
    ));
    wait_for_tcp(proxy_addr, Duration::from_secs(2)).await;

    // Image frame: id=21 (preview), width/height > 0, size > 1000
    let frame = make_frame(2000, 21, 100, 100, 0x42);
    let total_len = frame.len();

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    trigger.send(vec![frame]).unwrap();

    let _ = read_exact_timeout(&mut client, total_len).await;

    // Give the async recorder time to flush.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let frames_dir = record_dir.join("frames");
    let mut entries: Vec<_> = std::fs::read_dir(&frames_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    assert_eq!(entries.len(), 1, "expected one recorded frame file");
    assert!(
        entries[0]
            .file_name()
            .to_string_lossy()
            .contains("preview"),
        "expected a preview frame file"
    );
}
