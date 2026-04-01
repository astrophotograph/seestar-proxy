//! Shared runtime metrics updated by all proxy modules and read by the dashboard.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const LOG_CAPACITY: usize = 100;

/// A single traffic log entry shown in the dashboard.
#[derive(Clone, serde::Serialize)]
pub struct LogEntry {
    pub seq: u64,
    pub elapsed_ms: u64,
    /// Unix epoch milliseconds at the time the entry was recorded.
    pub timestamp_ms: u64,
    /// CSS channel name: "ctrl-rx", "ctrl-tx", "ctrl-evt", "img"
    pub channel: &'static str,
    pub summary: String,
    /// Raw JSON payload for display in the dashboard, if available.
    pub payload: Option<String>,
}

/// Shared metrics state, updated by control/imaging modules and read by the dashboard.
pub struct Metrics {
    /// Messages received from the telescope (control channel).
    pub control_rx: AtomicU64,
    /// Messages forwarded from clients to the telescope.
    pub control_tx: AtomicU64,
    /// Async events broadcast to all clients.
    pub control_events: AtomicU64,
    /// Total image frames received from the telescope.
    pub imaging_frames: AtomicU64,
    /// Total image bytes received from the telescope.
    pub imaging_bytes: AtomicU64,
    /// Currently connected control clients.
    pub control_clients: AtomicI32,
    /// Currently connected imaging clients.
    pub imaging_clients: AtomicI32,
    /// Whether the upstream control TCP connection is live.
    pub upstream_control_up: AtomicBool,
    /// Whether the upstream imaging TCP connection is live.
    pub upstream_imaging_up: AtomicBool,
    /// Ring buffer of recent traffic events.
    log: Mutex<VecDeque<LogEntry>>,
    /// Monotonically increasing log sequence counter.
    log_seq: AtomicU64,
    /// Proxy start time (for uptime calculation).
    pub started_at: Instant,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            control_rx: AtomicU64::new(0),
            control_tx: AtomicU64::new(0),
            control_events: AtomicU64::new(0),
            imaging_frames: AtomicU64::new(0),
            imaging_bytes: AtomicU64::new(0),
            control_clients: AtomicI32::new(0),
            imaging_clients: AtomicI32::new(0),
            upstream_control_up: AtomicBool::new(false),
            upstream_imaging_up: AtomicBool::new(false),
            log: Mutex::new(VecDeque::with_capacity(LOG_CAPACITY)),
            log_seq: AtomicU64::new(0),
            started_at: Instant::now(),
        })
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    /// Append a traffic log entry, dropping the oldest if the buffer is full.
    pub fn push_log(&self, channel: &'static str, summary: String) {
        self.push_log_with_payload(channel, summary, None);
    }

    /// Like [`push_log`] but also stores the raw JSON payload for dashboard display.
    pub fn push_log_with_payload(&self, channel: &'static str, summary: String, payload: Option<String>) {
        let seq = self.log_seq.fetch_add(1, Ordering::Relaxed);
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let entry = LogEntry { seq, elapsed_ms: self.elapsed_ms(), timestamp_ms, channel, summary, payload };
        if let Ok(mut log) = self.log.lock() {
            if log.len() >= LOG_CAPACITY {
                log.pop_front();
            }
            log.push_back(entry);
        }
    }

    /// Returns log entries with `seq >= from_seq`. If `from_seq` is `None`, returns all.
    pub fn log_since(&self, from_seq: Option<u64>) -> Vec<LogEntry> {
        self.log
            .lock()
            .map(|log| match from_seq {
                None => log.iter().cloned().collect(),
                Some(seq) => log.iter().filter(|e| e.seq >= seq).cloned().collect(),
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── construction ──────────────────────────────────────────────────────────

    #[test]
    fn new_initializes_all_counters_to_zero() {
        let m = Metrics::new();
        assert_eq!(m.control_rx.load(Ordering::Relaxed), 0);
        assert_eq!(m.control_tx.load(Ordering::Relaxed), 0);
        assert_eq!(m.control_events.load(Ordering::Relaxed), 0);
        assert_eq!(m.imaging_frames.load(Ordering::Relaxed), 0);
        assert_eq!(m.imaging_bytes.load(Ordering::Relaxed), 0);
        assert_eq!(m.control_clients.load(Ordering::Relaxed), 0);
        assert_eq!(m.imaging_clients.load(Ordering::Relaxed), 0);
        assert!(!m.upstream_control_up.load(Ordering::Relaxed));
        assert!(!m.upstream_imaging_up.load(Ordering::Relaxed));
    }

    #[test]
    fn elapsed_ms_is_small_immediately_after_construction() {
        let m = Metrics::new();
        assert!(m.elapsed_ms() < 500, "should be well under 500ms after construction");
    }

    // ── push_log ──────────────────────────────────────────────────────────────

    #[test]
    fn push_log_assigns_sequential_seqs_starting_at_zero() {
        let m = Metrics::new();
        m.push_log("ctrl-rx", "a".to_string());
        m.push_log("ctrl-tx", "b".to_string());
        m.push_log("ctrl-evt", "c".to_string());
        let entries = m.log_since(None);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[1].seq, 1);
        assert_eq!(entries[2].seq, 2);
    }

    #[test]
    fn push_log_records_channel_and_summary() {
        let m = Metrics::new();
        m.push_log("img", "preview 640x480 (45.1 KB)".to_string());
        let entries = m.log_since(None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].channel, "img");
        assert_eq!(entries[0].summary, "preview 640x480 (45.1 KB)");
    }

    #[test]
    fn push_log_sets_elapsed_ms_to_reasonable_value() {
        let m = Metrics::new();
        m.push_log("ctrl-rx", "test".to_string());
        let entries = m.log_since(None);
        // Must be non-negative and not in the distant future
        assert!(entries[0].elapsed_ms < 1_000, "elapsed_ms should be < 1s");
    }

    #[test]
    fn push_log_drops_oldest_entry_when_at_capacity() {
        let m = Metrics::new();
        for i in 0..LOG_CAPACITY {
            m.push_log("ctrl-rx", format!("msg-{}", i));
        }
        // One more entry evicts the first one (msg-0)
        m.push_log("ctrl-rx", "extra".to_string());

        let entries = m.log_since(None);
        assert_eq!(entries.len(), LOG_CAPACITY, "buffer must not grow past capacity");
        assert_eq!(entries[0].summary, "msg-1", "oldest entry must have been dropped");
        assert_eq!(entries[LOG_CAPACITY - 1].summary, "extra");
    }

    #[test]
    fn push_log_seq_is_monotonic_across_evictions() {
        let m = Metrics::new();
        // Overflow the ring buffer
        for i in 0..(LOG_CAPACITY + 10) {
            m.push_log("ctrl-rx", format!("{}", i));
        }
        let entries = m.log_since(None);
        // Seqs must still be strictly increasing even after wrapping
        for window in entries.windows(2) {
            assert!(window[1].seq > window[0].seq, "seqs must be strictly increasing");
        }
    }

    // ── log_since ─────────────────────────────────────────────────────────────

    #[test]
    fn log_since_none_returns_all_entries() {
        let m = Metrics::new();
        m.push_log("ctrl-rx", "a".to_string());
        m.push_log("ctrl-tx", "b".to_string());
        assert_eq!(m.log_since(None).len(), 2);
    }

    #[test]
    fn log_since_none_on_empty_log_returns_empty() {
        let m = Metrics::new();
        assert!(m.log_since(None).is_empty());
    }

    #[test]
    fn log_since_some_returns_entries_at_and_after_seq() {
        let m = Metrics::new();
        m.push_log("ctrl-rx", "a".to_string()); // seq 0
        m.push_log("ctrl-tx", "b".to_string()); // seq 1
        m.push_log("ctrl-evt", "c".to_string()); // seq 2

        let entries = m.log_since(Some(1));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 1);
        assert_eq!(entries[1].seq, 2);
    }

    #[test]
    fn log_since_seq_zero_returns_all_entries() {
        let m = Metrics::new();
        m.push_log("ctrl-rx", "a".to_string());
        m.push_log("ctrl-rx", "b".to_string());
        assert_eq!(m.log_since(Some(0)).len(), 2);
    }

    #[test]
    fn log_since_seq_past_all_entries_returns_empty() {
        let m = Metrics::new();
        m.push_log("ctrl-rx", "a".to_string()); // seq 0
        assert!(m.log_since(Some(1)).is_empty());
    }

    #[test]
    fn log_since_some_on_empty_log_returns_empty() {
        let m = Metrics::new();
        assert!(m.log_since(Some(0)).is_empty());
        assert!(m.log_since(Some(99)).is_empty());
    }
}
