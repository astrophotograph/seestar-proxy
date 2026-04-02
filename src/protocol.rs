//! Seestar protocol types and parsing.
//!
//! Port 4700: JSON-RPC 2.0 over TCP, `\r\n` delimited.
//! Port 4800: Binary frames with 80-byte header + payload.
//! Port 4720: UDP discovery broadcasts.

use serde_json::Value;

/// 80-byte binary frame header (big-endian).
///
/// Format: `>HHHIHHBBHH` (first 20 bytes) + 60 bytes padding.
///
/// Key fields:
/// - `size` (offset 6-9): payload length in bytes
/// - `id` (offset 15): frame type (21=streaming preview, 23=stacked image, 20=view)
/// - `width`/`height` (offsets 16-19): image dimensions
pub const HEADER_SIZE: usize = 80;

#[derive(Debug, Clone)]
pub struct FrameHeader {
    pub size: u32,
    #[allow(dead_code)]
    pub code: u8,
    pub id: u8,
    pub width: u16,
    pub height: u16,
}

impl FrameHeader {
    /// Parse a frame header from exactly 80 bytes.
    pub fn parse(buf: &[u8; HEADER_SIZE]) -> Self {
        Self {
            size: u32::from_be_bytes([buf[6], buf[7], buf[8], buf[9]]),
            code: buf[14],
            id: buf[15],
            width: u16::from_be_bytes([buf[16], buf[17]]),
            height: u16::from_be_bytes([buf[18], buf[19]]),
        }
    }

    /// Returns true if this looks like a real image frame (not a handshake).
    pub fn is_image(&self) -> bool {
        self.width > 0 && self.height > 0 && self.size > 1000
    }
}

/// Discovery broadcast port.
pub const DISCOVERY_PORT: u16 = 4720;

/// Parse the `id` field from a JSON-RPC message.
pub fn json_rpc_id(msg: &Value) -> Option<u64> {
    msg.get("id").and_then(|v| v.as_u64())
}

/// Replace the `id` field in a JSON-RPC message.
pub fn set_json_rpc_id(msg: &mut Value, new_id: u64) {
    if let Some(obj) = msg.as_object_mut() {
        obj.insert("id".to_string(), Value::Number(new_id.into()));
    }
}

/// Check if a JSON message is an async event (has "Event" field, no "id").
pub fn is_event(msg: &Value) -> bool {
    msg.get("Event").is_some()
}

/// Check if a JSON message is a response (has "code" or "result" field + "id").
#[allow(dead_code)]
pub fn is_response(msg: &Value) -> bool {
    msg.get("id").is_some() && (msg.get("code").is_some() || msg.get("result").is_some())
}

/// Get the method name from a JSON-RPC message.
pub fn method_name(msg: &Value) -> Option<&str> {
    msg.get("method").and_then(|v| v.as_str())
}

/// Return the raw `id` value if present and non-null.
///
/// Unlike [`json_rpc_id`], this accepts any JSON type (negative integers,
/// strings, …) and is used for client-side request routing.
pub fn get_id(msg: &Value) -> Option<&Value> {
    msg.get("id").filter(|v| !v.is_null())
}

/// Replace the `id` field with an arbitrary JSON value (used to restore the
/// original client id, which may not be a u64).
pub fn set_json_rpc_id_value(msg: &mut Value, id: Value) {
    if let Some(obj) = msg.as_object_mut() {
        obj.insert("id".to_string(), id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── FrameHeader::parse ────────────────────────────────────────────────────

    #[test]
    fn parse_real_header() {
        // Real header from a Seestar capture: "server connected!" handshake
        let mut buf = [0u8; 80];
        // 03c3 0002 0050 00000011 7321 0000 00 02 0000 0000
        buf[0..2].copy_from_slice(&0x03c3u16.to_be_bytes());
        buf[2..4].copy_from_slice(&0x0002u16.to_be_bytes());
        buf[4..6].copy_from_slice(&0x0050u16.to_be_bytes()); // 80 = header size
        buf[6..10].copy_from_slice(&17u32.to_be_bytes()); // size = 17
        buf[15] = 2; // id = 2

        let header = FrameHeader::parse(&buf);
        assert_eq!(header.size, 17);
        assert_eq!(header.id, 2);
        assert_eq!(header.width, 0);
        assert_eq!(header.height, 0);
        assert!(!header.is_image()); // handshake, not an image
    }

    #[test]
    fn parse_image_header() {
        let mut buf = [0u8; 80];
        buf[6..10].copy_from_slice(&1_833_419u32.to_be_bytes());
        buf[14] = 3; // code
        buf[15] = 20; // id = 20 (preview)
        buf[16..18].copy_from_slice(&1080u16.to_be_bytes());
        buf[18..20].copy_from_slice(&1920u16.to_be_bytes());

        let header = FrameHeader::parse(&buf);
        assert_eq!(header.size, 1_833_419);
        assert_eq!(header.id, 20);
        assert_eq!(header.width, 1080);
        assert_eq!(header.height, 1920);
        assert!(header.is_image());
    }

    #[test]
    fn parse_all_zero_header() {
        let buf = [0u8; 80];
        let header = FrameHeader::parse(&buf);
        assert_eq!(header.size, 0);
        assert_eq!(header.code, 0);
        assert_eq!(header.id, 0);
        assert_eq!(header.width, 0);
        assert_eq!(header.height, 0);
        assert!(!header.is_image());
    }

    #[test]
    fn parse_max_field_values() {
        let mut buf = [0u8; 80];
        buf[6..10].copy_from_slice(&u32::MAX.to_be_bytes());
        buf[14] = u8::MAX;
        buf[15] = u8::MAX;
        buf[16..18].copy_from_slice(&u16::MAX.to_be_bytes());
        buf[18..20].copy_from_slice(&u16::MAX.to_be_bytes());

        let header = FrameHeader::parse(&buf);
        assert_eq!(header.size, u32::MAX);
        assert_eq!(header.code, u8::MAX);
        assert_eq!(header.id, u8::MAX);
        assert_eq!(header.width, u16::MAX);
        assert_eq!(header.height, u16::MAX);
    }

    #[test]
    fn parse_stacked_image_header() {
        let mut buf = [0u8; 80];
        buf[6..10].copy_from_slice(&5_000_000u32.to_be_bytes());
        buf[15] = 23; // id = 23 (stacked image)
        buf[16..18].copy_from_slice(&4056u16.to_be_bytes());
        buf[18..20].copy_from_slice(&3040u16.to_be_bytes());

        let header = FrameHeader::parse(&buf);
        assert_eq!(header.id, 23);
        assert!(header.is_image());
    }

    // ── FrameHeader::is_image ─────────────────────────────────────────────────

    #[test]
    fn is_image_requires_nonzero_width_and_height() {
        let mut buf = [0u8; 80];
        // Large size but zero dimensions — not an image
        buf[6..10].copy_from_slice(&2_000_000u32.to_be_bytes());
        buf[16..18].copy_from_slice(&0u16.to_be_bytes()); // width = 0
        buf[18..20].copy_from_slice(&1080u16.to_be_bytes());
        assert!(!FrameHeader::parse(&buf).is_image());

        buf[16..18].copy_from_slice(&1920u16.to_be_bytes());
        buf[18..20].copy_from_slice(&0u16.to_be_bytes()); // height = 0
        assert!(!FrameHeader::parse(&buf).is_image());
    }

    #[test]
    fn is_image_size_boundary() {
        let mut buf = [0u8; 80];
        buf[16..18].copy_from_slice(&100u16.to_be_bytes());
        buf[18..20].copy_from_slice(&100u16.to_be_bytes());

        // size == 1000 → not an image (threshold is > 1000)
        buf[6..10].copy_from_slice(&1000u32.to_be_bytes());
        assert!(!FrameHeader::parse(&buf).is_image());

        // size == 1001 → is an image
        buf[6..10].copy_from_slice(&1001u32.to_be_bytes());
        assert!(FrameHeader::parse(&buf).is_image());
    }

    // ── json_rpc_id ───────────────────────────────────────────────────────────

    #[test]
    fn json_rpc_helpers() {
        let mut msg: Value =
            serde_json::from_str(r#"{"id": 42, "method": "get_device_state"}"#).unwrap();
        assert_eq!(json_rpc_id(&msg), Some(42));
        assert_eq!(method_name(&msg), Some("get_device_state"));
        assert!(!is_event(&msg));
        assert!(!is_response(&msg));

        set_json_rpc_id(&mut msg, 99);
        assert_eq!(json_rpc_id(&msg), Some(99));

        let event: Value = serde_json::from_str(r#"{"Event": "PiStatus", "temp": 35.0}"#).unwrap();
        assert!(is_event(&event));

        let response: Value =
            serde_json::from_str(r#"{"id": 1, "code": 0, "result": null}"#).unwrap();
        assert!(is_response(&response));
    }

    #[test]
    fn json_rpc_id_absent_returns_none() {
        let msg: Value = serde_json::from_str(r#"{"method": "test"}"#).unwrap();
        assert_eq!(json_rpc_id(&msg), None);
    }

    #[test]
    fn json_rpc_id_string_type_returns_none() {
        // id must be a u64; string IDs are not valid and should return None
        let msg: Value = serde_json::from_str(r#"{"id": "abc"}"#).unwrap();
        assert_eq!(json_rpc_id(&msg), None);
    }

    #[test]
    fn json_rpc_id_float_returns_none() {
        let msg: Value = serde_json::from_str(r#"{"id": 1.5}"#).unwrap();
        assert_eq!(json_rpc_id(&msg), None);
    }

    #[test]
    fn json_rpc_id_null_returns_none() {
        let msg: Value = serde_json::from_str(r#"{"id": null}"#).unwrap();
        assert_eq!(json_rpc_id(&msg), None);
    }

    #[test]
    fn json_rpc_id_zero_is_valid() {
        let msg: Value = serde_json::from_str(r#"{"id": 0}"#).unwrap();
        assert_eq!(json_rpc_id(&msg), Some(0));
    }

    // ── set_json_rpc_id ───────────────────────────────────────────────────────

    #[test]
    fn set_json_rpc_id_preserves_other_fields() {
        let mut msg: Value =
            serde_json::from_str(r#"{"id": 1, "method": "test", "params": {}}"#).unwrap();
        set_json_rpc_id(&mut msg, 10001);
        assert_eq!(json_rpc_id(&msg), Some(10001));
        assert_eq!(msg["method"], "test");
        assert!(msg["params"].is_object());
    }

    #[test]
    fn set_json_rpc_id_inserts_when_absent() {
        let mut msg: Value = serde_json::from_str(r#"{"method": "no_id"}"#).unwrap();
        set_json_rpc_id(&mut msg, 42);
        assert_eq!(json_rpc_id(&msg), Some(42));
    }

    #[test]
    fn set_json_rpc_id_on_non_object_is_noop() {
        // Should not panic or modify non-object values
        let mut arr: Value = serde_json::from_str(r#"[1, 2, 3]"#).unwrap();
        set_json_rpc_id(&mut arr, 99); // must not panic
        assert!(arr.is_array());
    }

    // ── is_event ──────────────────────────────────────────────────────────────

    #[test]
    fn is_event_true_only_when_event_key_present() {
        let with_event: Value = serde_json::from_str(r#"{"Event": "PiStatus"}"#).unwrap();
        assert!(is_event(&with_event));

        // Having an id alongside Event still counts as an event
        let with_both: Value = serde_json::from_str(r#"{"Event": "Alert", "id": 5}"#).unwrap();
        assert!(is_event(&with_both));

        let without_event: Value = serde_json::from_str(r#"{"id": 1, "code": 0}"#).unwrap();
        assert!(!is_event(&without_event));
    }

    // ── is_response ───────────────────────────────────────────────────────────

    #[test]
    fn is_response_requires_id_plus_code_or_result() {
        // id + code → response
        let r: Value = serde_json::from_str(r#"{"id": 1, "code": 0}"#).unwrap();
        assert!(is_response(&r));

        // id + result → response
        let r: Value = serde_json::from_str(r#"{"id": 1, "result": null}"#).unwrap();
        assert!(is_response(&r));

        // id + both → response
        let r: Value = serde_json::from_str(r#"{"id": 1, "code": 0, "result": {}}"#).unwrap();
        assert!(is_response(&r));

        // id alone → not a response
        let r: Value = serde_json::from_str(r#"{"id": 1, "method": "test"}"#).unwrap();
        assert!(!is_response(&r));

        // code without id → not a response
        let r: Value = serde_json::from_str(r#"{"code": 0}"#).unwrap();
        assert!(!is_response(&r));
    }

    // ── method_name ───────────────────────────────────────────────────────────

    #[test]
    fn method_name_absent_returns_none() {
        let msg: Value = serde_json::from_str(r#"{"id": 1}"#).unwrap();
        assert_eq!(method_name(&msg), None);
    }

    #[test]
    fn method_name_non_string_returns_none() {
        let msg: Value = serde_json::from_str(r#"{"method": 42}"#).unwrap();
        assert_eq!(method_name(&msg), None);
    }

    #[test]
    fn method_name_empty_string() {
        let msg: Value = serde_json::from_str(r#"{"method": ""}"#).unwrap();
        assert_eq!(method_name(&msg), Some(""));
    }

    // ── Bug regression: id type handling ─────────────────────────────────────

    /// JSON-RPC 2.0 §4 allows any JSON value (including negative integers and
    /// strings) as a request id. `json_rpc_id` calls `as_u64()` which silently
    /// returns `None` for negative or non-integer ids, causing those requests
    /// to be forwarded un-remapped with no pending entry, so the response is
    /// dropped and the client never gets a reply.
    ///
    /// The fix adds `get_id()` which accepts any non-null JSON value.
    #[test]
    fn get_id_accepts_negative_integer() {
        let msg: Value = serde_json::from_str(r#"{"id": -1, "method": "test"}"#).unwrap();
        // get_id must return Some for negative integers
        assert!(
            get_id(&msg).is_some(),
            "get_id should return Some for id=-1; negative ids are valid JSON-RPC 2.0"
        );
    }

    #[test]
    fn get_id_accepts_string_id() {
        let msg: Value = serde_json::from_str(r#"{"id": "req-abc", "method": "test"}"#).unwrap();
        assert!(get_id(&msg).is_some(), "get_id should accept string ids");
    }

    #[test]
    fn get_id_returns_none_for_null_and_missing() {
        let null_id: Value = serde_json::from_str(r#"{"id": null}"#).unwrap();
        assert!(get_id(&null_id).is_none());

        let no_id: Value = serde_json::from_str(r#"{"method": "notify"}"#).unwrap();
        assert!(get_id(&no_id).is_none());
    }

    #[test]
    fn set_json_rpc_id_value_restores_original_type() {
        let mut msg: Value = serde_json::from_str(r#"{"id": 10001, "code": 0}"#).unwrap();
        // Simulate restoring a negative original id
        set_json_rpc_id_value(&mut msg, serde_json::Value::from(-1i64));
        assert_eq!(msg["id"], -1i64);
    }
}
