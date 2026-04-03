//! Traffic recorder — writes session files compatible with ESC's mock telescope.
//!
//! Session directory layout:
//! ```text
//! session_dir/
//!   manifest.json
//!   control.jsonl
//!   frames/
//!     frame_0000_preview.bin
//!     frame_0001_stack.bin
//! ```

use crate::protocol::{FrameHeader, HEADER_SIZE};
use chrono::Utc;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tracing::{error, info};

#[derive(Serialize)]
struct Manifest {
    capture_date: String,
    telescope_model: Option<String>,
    firmware_version: Option<String>,
    frame_count: u32,
    preview_frame_count: u32,
    stack_frame_count: u32,
    control_message_count: u32,
    duration_seconds: f64,
    source: String,
}

#[derive(Serialize)]
struct ControlLine {
    timestamp: f64,
    direction: &'static str,
    raw: String,
}

pub struct Recorder {
    dir: PathBuf,
    control_file: Mutex<File>,
    frame_counter: AtomicU32,
    preview_count: AtomicU32,
    stack_count: AtomicU32,
    message_count: AtomicU32,
    start_time: std::time::Instant,
    start_epoch: f64,
}

impl Recorder {
    /// Create a new recorder writing to the given directory.
    pub async fn new(dir: &Path) -> std::io::Result<Self> {
        fs::create_dir_all(dir).await?;
        fs::create_dir_all(dir.join("frames")).await?;

        let control_file = File::create(dir.join("control.jsonl")).await?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        Ok(Self {
            dir: dir.to_path_buf(),
            control_file: Mutex::new(control_file),
            frame_counter: AtomicU32::new(0),
            preview_count: AtomicU32::new(0),
            stack_count: AtomicU32::new(0),
            message_count: AtomicU32::new(0),
            start_time: std::time::Instant::now(),
            start_epoch: now,
        })
    }

    fn timestamp(&self) -> f64 {
        self.start_epoch + self.start_time.elapsed().as_secs_f64()
    }

    /// Record a control message (port 4700).
    pub async fn record_control(&self, direction: &'static str, raw: &str) {
        self.message_count.fetch_add(1, Ordering::Relaxed);

        let line = ControlLine {
            timestamp: self.timestamp(),
            direction,
            raw: raw.to_string(),
        };

        if let Ok(json) = serde_json::to_string(&line) {
            let mut file = self.control_file.lock().await;
            let _ = file.write_all(json.as_bytes()).await;
            let _ = file.write_all(b"\n").await;
            let _ = file.flush().await;
        }
    }

    /// Record a binary frame (port 4800).
    pub async fn record_frame(&self, header: &[u8; HEADER_SIZE], payload: &[u8]) {
        let parsed = FrameHeader::parse(header);
        if !parsed.is_image() {
            return; // Skip handshake/control frames
        }

        let idx = self.frame_counter.fetch_add(1, Ordering::Relaxed);
        let kind = if parsed.id == 23 {
            self.stack_count.fetch_add(1, Ordering::Relaxed);
            "stack"
        } else {
            self.preview_count.fetch_add(1, Ordering::Relaxed);
            "preview"
        };

        let path = self.dir.join(format!("frames/frame_{idx:04}_{kind}.bin"));

        let mut data = Vec::with_capacity(HEADER_SIZE + payload.len());
        data.extend_from_slice(header);
        data.extend_from_slice(payload);

        if let Err(e) = fs::write(&path, &data).await {
            error!("Failed to write frame {}: {}", path.display(), e);
        }
    }

    /// Write the manifest and finalize the recording.
    pub async fn finalize(&self) {
        let manifest = Manifest {
            capture_date: Utc::now().to_rfc3339(),
            telescope_model: None,
            firmware_version: None,
            frame_count: self.frame_counter.load(Ordering::Relaxed),
            preview_frame_count: self.preview_count.load(Ordering::Relaxed),
            stack_frame_count: self.stack_count.load(Ordering::Relaxed),
            control_message_count: self.message_count.load(Ordering::Relaxed),
            duration_seconds: self.start_time.elapsed().as_secs_f64(),
            source: "seestar-proxy".to_string(),
        };

        let path = self.dir.join("manifest.json");
        match serde_json::to_string_pretty(&manifest) {
            Ok(json) => {
                if let Err(e) = fs::write(&path, json).await {
                    error!("Failed to write manifest: {}", e);
                } else {
                    info!(
                        "Recording saved: {} messages, {} frames ({:.1}s)",
                        manifest.control_message_count,
                        manifest.frame_count,
                        manifest.duration_seconds
                    );
                }
            }
            Err(e) => error!("Failed to serialize manifest: {}", e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::HEADER_SIZE;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_test_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("seestar_recorder_test_{pid}_{id}"))
    }

    /// Build an 80-byte frame header with the given fields.
    fn make_header(size: u32, id: u8, width: u16, height: u16) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        buf[6..10].copy_from_slice(&size.to_be_bytes());
        buf[15] = id;
        buf[16..18].copy_from_slice(&width.to_be_bytes());
        buf[18..20].copy_from_slice(&height.to_be_bytes());
        buf
    }

    #[tokio::test]
    async fn new_creates_session_directories() {
        let dir = unique_test_dir();
        let _rec = Recorder::new(&dir).await.unwrap();

        assert!(dir.exists());
        assert!(dir.join("frames").exists());
        assert!(dir.join("control.jsonl").exists());
    }

    #[tokio::test]
    async fn record_control_writes_jsonl_line() {
        let dir = unique_test_dir();
        let rec = Recorder::new(&dir).await.unwrap();

        rec.record_control("client", r#"{"id":1,"method":"test"}"#)
            .await;

        let content = tokio::fs::read_to_string(dir.join("control.jsonl"))
            .await
            .unwrap();

        // Each line must be a valid JSON object
        let line: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(line["direction"], "client");
        assert_eq!(line["raw"], r#"{"id":1,"method":"test"}"#);
        assert!(line["timestamp"].is_number());
    }

    #[tokio::test]
    async fn record_control_increments_message_count() {
        let dir = unique_test_dir();
        let rec = Recorder::new(&dir).await.unwrap();

        rec.record_control("client", "msg1").await;
        rec.record_control("telescope", "msg2").await;
        rec.record_control("client", "msg3").await;

        assert_eq!(rec.message_count.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn record_control_multiple_lines_are_separate_jsonl() {
        let dir = unique_test_dir();
        let rec = Recorder::new(&dir).await.unwrap();

        rec.record_control("client", r#"{"id":1}"#).await;
        rec.record_control("telescope", r#"{"id":1,"code":0}"#)
            .await;

        let content = tokio::fs::read_to_string(dir.join("control.jsonl"))
            .await
            .unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);

        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(first["direction"], "client");
        assert_eq!(second["direction"], "telescope");
    }

    #[tokio::test]
    async fn record_frame_skips_non_image_frames() {
        let dir = unique_test_dir();
        let rec = Recorder::new(&dir).await.unwrap();

        // Handshake frame: size=17, id=2, width=0, height=0 → not an image
        let header = make_header(17, 2, 0, 0);
        rec.record_frame(&header, &[0u8; 17]).await;

        assert_eq!(rec.frame_counter.load(Ordering::Relaxed), 0);

        let entries: Vec<_> = std::fs::read_dir(dir.join("frames")).unwrap().collect();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn record_frame_writes_preview_file() {
        let dir = unique_test_dir();
        let rec = Recorder::new(&dir).await.unwrap();

        let payload = vec![0xAAu8; 2000];
        // id=21 → preview
        let header = make_header(2000, 21, 100, 100);
        rec.record_frame(&header, &payload).await;

        assert_eq!(rec.frame_counter.load(Ordering::Relaxed), 1);
        assert_eq!(rec.preview_count.load(Ordering::Relaxed), 1);
        assert_eq!(rec.stack_count.load(Ordering::Relaxed), 0);

        let frame_path = dir.join("frames/frame_0000_preview.bin");
        assert!(frame_path.exists());

        let written = tokio::fs::read(&frame_path).await.unwrap();
        assert_eq!(written.len(), HEADER_SIZE + 2000);
        assert_eq!(&written[..HEADER_SIZE], &header[..]);
        assert_eq!(&written[HEADER_SIZE..], &payload[..]);
    }

    #[tokio::test]
    async fn record_frame_writes_stack_file() {
        let dir = unique_test_dir();
        let rec = Recorder::new(&dir).await.unwrap();

        let payload = vec![0xBBu8; 5000];
        // id=23 → stack
        let header = make_header(5000, 23, 200, 200);
        rec.record_frame(&header, &payload).await;

        assert_eq!(rec.stack_count.load(Ordering::Relaxed), 1);
        assert_eq!(rec.preview_count.load(Ordering::Relaxed), 0);

        let frame_path = dir.join("frames/frame_0000_stack.bin");
        assert!(frame_path.exists());
    }

    #[tokio::test]
    async fn record_frame_sequential_indices() {
        let dir = unique_test_dir();
        let rec = Recorder::new(&dir).await.unwrap();

        let payload = vec![0u8; 2000];
        let header_preview = make_header(2000, 21, 100, 100);
        let header_stack = make_header(2000, 23, 100, 100);

        rec.record_frame(&header_preview, &payload).await;
        rec.record_frame(&header_stack, &payload).await;
        rec.record_frame(&header_preview, &payload).await;

        assert!(dir.join("frames/frame_0000_preview.bin").exists());
        assert!(dir.join("frames/frame_0001_stack.bin").exists());
        assert!(dir.join("frames/frame_0002_preview.bin").exists());
        assert_eq!(rec.frame_counter.load(Ordering::Relaxed), 3);
        assert_eq!(rec.preview_count.load(Ordering::Relaxed), 2);
        assert_eq!(rec.stack_count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn finalize_writes_valid_manifest() {
        let dir = unique_test_dir();
        let rec = Recorder::new(&dir).await.unwrap();

        rec.record_control("client", r#"{"id":1}"#).await;
        rec.record_control("telescope", r#"{"id":1,"code":0}"#)
            .await;

        let payload = vec![0u8; 2000];
        rec.record_frame(&make_header(2000, 21, 100, 100), &payload)
            .await;
        rec.record_frame(&make_header(2000, 23, 100, 100), &payload)
            .await;

        rec.finalize().await;

        let manifest_path = dir.join("manifest.json");
        assert!(manifest_path.exists());

        let content = tokio::fs::read_to_string(&manifest_path).await.unwrap();
        let m: serde_json::Value = serde_json::from_str(&content).unwrap();

        assert_eq!(m["control_message_count"], 2);
        assert_eq!(m["frame_count"], 2);
        assert_eq!(m["preview_frame_count"], 1);
        assert_eq!(m["stack_frame_count"], 1);
        assert_eq!(m["source"], "seestar-proxy");
        assert!(m["capture_date"].is_string());
        assert!(m["duration_seconds"].as_f64().unwrap() >= 0.0);
    }

    #[tokio::test]
    async fn finalize_with_no_recordings_produces_zero_counts() {
        let dir = unique_test_dir();
        let rec = Recorder::new(&dir).await.unwrap();
        rec.finalize().await;

        let content = tokio::fs::read_to_string(dir.join("manifest.json"))
            .await
            .unwrap();
        let m: serde_json::Value = serde_json::from_str(&content).unwrap();

        assert_eq!(m["control_message_count"], 0);
        assert_eq!(m["frame_count"], 0);
        assert_eq!(m["preview_frame_count"], 0);
        assert_eq!(m["stack_frame_count"], 0);
    }
}
