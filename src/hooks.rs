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
}

/// Manages Lua hook scripts.
pub struct HookEngine {
    lua: Mutex<Lua>,
    has_on_request: bool,
    has_on_response: bool,
    has_on_event: bool,
    has_on_client_connect: bool,
    has_on_client_disconnect: bool,
}

impl HookEngine {
    /// Create a new hook engine and load the given script files.
    pub fn new(script_paths: &[impl AsRef<Path>]) -> anyhow::Result<Self> {
        let lua = Lua::new();

        // Create the telescope state table.
        let telescope = lua.create_table()?;
        telescope.set("is_streaming", false)?;
        telescope.set("is_goto", false)?;
        telescope.set("is_stacking", false)?;
        telescope.set("view_mode", LuaNil)?;
        telescope.set("battery", 0)?;
        telescope.set("temperature", 0.0)?;
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
        let has_on_client_connect = globals.get::<LuaFunction>("on_client_connect").is_ok();
        let has_on_client_disconnect = globals.get::<LuaFunction>("on_client_disconnect").is_ok();

        if script_paths.is_empty() {
            info!("No hook scripts loaded");
        } else {
            info!(
                "Hook functions: request={} response={} event={} connect={} disconnect={}",
                has_on_request, has_on_response, has_on_event,
                has_on_client_connect, has_on_client_disconnect
            );
        }

        Ok(Self {
            lua: Mutex::new(lua),
            has_on_request,
            has_on_response,
            has_on_event,
            has_on_client_connect,
            has_on_client_disconnect,
        })
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
        if let Ok(func) = lua.globals().get::<LuaFunction>("on_client_connect") {
            if let Err(e) = func.call::<()>((addr.to_string(), port_type.to_string())) {
                warn!("on_client_connect hook error: {}", e);
            }
        }
    }

    /// Call on_client_disconnect hook.
    pub async fn on_client_disconnect(&self, addr: &str, port_type: &str) {
        if !self.has_on_client_disconnect {
            return;
        }
        let lua = self.lua.lock().await;
        if let Ok(func) = lua.globals().get::<LuaFunction>("on_client_disconnect") {
            if let Err(e) = func.call::<()>((addr.to_string(), port_type.to_string())) {
                warn!("on_client_disconnect hook error: {}", e);
            }
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
                // Table returned — serialize it back to JSON as modified message.
                match lua.from_value::<Value>(result) {
                    Ok(modified) => match serde_json::to_string(&modified) {
                        Ok(s) => HookAction::Modify(s),
                        Err(e) => {
                            warn!("{} hook: failed to serialize modified table: {}", func_name, e);
                            HookAction::Forward
                        }
                    },
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
