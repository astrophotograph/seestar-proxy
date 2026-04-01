# Lua Hooks

seestar-proxy can load one or more Lua scripts that intercept traffic between clients and the telescope. Hooks let you filter, modify, or log messages without touching the proxy source code.

## Loading scripts

Pass `--hook <path>` for each script. Multiple scripts are loaded into the same Lua state and executed in order, so they share globals.

```sh
seestar-proxy --hook filters.lua --hook logger.lua
```

## Hook functions

Define any of the following global functions in your script. Undefined functions are silently skipped with no overhead.

---

### `on_request(msg)`

Called for every JSON-RPC request sent by a client **before** it is forwarded to the telescope.

| Return value | Effect |
|---|---|
| `"forward"` / `"pass"` / `nil` / `true` | Forward the message unchanged |
| `"block"` / `"drop"` / `"reject"` / `false` | Drop the message; the client receives no response |
| A modified table | Forward the modified message instead |
| A JSON string | Forward the raw JSON string instead |

```lua
function on_request(msg)
    -- Block any attempt to park the scope.
    if msg.method == "scope_park" then
        return "block"
    end
    return "forward"
end
```

---

### `on_response(msg)`

Called for every JSON-RPC response received from the telescope **before** it is routed back to the originating client. Return semantics are identical to `on_request`.

```lua
function on_response(msg)
    -- Strip any internal debug fields before clients see the response.
    msg.debug = nil
    return msg
end
```

---

### `on_event(msg)`

Called for every async event broadcast from the telescope **before** it is sent to all connected clients. Return semantics are identical to `on_request`.

```lua
function on_event(msg)
    -- Suppress noisy PiStatus heartbeats.
    if msg.Event == "PiStatus" then
        return "block"
    end
    return "forward"
end
```

---

### `on_client_connect(addr, port_type)`

Called when a client establishes a connection. No return value is used.

| Argument | Value |
|---|---|
| `addr` | Client address as a string, e.g. `"192.168.1.42:51234"` |
| `port_type` | `"control"` or `"imaging"` |

```lua
function on_client_connect(addr, port_type)
    log("Client connected: " .. addr .. " (" .. port_type .. ")")
end
```

---

### `on_client_disconnect(addr, port_type)`

Called when a client disconnects. Arguments are the same as `on_client_connect`.

```lua
function on_client_disconnect(addr, port_type)
    log("Client disconnected: " .. addr)
end
```

---

## Globals

### `telescope`

A table that is automatically updated as events arrive from the telescope. Use it to make context-aware decisions.

| Field | Type | Updated by event |
|---|---|---|
| `telescope.is_streaming` | boolean | *(reserved)* |
| `telescope.is_goto` | boolean | `AutoGoto`, `ScopeGoto` |
| `telescope.is_stacking` | boolean | `Stack` |
| `telescope.stack_count` | integer | `Stack` |
| `telescope.view_mode` | string or nil | `View` |
| `telescope.battery` | integer (%) | `PiStatus` |
| `telescope.temperature` | number (°C) | `PiStatus` |

```lua
function on_request(msg)
    -- Prevent parking while a stack is in progress.
    if msg.method == "scope_park" and telescope.is_stacking then
        log("Blocking park — stack in progress (" .. telescope.stack_count .. " frames)")
        return "block"
    end
    return "forward"
end
```

### `log(message)`

Writes a message to the proxy log at `INFO` level (tagged `hook`). Use this instead of `print`.

```lua
log("hook script loaded")
```

---

## Error handling

A runtime error inside a hook is logged as a warning and the message is forwarded unchanged — a buggy script will never crash or block the proxy.

---

## Examples

### Allow-list of methods

Only forward a specific set of control commands; block everything else.

```lua
local ALLOWED = {
    get_device_state = true,
    get_view_state   = true,
    iscope_start_view = true,
    iscope_stop_view  = true,
}

function on_request(msg)
    if not ALLOWED[msg.method] then
        log("Blocked: " .. (msg.method or "?"))
        return "block"
    end
    return "forward"
end
```

### Annotate responses with a proxy timestamp

```lua
function on_response(msg)
    msg._proxy_ts = os.time()
    return msg
end
```

### Log all events to a file

```lua
local f = io.open("/tmp/seestar-events.log", "a")

function on_event(msg)
    if f then
        f:write(os.date("%H:%M:%S") .. "  " .. (msg.Event or "?") .. "\n")
        f:flush()
    end
    return "forward"
end
```

### Suppress noisy telemetry from clients

Drop `get_device_state` polling from any client so the telescope isn't hammered when multiple apps are connected.

```lua
local last_poll = 0

function on_request(msg)
    if msg.method == "get_device_state" then
        local now = os.time()
        if now - last_poll < 5 then
            return "block"   -- throttle to once every 5 seconds
        end
        last_poll = now
    end
    return "forward"
end
```
