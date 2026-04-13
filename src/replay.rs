//! Replay mode — plays back a recorded session as if it were a live telescope.
//!
//! Loads a session directory (produced by `--record`) and feeds the recorded
//! telescope messages and image frames into the same channels that the real
//! upstream tasks would, so clients see an exact replay of the original session.

use crate::metrics::Metrics;
use crate::protocol::{self, FrameHeader, HEADER_SIZE};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tracing::{info, warn};

/// A recorded control message from the telescope.
#[derive(Debug)]
pub struct ControlEntry {
    pub timestamp: f64,
    pub raw: String,
}

/// A loaded recording session ready for replay.
#[derive(Debug)]
pub struct ReplaySession {
    pub control_messages: Vec<ControlEntry>,
    pub frame_paths: Vec<PathBuf>,
    pub frame_interval: Duration,
}

#[derive(Deserialize)]
struct RecordedManifest {
    #[serde(default)]
    frame_count: u32,
    #[serde(default)]
    duration_seconds: f64,
}

#[derive(Deserialize)]
struct RecordedLine {
    timestamp: f64,
    direction: String,
    raw: String,
}

impl ReplaySession {
    /// Load a recorded session directory.
    ///
    /// Reads `manifest.json` for timing metadata, `control.jsonl` for telescope
    /// messages (client-direction lines are filtered out), and discovers frame
    /// files in `frames/`.
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let manifest_path = dir.join("manifest.json");
        let manifest_text = std::fs::read_to_string(&manifest_path).map_err(|e| {
            anyhow::anyhow!(
                "Failed to read {}: {} — is this a valid recording directory?",
                manifest_path.display(),
                e
            )
        })?;
        let manifest: RecordedManifest = serde_json::from_str(&manifest_text)?;

        // Load control messages, keeping only telescope-direction lines.
        let control_path = dir.join("control.jsonl");
        let mut control_messages = Vec::new();
        if let Ok(text) = std::fs::read_to_string(&control_path) {
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<RecordedLine>(trimmed) {
                    Ok(entry) if entry.direction == "telescope" => {
                        control_messages.push(ControlEntry {
                            timestamp: entry.timestamp,
                            raw: entry.raw,
                        });
                    }
                    Ok(_) => {} // client-direction, skip
                    Err(e) => {
                        warn!("Replay: skipping corrupt control.jsonl line: {}", e);
                    }
                }
            }
        }

        // Discover frame files.
        let frames_dir = dir.join("frames");
        let mut frame_paths = Vec::new();
        if frames_dir.is_dir() {
            for entry in std::fs::read_dir(&frames_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("frame_") && n.ends_with(".bin"))
                {
                    frame_paths.push(path);
                }
            }
            frame_paths.sort();
        }

        // Compute uniform frame interval from manifest.
        let frame_interval = if manifest.frame_count > 0 && manifest.duration_seconds > 0.0 {
            Duration::from_secs_f64(manifest.duration_seconds / manifest.frame_count as f64)
        } else {
            Duration::from_secs(2) // fallback: typical Seestar preview cadence
        };

        info!(
            "Replay: loaded {} control messages, {} frames from {}",
            control_messages.len(),
            frame_paths.len(),
            dir.display()
        );

        Ok(Self {
            control_messages,
            frame_paths,
            frame_interval,
        })
    }
}

/// Replay recorded control messages into the event broadcast channel.
///
/// Plays back all telescope-direction messages in recorded order with the
/// original timing. All messages are broadcast via `event_tx` so every
/// connected client receives them.
pub async fn replay_control(
    session: &ReplaySession,
    event_tx: &broadcast::Sender<String>,
    handshake_done: &AtomicBool,
    metrics: Option<&Metrics>,
) {
    let mut prev_ts: Option<f64> = None;

    for entry in &session.control_messages {
        // Sleep for the delta between consecutive timestamps.
        if let Some(prev) = prev_ts {
            let delta = (entry.timestamp - prev).max(0.0);
            if delta > 0.0 {
                tokio::time::sleep(Duration::from_secs_f64(delta)).await;
            }
        }
        prev_ts = Some(entry.timestamp);

        // Signal handshake completion on first message.
        if !handshake_done.load(Ordering::Relaxed) {
            handshake_done.store(true, Ordering::Relaxed);
        }

        // Update metrics.
        if let Some(m) = metrics {
            m.control_rx.fetch_add(1, Ordering::Relaxed);
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&entry.raw) {
                if protocol::is_event(&v) {
                    m.control_events.fetch_add(1, Ordering::Relaxed);
                }
                let method = protocol::method_name(&v).unwrap_or("?");
                m.push_log_with_payload(
                    "ctrl-rx",
                    method.to_string(),
                    Some(entry.raw.clone()),
                );
            }
        }

        let _ = event_tx.send(entry.raw.clone());
    }

    info!(
        "Replay: control playback complete ({} messages)",
        session.control_messages.len()
    );
}

/// Replay recorded image frames into the frame broadcast channel.
///
/// Reads each frame file (80-byte header + payload) and broadcasts it with
/// uniform spacing derived from the recording's duration and frame count.
pub async fn replay_imaging(
    session: &ReplaySession,
    frame_tx: &broadcast::Sender<Arc<Vec<u8>>>,
    metrics: Option<&Metrics>,
) {
    for path in &session.frame_paths {
        tokio::time::sleep(session.frame_interval).await;

        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                warn!("Replay: skipping unreadable frame {}: {}", path.display(), e);
                continue;
            }
        };

        if data.len() < HEADER_SIZE {
            warn!(
                "Replay: skipping truncated frame {} ({} bytes)",
                path.display(),
                data.len()
            );
            continue;
        }

        // Update metrics.
        if let Some(m) = metrics {
            let header: [u8; HEADER_SIZE] = data[..HEADER_SIZE].try_into().unwrap();
            let parsed = FrameHeader::parse(&header);
            m.imaging_frames.fetch_add(1, Ordering::Relaxed);
            m.imaging_bytes
                .fetch_add(data.len() as u64, Ordering::Relaxed);
            let kind = match parsed.id {
                23 => "stack",
                21 => "preview",
                20 => "view",
                _ => "frame",
            };
            m.push_log(
                "img-rx",
                format!("{} {}x{}", kind, parsed.width, parsed.height),
            );
        }

        let _ = frame_tx.send(Arc::new(data));
    }

    info!(
        "Replay: imaging playback complete ({} frames)",
        session.frame_paths.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    fn unique_test_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("seestar_replay_test_{pid}_{id}"))
    }

    /// Create a minimal valid session directory for testing.
    fn create_test_session(
        dir: &Path,
        control_lines: &[(&str, &str)], // (direction, raw)
        frame_count: usize,
    ) {
        std::fs::create_dir_all(dir.join("frames")).unwrap();

        // Write manifest.
        let manifest = serde_json::json!({
            "capture_date": "2026-01-01T00:00:00Z",
            "frame_count": frame_count,
            "preview_frame_count": frame_count,
            "stack_frame_count": 0,
            "control_message_count": control_lines.len(),
            "duration_seconds": frame_count as f64 * 2.0,
            "source": "test"
        });
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        // Write control.jsonl.
        let mut jsonl = String::new();
        for (i, (direction, raw)) in control_lines.iter().enumerate() {
            let line = serde_json::json!({
                "timestamp": 1000.0 + i as f64 * 0.5,
                "direction": direction,
                "raw": raw,
            });
            jsonl.push_str(&serde_json::to_string(&line).unwrap());
            jsonl.push('\n');
        }
        std::fs::write(dir.join("control.jsonl"), jsonl).unwrap();

        // Write frame files.
        for i in 0..frame_count {
            let mut data = vec![0u8; HEADER_SIZE + 2000];
            // Set up a valid image header: size, id=21 (preview), dimensions.
            data[6..10].copy_from_slice(&2000u32.to_be_bytes());
            data[15] = 21; // preview
            data[16..18].copy_from_slice(&100u16.to_be_bytes());
            data[18..20].copy_from_slice(&100u16.to_be_bytes());
            let path = dir.join(format!("frames/frame_{i:04}_preview.bin"));
            std::fs::write(path, &data).unwrap();
        }
    }

    #[test]
    fn load_valid_session() {
        let dir = unique_test_dir();
        create_test_session(
            &dir,
            &[
                ("telescope", r#"{"Event":"PiStatus"}"#),
                ("client", r#"{"id":1,"method":"test"}"#),
                ("telescope", r#"{"id":1,"code":0}"#),
            ],
            2,
        );

        let session = ReplaySession::load(&dir).unwrap();
        assert_eq!(session.control_messages.len(), 2); // telescope only
        assert_eq!(session.frame_paths.len(), 2);
    }

    #[test]
    fn load_filters_telescope_only() {
        let dir = unique_test_dir();
        create_test_session(
            &dir,
            &[
                ("client", r#"{"id":1,"method":"get_device_state"}"#),
                ("telescope", r#"{"id":1,"code":0,"result":{}}"#),
                ("client", r#"{"id":2,"method":"scope_park"}"#),
                ("telescope", r#"{"id":2,"code":0}"#),
            ],
            0,
        );

        let session = ReplaySession::load(&dir).unwrap();
        assert_eq!(session.control_messages.len(), 2);
        assert!(session.control_messages[0].raw.contains("result"));
        assert!(!session.control_messages[1].raw.contains("scope_park"));
    }

    #[test]
    fn load_sorts_frame_paths() {
        let dir = unique_test_dir();
        create_test_session(&dir, &[], 3);

        let session = ReplaySession::load(&dir).unwrap();
        let names: Vec<_> = session
            .frame_paths
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "frame_0000_preview.bin",
                "frame_0001_preview.bin",
                "frame_0002_preview.bin",
            ]
        );
    }

    #[test]
    fn load_missing_manifest_errors() {
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        // No manifest.json

        let result = ReplaySession::load(&dir);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("manifest.json"),
            "Error should mention manifest.json: {err}"
        );
    }

    #[test]
    fn load_empty_recording() {
        let dir = unique_test_dir();
        create_test_session(&dir, &[], 0);

        let session = ReplaySession::load(&dir).unwrap();
        assert!(session.control_messages.is_empty());
        assert!(session.frame_paths.is_empty());
        // Fallback interval when frame_count is 0.
        assert_eq!(session.frame_interval, Duration::from_secs(2));
    }

    #[test]
    fn load_skips_corrupt_jsonl_lines() {
        let dir = unique_test_dir();
        create_test_session(&dir, &[], 0);

        // Overwrite control.jsonl with a mix of valid and corrupt lines.
        let jsonl = format!(
            "{}\nNOT VALID JSON\n{}\n",
            serde_json::json!({"timestamp": 1.0, "direction": "telescope", "raw": "msg1"}),
            serde_json::json!({"timestamp": 2.0, "direction": "telescope", "raw": "msg2"}),
        );
        std::fs::write(dir.join("control.jsonl"), jsonl).unwrap();

        let session = ReplaySession::load(&dir).unwrap();
        assert_eq!(session.control_messages.len(), 2);
    }

    #[test]
    fn load_frame_interval_from_manifest() {
        let dir = unique_test_dir();
        create_test_session(&dir, &[], 4);

        let session = ReplaySession::load(&dir).unwrap();
        // 4 frames, duration = 4 * 2.0 = 8.0s → interval = 2.0s
        assert_eq!(session.frame_interval, Duration::from_secs(2));
    }

    #[tokio::test]
    async fn replay_control_broadcasts_all_messages() {
        let dir = unique_test_dir();
        create_test_session(
            &dir,
            &[
                ("telescope", r#"{"Event":"PiStatus","temp":35}"#),
                ("client", r#"{"id":1,"method":"test"}"#),
                ("telescope", r#"{"id":1,"code":0}"#),
                ("telescope", r#"{"Event":"Alert","msg":"done"}"#),
            ],
            0,
        );

        let session = ReplaySession::load(&dir).unwrap();
        let (tx, mut rx) = broadcast::channel::<String>(16);
        let done = AtomicBool::new(false);

        replay_control(&session, &tx, &done, None).await;

        // Should have received 3 telescope messages.
        let mut received = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            received.push(msg);
        }
        assert_eq!(received.len(), 3);
        assert!(received[0].contains("PiStatus"));
        assert!(received[1].contains("code"));
        assert!(received[2].contains("Alert"));
    }

    #[tokio::test]
    async fn replay_control_sets_handshake_done() {
        let dir = unique_test_dir();
        create_test_session(&dir, &[("telescope", r#"{"id":1,"code":0}"#)], 0);

        let session = ReplaySession::load(&dir).unwrap();
        let (tx, _rx) = broadcast::channel::<String>(16);
        let done = AtomicBool::new(false);

        assert!(!done.load(Ordering::Relaxed));
        replay_control(&session, &tx, &done, None).await;
        assert!(done.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn replay_imaging_sends_frames_in_order() {
        let dir = unique_test_dir();
        create_test_session(&dir, &[], 3);

        // Override frame_interval to 0 for fast tests.
        let mut session = ReplaySession::load(&dir).unwrap();
        session.frame_interval = Duration::ZERO;

        let (tx, mut rx) = broadcast::channel::<Arc<Vec<u8>>>(16);

        replay_imaging(&session, &tx, None).await;

        let mut received = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            received.push(frame);
        }
        assert_eq!(received.len(), 3);
        // Each frame should be HEADER_SIZE + 2000 bytes.
        for frame in &received {
            assert_eq!(frame.len(), HEADER_SIZE + 2000);
        }
    }
}
