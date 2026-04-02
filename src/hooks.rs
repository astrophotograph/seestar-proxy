//! Lua script hooks for intercepting and filtering proxy traffic.
//!
//! Scripts can define any of these functions:
//!
//! ```lua
//! -- Called for each client request before forwarding to telescope.
//! -- Return "forward" (default), "block", or a modified JSON string.
//! function on_request(msg)
//!     return "forward"
//! end
//!
//! -- Called for each telescope response before routing to client.
//! function on_response(msg)
//!     return "forward"
//! end
//!
//! -- Called for each async event before broadcasting to clients.
//! function on_event(msg)
//!     return "forward"
//! end
//!
//! -- Called when a client connects.
//! function on_client_connect(addr, port_type)
//! end
//!
//! -- Called when a client disconnects.
//! function on_client_disconnect(addr, port_type)
//! end
//! ```
//!
//! The `msg` argument is a Lua table with the parsed JSON fields.
//! A `telescope` global table exposes cached state.

use mlua::prelude::*;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tracing::{info, warn};

/// Result of a hook invocation.
#[derive(Debug, Clone, PartialEq)]
pub enum HookAction {
    /// Forward the message as-is.
    Forward,
    /// Block/drop the message.
    Block,
    /// Forward a modified message (the string is the new JSON).
    Modify(String),
    /// Send a synthetic response directly back to the client without forwarding to the telescope.
    /// The string is the JSON response to deliver. Used by hooks to fake responses (e.g. auth).
    Reply(String),
}

/// Manages Lua hook scripts.
pub struct HookEngine {
    lua: Mutex<Lua>,
    has_on_request: bool,
    has_on_response: bool,
    has_on_event: bool,
    has_on_upstream_connect: bool,
    has_on_client_connect: bool,
    has_on_client_disconnect: bool,
    upstream_tx: Arc<StdMutex<Option<mpsc::Sender<String>>>>,
}

impl HookEngine {
    /// Create a new hook engine and load the given script files.
    pub fn new(script_paths: &[impl AsRef<Path>]) -> anyhow::Result<Self> {
        // unsafe_new is required to allow loading C extensions (e.g. lua-openssl) via require().
        // Safe mode blocks all C modules.
        let lua = unsafe { Lua::unsafe_new() };

        // Create the telescope state table.
        let telescope = lua.create_table()?;
        telescope.set("is_streaming", false)?;
        telescope.set("is_goto", false)?;
        telescope.set("is_stacking", false)?;
        telescope.set("view_mode", LuaNil)?;
        telescope.set("battery", 0)?;
        telescope.set("temperature", 0.0)?;

        let upstream_tx_arc: Arc<StdMutex<Option<mpsc::Sender<String>>>> =
            Arc::new(StdMutex::new(None));

        // telescope.send — inject a message directly into the upstream channel.
        {
            let tx_ref = upstream_tx_arc.clone();
            let send_fn = lua.create_function(move |lua_ctx, msg: LuaValue| {
                let guard = tx_ref.lock().unwrap();
                if let Some(tx) = guard.as_ref() {
                    let value: serde_json::Value = lua_ctx.from_value(msg)?;
                    let json = serde_json::to_string(&value)
                        .map_err(|e| mlua::Error::external(format!("JSON error: {e}")))?;
                    tx.try_send(json)
                        .map_err(|e| mlua::Error::external(format!("Channel error: {e}")))?;
                } else {
                    warn!(target: "hook", "telescope.send called before upstream connected");
                }
                Ok(())
            })?;
            telescope.set("send", send_fn)?;
        }

        lua.globals().set("telescope", telescope)?;

        // Create a `log` function.
        let log_fn = lua.create_function(|_, msg: String| {
            info!(target: "hook", "{}", msg);
            Ok(())
        })?;
        lua.globals().set("log", log_fn)?;

        // Load each script.
        for path in script_paths {
            let path = path.as_ref();
            info!("Loading hook script: {}", path.display());
            let source = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;
            lua.load(&source)
                .set_name(path.to_string_lossy())
                .exec()
                .map_err(|e| anyhow::anyhow!("Error in {}: {}", path.display(), e))?;
        }

        // Check which hooks are defined.
        let globals = lua.globals();
        let has_on_request = globals.get::<LuaFunction>("on_request").is_ok();
        let has_on_response = globals.get::<LuaFunction>("on_response").is_ok();
        let has_on_event = globals.get::<LuaFunction>("on_event").is_ok();
        let has_on_upstream_connect = globals.get::<LuaFunction>("on_upstream_connect").is_ok();
        let has_on_client_connect = globals.get::<LuaFunction>("on_client_connect").is_ok();
        let has_on_client_disconnect = globals.get::<LuaFunction>("on_client_disconnect").is_ok();

        if script_paths.is_empty() {
            info!("No hook scripts loaded");
        } else {
            info!(
                "Hook functions: request={} response={} event={} upstream={} connect={} disconnect={}",
                has_on_request, has_on_response, has_on_event,
                has_on_upstream_connect, has_on_client_connect, has_on_client_disconnect
            );
        }

        Ok(Self {
            lua: Mutex::new(lua),
            has_on_request,
            has_on_response,
            has_on_event,
            has_on_upstream_connect,
            has_on_client_connect,
            has_on_client_disconnect,
            upstream_tx: upstream_tx_arc,
        })
    }

    /// Set the upstream channel sender so Lua scripts can inject messages via `telescope.send`.
    pub fn set_upstream_tx(&self, tx: mpsc::Sender<String>) {
        *self.upstream_tx.lock().unwrap() = Some(tx);
    }

    /// Call on_upstream_connect hook (fired when the upstream TCP connection is established).
    pub async fn on_upstream_connect(&self, addr: &str) {
        if !self.has_on_upstream_connect {
            return;
        }
        let lua = self.lua.lock().await;
        if let Ok(func) = lua.globals().get::<LuaFunction>("on_upstream_connect")
            && let Err(e) = func.call::<()>(addr.to_string())
        {
            warn!("on_upstream_connect hook error: {}", e);
        }
    }

    /// Call on_request hook. Returns the action to take.
    pub async fn on_request(&self, json: &Value) -> HookAction {
        if !self.has_on_request {
            return HookAction::Forward;
        }
        self.call_message_hook("on_request", json).await
    }

    /// Call on_response hook. Returns the action to take.
    pub async fn on_response(&self, json: &Value) -> HookAction {
        if !self.has_on_response {
            return HookAction::Forward;
        }
        self.call_message_hook("on_response", json).await
    }

    /// Call on_event hook. Returns the action to take.
    pub async fn on_event(&self, json: &Value) -> HookAction {
        if !self.has_on_event {
            return HookAction::Forward;
        }
        self.call_message_hook("on_event", json).await
    }

    /// Call on_client_connect hook.
    pub async fn on_client_connect(&self, addr: &str, port_type: &str) {
        if !self.has_on_client_connect {
            return;
        }
        let lua = self.lua.lock().await;
        if let Ok(func) = lua.globals().get::<LuaFunction>("on_client_connect")
            && let Err(e) = func.call::<()>((addr.to_string(), port_type.to_string()))
        {
            warn!("on_client_connect hook error: {}", e);
        }
    }

    /// Call on_client_disconnect hook.
    pub async fn on_client_disconnect(&self, addr: &str, port_type: &str) {
        if !self.has_on_client_disconnect {
            return;
        }
        let lua = self.lua.lock().await;
        if let Ok(func) = lua.globals().get::<LuaFunction>("on_client_disconnect")
            && let Err(e) = func.call::<()>((addr.to_string(), port_type.to_string()))
        {
            warn!("on_client_disconnect hook error: {}", e);
        }
    }

    /// Update the telescope state table from an event.
    pub async fn update_telescope_state(&self, event: &Value) {
        let lua = self.lua.lock().await;
        let Ok(telescope) = lua.globals().get::<LuaTable>("telescope") else {
            return;
        };

        if let Some(event_type) = event.get("Event").and_then(|v| v.as_str()) {
            match event_type {
                "PiStatus" => {
                    if let Some(temp) = event.get("temp").and_then(|v| v.as_f64()) {
                        let _ = telescope.set("temperature", temp);
                    }
                    if let Some(batt) = event.get("battery_capacity").and_then(|v| v.as_i64()) {
                        let _ = telescope.set("battery", batt);
                    }
                }
                "View" => {
                    if let Some(mode) = event.get("mode").and_then(|v| v.as_str()) {
                        let _ = telescope.set("view_mode", mode.to_string());
                    }
                }
                "Stack" => {
                    let _ = telescope.set("is_stacking", true);
                    if let Some(count) = event.get("count").and_then(|v| v.as_i64()) {
                        let _ = telescope.set("stack_count", count);
                    }
                }
                "AutoGoto" | "ScopeGoto" => {
                    let _ = telescope.set("is_goto", true);
                }
                _ => {}
            }
        }
    }

    /// Internal: call a message hook function and interpret the result.
    async fn call_message_hook(&self, func_name: &str, json: &Value) -> HookAction {
        let lua = self.lua.lock().await;

        let func = match lua.globals().get::<LuaFunction>(func_name) {
            Ok(f) => f,
            Err(_) => return HookAction::Forward,
        };

        // Convert JSON Value to Lua value.
        let lua_value = match lua.to_value(json) {
            Ok(v) => v,
            Err(e) => {
                warn!("{} hook: failed to convert JSON to Lua: {}", func_name, e);
                return HookAction::Forward;
            }
        };

        // Call the hook function.
        let result = match func.call::<LuaValue>(lua_value) {
            Ok(v) => v,
            Err(e) => {
                warn!("{} hook error: {}", func_name, e);
                return HookAction::Forward;
            }
        };

        // Interpret the result.
        match result {
            LuaValue::Nil => HookAction::Forward,
            LuaValue::String(s) => {
                let s = s.to_string_lossy();
                match s.as_ref() {
                    "block" | "drop" | "reject" => HookAction::Block,
                    "forward" | "pass" => HookAction::Forward,
                    other => {
                        // Assume it's a modified JSON string.
                        HookAction::Modify(other.to_string())
                    }
                }
            }
            LuaValue::Boolean(false) => HookAction::Block,
            LuaValue::Boolean(true) => HookAction::Forward,
            LuaValue::Table(_) => {
                match lua.from_value::<Value>(result) {
                    Ok(mut modified) => {
                        // A table with `_reply = true` is a synthetic response to send back
                        // to the client rather than a modified request to forward upstream.
                        let is_reply = modified
                            .get("_reply")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if is_reply {
                            if let Some(obj) = modified.as_object_mut() {
                                obj.remove("_reply");
                            }
                            match serde_json::to_string(&modified) {
                                Ok(s) => HookAction::Reply(s),
                                Err(e) => {
                                    warn!("{} hook: failed to serialize reply table: {}", func_name, e);
                                    HookAction::Block
                                }
                            }
                        } else {
                            match serde_json::to_string(&modified) {
                                Ok(s) => HookAction::Modify(s),
                                Err(e) => {
                                    warn!("{} hook: failed to serialize modified table: {}", func_name, e);
                                    HookAction::Forward
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("{} hook: failed to convert Lua table to JSON: {}", func_name, e);
                        HookAction::Forward
                    }
                }
            }
            _ => HookAction::Forward,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_script(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[tokio::test]
    async fn no_scripts_forwards_everything() {
        let engine = HookEngine::new(&Vec::<std::path::PathBuf>::new()).unwrap();
        let msg = json!({"id": 1, "method": "test"});
        assert_eq!(engine.on_request(&msg).await, HookAction::Forward);
        assert_eq!(engine.on_response(&msg).await, HookAction::Forward);
        assert_eq!(engine.on_event(&msg).await, HookAction::Forward);
    }

    #[tokio::test]
    async fn block_by_string() {
        let script = write_script(r#"
            function on_request(msg)
                if msg.method == "scope_park" then
                    return "block"
                end
                return "forward"
            end
        "#);
        let engine = HookEngine::new(&[script.path()]).unwrap();

        let park = json!({"id": 1, "method": "scope_park"});
        assert_eq!(engine.on_request(&park).await, HookAction::Block);

        let other = json!({"id": 2, "method": "get_device_state"});
        assert_eq!(engine.on_request(&other).await, HookAction::Forward);
    }

    #[tokio::test]
    async fn block_by_boolean() {
        let script = write_script(r#"
            function on_request(msg)
                return msg.method ~= "scope_park"
            end
        "#);
        let engine = HookEngine::new(&[script.path()]).unwrap();

        let park = json!({"id": 1, "method": "scope_park"});
        assert_eq!(engine.on_request(&park).await, HookAction::Block);

        let other = json!({"id": 2, "method": "get_device_state"});
        assert_eq!(engine.on_request(&other).await, HookAction::Forward);
    }

    #[tokio::test]
    async fn modify_by_returning_table() {
        let script = write_script(r#"
            function on_request(msg)
                msg.extra = "injected"
                return msg
            end
        "#);
        let engine = HookEngine::new(&[script.path()]).unwrap();

        let msg = json!({"id": 1, "method": "test"});
        let action = engine.on_request(&msg).await;
        match action {
            HookAction::Modify(s) => {
                let modified: Value = serde_json::from_str(&s).unwrap();
                assert_eq!(modified["extra"], "injected");
                assert_eq!(modified["method"], "test");
            }
            other => panic!("Expected Modify, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn telescope_state_accessible() {
        let script = write_script(r#"
            function on_request(msg)
                if msg.method == "scope_park" and telescope.is_stacking then
                    log("Blocking park during stacking")
                    return "block"
                end
                return "forward"
            end
        "#);
        let engine = HookEngine::new(&[script.path()]).unwrap();

        // Not stacking — park should forward.
        let park = json!({"id": 1, "method": "scope_park"});
        assert_eq!(engine.on_request(&park).await, HookAction::Forward);

        // Simulate stacking event.
        engine
            .update_telescope_state(&json!({"Event": "Stack", "count": 5}))
            .await;

        // Now park should be blocked.
        assert_eq!(engine.on_request(&park).await, HookAction::Block);
    }

    #[tokio::test]
    async fn on_event_can_filter() {
        let script = write_script(r#"
            function on_event(msg)
                if msg.Event == "PiStatus" then
                    return "forward"
                end
                return "block"
            end
        "#);
        let engine = HookEngine::new(&[script.path()]).unwrap();

        let pi = json!({"Event": "PiStatus", "temp": 35.0});
        assert_eq!(engine.on_event(&pi).await, HookAction::Forward);

        let other = json!({"Event": "Debug", "data": "noisy"});
        assert_eq!(engine.on_event(&other).await, HookAction::Block);
    }

    #[tokio::test]
    async fn script_error_defaults_to_forward() {
        let script = write_script(r#"
            function on_request(msg)
                error("intentional crash")
            end
        "#);
        let engine = HookEngine::new(&[script.path()]).unwrap();

        let msg = json!({"id": 1, "method": "test"});
        // Error in hook should not crash the proxy — defaults to forward.
        assert_eq!(engine.on_request(&msg).await, HookAction::Forward);
    }

    #[tokio::test]
    async fn nil_return_means_forward() {
        let script = write_script(r#"
            function on_request(msg)
                -- no return = nil
            end
        "#);
        let engine = HookEngine::new(&[script.path()]).unwrap();

        let msg = json!({"id": 1, "method": "test"});
        assert_eq!(engine.on_request(&msg).await, HookAction::Forward);
    }
}

#[cfg(test)]
mod auth_tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use tempfile::NamedTempFile;
    use tokio::sync::mpsc;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn write_script(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    fn auth_script_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("hooks")
            .join("authenticate.lua")
    }

    /// Load hooks/authenticate.lua and prepend a Lua stub for `_sign_challenge_override`.
    ///
    /// Returns `None` if the script file is not present (gitignored).
    ///
    /// `sign_stub` is a Lua expression for the override function body, e.g.:
    ///   `"function(_k, _c) return \"dGVzdA==\" end"`  — always succeeds
    ///   `"function(_k, _c) error(\"signing failed\") end"` — always fails
    fn patched_auth_script(sign_stub: &str) -> Option<NamedTempFile> {
        let script_path = auth_script_path();
        let src = match std::fs::read_to_string(&script_path) {
            Ok(s) => s,
            Err(_) => {
                eprintln!("Skipping: hooks/authenticate.lua not found at {}", script_path.display());
                return None;
            }
        };
        let header = format!("_sign_challenge_override = {}\n", sign_stub);
        let patched = header + &src;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(patched.as_bytes()).unwrap();
        f.flush().unwrap();
        Some(f)
    }

    const STUB_SIGN_OK: &str = r#"function(_k, _c) return "dGVzdA==" end"#;
    const STUB_SIGN_FAIL: &str = r#"function(_k, _c) error("signing failed") end"#;

    /// Build a HookEngine from authenticate.lua with a signing stub injected.
    /// Returns `None` if the script file is not present (gitignored).
    fn make_engine(sign_stub: &str) -> Option<(HookEngine, mpsc::Receiver<String>)> {
        let script = patched_auth_script(sign_stub)?;
        let engine = HookEngine::new(&[script.path()]).unwrap();
        let (tx, rx) = mpsc::channel(16);
        engine.set_upstream_tx(tx);
        Some((engine, rx))
    }

    /// Pull all currently buffered messages from the upstream channel.
    async fn drain(rx: &mut mpsc::Receiver<String>) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        while let Ok(s) = rx.try_recv() {
            out.push(serde_json::from_str(&s).unwrap());
        }
        out
    }

    // ── auth script loading ───────────────────────────────────────────────────

    #[test]
    fn patched_auth_script_returns_none_when_file_missing() {
        // Point at a path that is guaranteed not to exist.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("authenticate.lua");
        assert!(!missing.exists());

        // Replicate the logic of patched_auth_script with an arbitrary path.
        let result = std::fs::read_to_string(&missing);
        assert!(result.is_err(), "expected Err for missing file");

        // The real helper maps Err → None; verify that contract is exercised
        // by calling it directly.  When the real hooks/authenticate.lua is
        // absent (e.g. CI), patched_auth_script must return None.
        if !auth_script_path().exists() {
            assert!(patched_auth_script(STUB_SIGN_OK).is_none());
        }
    }

    // ── telescope.send ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn telescope_send_delivers_to_channel() {
        let script = write_script(r#"
            function on_client_connect(addr, port_type)
                telescope.send({ id = 42, method = "ping", params = "verify" })
            end
        "#);
        let engine = HookEngine::new(&[script.path()]).unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        engine.set_upstream_tx(tx);

        engine.on_client_connect("127.0.0.1:1234", "control").await;

        let raw = rx.recv().await.unwrap();
        let msg: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(msg["id"], 42);
        assert_eq!(msg["method"], "ping");
    }

    #[tokio::test]
    async fn telescope_send_without_upstream_does_not_panic() {
        // telescope.send before set_upstream_tx is called should warn but not crash.
        let script = write_script(r#"
            function on_client_connect(addr, port_type)
                telescope.send({ id = 1, method = "test", params = "verify" })
            end
        "#);
        let engine = HookEngine::new(&[script.path()]).unwrap();
        // Deliberately do NOT call set_upstream_tx.
        engine.on_client_connect("127.0.0.1:1234", "control").await;
        // No panic — pass.
    }

    // ── authenticate.lua — happy path ─────────────────────────────────────────

    #[tokio::test]
    async fn auth_connect_sends_get_verify_str() {
        let Some((engine, mut rx)) = make_engine(STUB_SIGN_OK) else { return };

        engine.on_upstream_connect("192.168.1.1:4700").await;

        let sent = drain(&mut rx).await;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["id"], 1001);
        assert_eq!(sent[0]["method"], "get_verify_str");
    }

    #[tokio::test]
    async fn auth_does_not_trigger_without_upstream_connect() {
        let Some((_engine, mut rx)) = make_engine(STUB_SIGN_OK) else { return };

        // No on_upstream_connect called — nothing should be sent.
        assert!(drain(&mut rx).await.is_empty());
    }

    #[tokio::test]
    async fn auth_reconnect_re_triggers_handshake() {
        let Some((engine, mut rx)) = make_engine(STUB_SIGN_OK) else { return };

        engine.on_upstream_connect("192.168.1.1:4700").await;
        drain(&mut rx).await; // consume first get_verify_str

        // on_upstream_connect always resets auth state and re-triggers the handshake,
        // so a reconnect (or duplicate connect) must emit another get_verify_str.
        engine.on_upstream_connect("192.168.1.1:4700").await;
        let msgs = drain(&mut rx).await;
        assert_eq!(msgs.len(), 1, "reconnect must re-send get_verify_str");
        assert_eq!(msgs[0]["method"], "get_verify_str");
    }

    #[tokio::test]
    async fn auth_full_happy_path() {
        let Some((engine, mut rx)) = make_engine(STUB_SIGN_OK) else { return };

        // 1. Connect → get_verify_str sent
        engine.on_upstream_connect("192.168.1.1:4700").await;
        let step1 = drain(&mut rx).await;
        assert_eq!(step1[0]["method"], "get_verify_str");

        // 2. Challenge arrives → response blocked, verify_client injected
        let action = engine
            .on_response(&json!({"id": 1001, "code": 0, "result": {"str": "challenge-xyz"}}))
            .await;
        assert_eq!(action, HookAction::Block);
        let step2 = drain(&mut rx).await;
        assert_eq!(step2.len(), 1);
        assert_eq!(step2[0]["method"], "verify_client");
        assert!(!step2[0]["params"]["sign"].as_str().unwrap_or("").is_empty(), "sign must be non-empty");
        assert_eq!(step2[0]["params"]["data"], "challenge-xyz", "data must echo the original challenge");

        // 3. Verify accepted → response blocked, pi_is_verified injected
        let action = engine
            .on_response(&json!({"id": 1002, "code": 0}))
            .await;
        assert_eq!(action, HookAction::Block);
        let step3 = drain(&mut rx).await;
        assert_eq!(step3[0]["method"], "pi_is_verified");

        // 4. Final ack → response blocked, auth done
        let action = engine
            .on_response(&json!({"id": 1003, "code": 0}))
            .await;
        assert_eq!(action, HookAction::Block);
        assert!(drain(&mut rx).await.is_empty());

        // 5. Normal requests now forward
        let req = json!({"id": 1, "method": "get_device_state", "params": "verify"});
        assert_eq!(engine.on_request(&req).await, HookAction::Forward);
    }

    // ── authenticate.lua — request blocking ───────────────────────────────────

    #[tokio::test]
    async fn requests_blocked_until_auth_done() {
        let Some((engine, mut rx)) = make_engine(STUB_SIGN_OK) else { return };
        let req = json!({"id": 1, "method": "get_device_state", "params": "verify"});

        // Idle (before first connect): blocked
        assert_eq!(engine.on_request(&req).await, HookAction::Block);

        // Trigger auth
        engine.on_upstream_connect("192.168.1.1:4700").await;
        drain(&mut rx).await;

        // waiting_challenge: blocked
        assert_eq!(engine.on_request(&req).await, HookAction::Block);

        // Advance through all three steps
        engine.on_response(&json!({"id": 1001, "code": 0, "result": {"str": "c"}})).await;
        drain(&mut rx).await;
        engine.on_response(&json!({"id": 1002, "code": 0})).await;
        drain(&mut rx).await;
        engine.on_response(&json!({"id": 1003, "code": 0})).await;

        // done: forwards
        assert_eq!(engine.on_request(&req).await, HookAction::Forward);
    }

    #[tokio::test]
    async fn auth_methods_get_synthetic_reply_from_clients() {
        let Some((engine, _rx)) = make_engine(STUB_SIGN_OK) else { return };

        // Complete auth so state=done, confirming synthetic replies are unconditional.
        engine.on_upstream_connect("192.168.1.1:4700").await;
        engine.on_response(&json!({"id": 1001, "code": 0, "result": {"str": "c"}})).await;
        engine.on_response(&json!({"id": 1002, "code": 0})).await;
        engine.on_response(&json!({"id": 1003, "code": 0})).await;

        // The script synthesizes success responses for client auth methods so the
        // client believes it authenticated without forwarding to the telescope.
        for method in ["get_verify_str", "verify_client", "pi_is_verified"] {
            let req = json!({"id": 1, "method": method, "params": "verify"});
            let action = engine.on_request(&req).await;
            assert!(
                matches!(action, HookAction::Reply(_)),
                "{method} must get a synthetic reply, got {action:?}"
            );
        }
    }

    // ── authenticate.lua — failure paths ─────────────────────────────────────

    #[tokio::test]
    async fn auth_fails_gracefully_on_missing_key() {
        let Some((engine, mut rx)) = make_engine(STUB_SIGN_FAIL) else { return };
        engine.on_upstream_connect("192.168.1.1:4700").await;
        drain(&mut rx).await; // consume get_verify_str

        engine.on_response(&json!({"id": 1001, "code": 0, "result": {"str": "challenge"}})).await;

        // state=failed → requests pass through
        let req = json!({"id": 1, "method": "get_device_state", "params": "verify"});
        assert_eq!(engine.on_request(&req).await, HookAction::Forward);
    }

    #[tokio::test]
    async fn auth_fails_on_empty_challenge() {
        let Some((engine, mut rx)) = make_engine(STUB_SIGN_OK) else { return };
        engine.on_upstream_connect("192.168.1.1:4700").await;
        drain(&mut rx).await;

        engine.on_response(&json!({"id": 1001, "code": 0, "result": {"str": ""}})).await;

        let req = json!({"id": 1, "method": "get_device_state", "params": "verify"});
        assert_eq!(engine.on_request(&req).await, HookAction::Forward);
    }

    #[tokio::test]
    async fn auth_fails_on_verify_rejection() {
        let Some((engine, mut rx)) = make_engine(STUB_SIGN_OK) else { return };
        engine.on_upstream_connect("192.168.1.1:4700").await;
        drain(&mut rx).await;

        engine.on_response(&json!({"id": 1001, "code": 0, "result": {"str": "challenge"}})).await;
        drain(&mut rx).await; // consume verify_client

        engine.on_response(&json!({"id": 1002, "code": -1})).await;

        let req = json!({"id": 1, "method": "get_device_state", "params": "verify"});
        assert_eq!(engine.on_request(&req).await, HookAction::Forward);
    }

    #[tokio::test]
    async fn pi_is_verified_nonzero_is_non_fatal() {
        let Some((engine, mut rx)) = make_engine(STUB_SIGN_OK) else { return };
        engine.on_upstream_connect("192.168.1.1:4700").await;
        drain(&mut rx).await;
        engine.on_response(&json!({"id": 1001, "code": 0, "result": {"str": "c"}})).await;
        drain(&mut rx).await;
        engine.on_response(&json!({"id": 1002, "code": 0})).await;
        drain(&mut rx).await;

        // Non-zero pi_is_verified should still mark auth done.
        engine.on_response(&json!({"id": 1003, "code": 1})).await;

        let req = json!({"id": 1, "method": "get_device_state", "params": "verify"});
        assert_eq!(engine.on_request(&req).await, HookAction::Forward);
    }
}
