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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn json_rpc_helpers() {
        let mut msg: Value =
            serde_json::from_str(r#"{"id": 42, "method": "get_device_state"}"#).unwrap();
        assert_eq!(json_rpc_id(&msg), Some(42));
        assert_eq!(method_name(&msg), Some("get_device_state"));
        assert!(!is_event(&msg));
        assert!(!is_response(&msg));

        set_json_rpc_id(&mut msg, 99);
        assert_eq!(json_rpc_id(&msg), Some(99));

        let event: Value =
            serde_json::from_str(r#"{"Event": "PiStatus", "temp": 35.0}"#).unwrap();
        assert!(is_event(&event));

        let response: Value =
            serde_json::from_str(r#"{"id": 1, "code": 0, "result": null}"#).unwrap();
        assert!(is_response(&response));
    }
}
