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
