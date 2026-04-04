//! Shared runtime metrics updated by all proxy modules and read by the dashboard.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const LOG_CAPACITY: usize = 100;

/// Latest telescope state derived from event messages.
#[derive(Clone, Default, serde::Serialize)]
pub struct TelescopeStatusSnapshot {
    pub battery: Option<i64>,
    pub temperature: Option<f64>,
    pub is_stacking: bool,
    pub stack_count: i64,
    pub is_goto: bool,
    pub is_homing: bool,
    pub tracking: bool,
    pub view_mode: Option<String>,
    pub charger_status: Option<String>,
    pub last_event: Option<String>,
    pub last_event_ts_ms: u64,
}

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
    /// Latest telescope state derived from event messages.
    telescope_status: Mutex<TelescopeStatusSnapshot>,
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
            telescope_status: Mutex::new(TelescopeStatusSnapshot::default()),
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
    pub fn push_log_with_payload(
        &self,
        channel: &'static str,
        summary: String,
        payload: Option<String>,
    ) {
        let seq = self.log_seq.fetch_add(1, Ordering::Relaxed);
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let entry = LogEntry {
            seq,
            elapsed_ms: self.elapsed_ms(),
            timestamp_ms,
            channel,
            summary,
            payload,
        };
        if let Ok(mut log) = self.log.lock() {
            if log.len() >= LOG_CAPACITY {
                log.pop_front();
            }
            log.push_back(entry);
        }
    }

    /// Update telescope status from an async event message.
    pub fn update_event(&self, event_name: &str, msg: &serde_json::Value) {
        if let Ok(mut s) = self.telescope_status.lock() {
            s.last_event = Some(event_name.to_string());
            s.last_event_ts_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            match event_name {
                "PiStatus" => {
                    // battery_capacity may arrive as integer or float in JSON
                    if let Some(v) = msg
                        .get("battery_capacity")
                        .and_then(|v| v.as_f64())
                        .map(|v| v.round() as i64)
                    {
                        s.battery = Some(v);
                    }
                    if let Some(v) = msg.get("temp").and_then(|v| v.as_f64()) {
                        s.temperature = Some(v);
                    }
                }
                "Stack" => {
                    s.is_stacking = true;
                    if let Some(v) = msg.get("count").and_then(|v| v.as_i64()) {
                        s.stack_count = v;
                    }
                }
                "AutoGoto" | "ScopeGoto" => {
                    s.is_goto = true;
                }
                "ScopeHome" => {
                    let state = msg.get("state").and_then(|v| v.as_str()).unwrap_or("");
                    s.is_homing = state == "working";
                }
                "ScopeTrack" => {
                    s.tracking = msg
                        .get("tracking")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                }
                "View" => {
                    s.view_mode = msg.get("mode").and_then(|v| v.as_str()).map(str::to_string);
                }
                _ => {}
            }
        }
    }

    /// Update telescope status from a ctrl-rx response (e.g. get_device_state).
    pub fn update_response(&self, msg: &serde_json::Value) {
        let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
        if method == "get_device_state"
            && let Some(pi) = msg.get("result").and_then(|r| r.get("pi_status"))
            && let Ok(mut s) = self.telescope_status.lock()
        {
            if let Some(v) = pi
                .get("battery_capacity")
                .and_then(|v| v.as_f64())
                .map(|v| v.round() as i64)
            {
                s.battery = Some(v);
            }
            if let Some(v) = pi.get("temp").and_then(|v| v.as_f64()) {
                s.temperature = Some(v);
            }
            if let Some(v) = pi.get("charger_status").and_then(|v| v.as_str()) {
                s.charger_status = Some(v.to_string());
            }
        }
    }

    /// Reset transient telescope state on upstream reconnect.
    pub fn reset_telescope_state(&self) {
        if let Ok(mut s) = self.telescope_status.lock() {
            s.is_stacking = false;
            s.stack_count = 0;
            s.is_goto = false;
            s.is_homing = false;
        }
    }

    /// Snapshot of the current telescope status for the dashboard.
    pub fn telescope_status(&self) -> TelescopeStatusSnapshot {
        self.telescope_status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
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
        assert!(
            m.elapsed_ms() < 500,
            "should be well under 500ms after construction"
        );
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
        assert_eq!(
            entries.len(),
            LOG_CAPACITY,
            "buffer must not grow past capacity"
        );
        assert_eq!(
            entries[0].summary, "msg-1",
            "oldest entry must have been dropped"
        );
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
            assert!(
                window[1].seq > window[0].seq,
                "seqs must be strictly increasing"
            );
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

    // ── telescope status: initial state ──────────────────────────────────────

    #[test]
    fn telescope_status_defaults_to_empty() {
        let m = Metrics::new();
        let s = m.telescope_status();
        assert!(s.battery.is_none());
        assert!(s.temperature.is_none());
        assert!(s.charger_status.is_none());
        assert!(!s.is_stacking);
        assert_eq!(s.stack_count, 0);
        assert!(!s.is_goto);
        assert!(!s.is_homing);
        assert!(!s.tracking);
        assert!(s.view_mode.is_none());
        assert!(s.last_event.is_none());
        assert_eq!(s.last_event_ts_ms, 0);
    }

    // ── update_event: PiStatus ────────────────────────────────────────────────

    #[test]
    fn update_event_pi_status_sets_temperature() {
        let m = Metrics::new();
        m.update_event(
            "PiStatus",
            &serde_json::json!({"Event": "PiStatus", "temp": 42.5}),
        );
        assert_eq!(m.telescope_status().temperature, Some(42.5));
    }

    #[test]
    fn update_event_pi_status_battery_integer() {
        let m = Metrics::new();
        m.update_event(
            "PiStatus",
            &serde_json::json!({"Event": "PiStatus", "battery_capacity": 75}),
        );
        assert_eq!(m.telescope_status().battery, Some(75));
    }

    #[test]
    fn update_event_pi_status_battery_float_is_rounded() {
        let m = Metrics::new();
        m.update_event(
            "PiStatus",
            &serde_json::json!({"Event": "PiStatus", "battery_capacity": 74.6}),
        );
        assert_eq!(m.telescope_status().battery, Some(75));
    }

    #[test]
    fn update_event_pi_status_missing_battery_leaves_existing_value() {
        let m = Metrics::new();
        m.update_event(
            "PiStatus",
            &serde_json::json!({"Event": "PiStatus", "battery_capacity": 50}),
        );
        // Second update without battery_capacity must not clear what was set.
        m.update_event(
            "PiStatus",
            &serde_json::json!({"Event": "PiStatus", "temp": 40.0}),
        );
        assert_eq!(m.telescope_status().battery, Some(50));
    }

    // ── update_event: Stack ───────────────────────────────────────────────────

    #[test]
    fn update_event_stack_sets_stacking_and_count() {
        let m = Metrics::new();
        m.update_event("Stack", &serde_json::json!({"Event": "Stack", "count": 12}));
        let s = m.telescope_status();
        assert!(s.is_stacking);
        assert_eq!(s.stack_count, 12);
    }

    #[test]
    fn update_event_stack_without_count_still_sets_is_stacking() {
        let m = Metrics::new();
        m.update_event("Stack", &serde_json::json!({"Event": "Stack"}));
        assert!(m.telescope_status().is_stacking);
    }

    // ── update_event: AutoGoto / ScopeGoto ───────────────────────────────────

    #[test]
    fn update_event_autogoto_sets_is_goto() {
        let m = Metrics::new();
        m.update_event("AutoGoto", &serde_json::json!({"Event": "AutoGoto"}));
        assert!(m.telescope_status().is_goto);
    }

    #[test]
    fn update_event_scopegoto_sets_is_goto() {
        let m = Metrics::new();
        m.update_event("ScopeGoto", &serde_json::json!({"Event": "ScopeGoto"}));
        assert!(m.telescope_status().is_goto);
    }

    // ── update_event: ScopeHome ───────────────────────────────────────────────

    #[test]
    fn update_event_scope_home_working_sets_is_homing() {
        let m = Metrics::new();
        m.update_event(
            "ScopeHome",
            &serde_json::json!({"Event": "ScopeHome", "state": "working"}),
        );
        assert!(m.telescope_status().is_homing);
    }

    #[test]
    fn update_event_scope_home_complete_clears_is_homing() {
        let m = Metrics::new();
        m.update_event(
            "ScopeHome",
            &serde_json::json!({"Event": "ScopeHome", "state": "working"}),
        );
        m.update_event(
            "ScopeHome",
            &serde_json::json!({"Event": "ScopeHome", "state": "complete"}),
        );
        assert!(!m.telescope_status().is_homing);
    }

    // ── update_event: ScopeTrack ──────────────────────────────────────────────

    #[test]
    fn update_event_scope_track_true_sets_tracking() {
        let m = Metrics::new();
        m.update_event(
            "ScopeTrack",
            &serde_json::json!({"Event": "ScopeTrack", "tracking": true}),
        );
        assert!(m.telescope_status().tracking);
    }

    #[test]
    fn update_event_scope_track_false_clears_tracking() {
        let m = Metrics::new();
        m.update_event(
            "ScopeTrack",
            &serde_json::json!({"Event": "ScopeTrack", "tracking": true}),
        );
        m.update_event(
            "ScopeTrack",
            &serde_json::json!({"Event": "ScopeTrack", "tracking": false}),
        );
        assert!(!m.telescope_status().tracking);
    }

    // ── update_event: View ────────────────────────────────────────────────────

    #[test]
    fn update_event_view_sets_view_mode() {
        let m = Metrics::new();
        m.update_event(
            "View",
            &serde_json::json!({"Event": "View", "mode": "star"}),
        );
        assert_eq!(m.telescope_status().view_mode.as_deref(), Some("star"));
    }

    // ── update_event: last_event bookkeeping ─────────────────────────────────

    #[test]
    fn update_event_sets_last_event_and_nonzero_timestamp() {
        let m = Metrics::new();
        m.update_event("ScopeTrack", &serde_json::json!({"Event": "ScopeTrack"}));
        let s = m.telescope_status();
        assert_eq!(s.last_event.as_deref(), Some("ScopeTrack"));
        assert!(s.last_event_ts_ms > 0, "timestamp must be set");
    }

    #[test]
    fn update_event_unknown_event_still_records_last_event() {
        let m = Metrics::new();
        m.update_event("Mystery", &serde_json::json!({"Event": "Mystery"}));
        assert_eq!(m.telescope_status().last_event.as_deref(), Some("Mystery"));
    }

    // ── reset_telescope_state ─────────────────────────────────────────────────

    #[test]
    fn reset_telescope_state_clears_transient_flags() {
        let m = Metrics::new();
        m.update_event("Stack", &serde_json::json!({"Event": "Stack", "count": 5}));
        m.update_event("AutoGoto", &serde_json::json!({"Event": "AutoGoto"}));
        m.update_event(
            "ScopeHome",
            &serde_json::json!({"Event": "ScopeHome", "state": "working"}),
        );
        m.reset_telescope_state();
        let s = m.telescope_status();
        assert!(!s.is_stacking);
        assert_eq!(s.stack_count, 0);
        assert!(!s.is_goto);
        assert!(!s.is_homing);
    }

    #[test]
    fn reset_telescope_state_preserves_battery_and_temperature() {
        let m = Metrics::new();
        m.update_event(
            "PiStatus",
            &serde_json::json!({"Event": "PiStatus", "temp": 45.0, "battery_capacity": 80}),
        );
        m.reset_telescope_state();
        let s = m.telescope_status();
        assert_eq!(s.temperature, Some(45.0));
        assert_eq!(s.battery, Some(80));
    }

    // ── update_response ───────────────────────────────────────────────────────

    #[test]
    fn update_response_get_device_state_sets_battery() {
        let m = Metrics::new();
        m.update_response(&serde_json::json!({
            "method": "get_device_state",
            "result": { "pi_status": { "battery_capacity": 63 } }
        }));
        assert_eq!(m.telescope_status().battery, Some(63));
    }

    #[test]
    fn update_response_get_device_state_battery_float_is_rounded() {
        let m = Metrics::new();
        m.update_response(&serde_json::json!({
            "method": "get_device_state",
            "result": { "pi_status": { "battery_capacity": 62.7 } }
        }));
        assert_eq!(m.telescope_status().battery, Some(63));
    }

    #[test]
    fn update_response_get_device_state_sets_temperature_and_charger() {
        let m = Metrics::new();
        m.update_response(&serde_json::json!({
            "method": "get_device_state",
            "result": {
                "pi_status": {
                    "temp": 38.5,
                    "charger_status": "Charging"
                }
            }
        }));
        let s = m.telescope_status();
        assert_eq!(s.temperature, Some(38.5));
        assert_eq!(s.charger_status.as_deref(), Some("Charging"));
    }

    #[test]
    fn update_response_ignores_other_methods() {
        let m = Metrics::new();
        m.update_response(&serde_json::json!({
            "method": "get_view_state",
            "result": { "pi_status": { "battery_capacity": 99 } }
        }));
        assert!(m.telescope_status().battery.is_none());
    }

    #[test]
    fn update_response_missing_pi_status_is_a_noop() {
        let m = Metrics::new();
        m.update_response(&serde_json::json!({
            "method": "get_device_state",
            "result": { "device": { "firmware_ver_int": 2430 } }
        }));
        assert!(m.telescope_status().battery.is_none());
    }
}
