//! Embedded web dashboard for monitoring the proxy.
//!
//! Provides REST endpoints for proxy status and a WebSocket for live
//! message streaming. The frontend is embedded in the binary.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, info};

// ─── Shared State ───────────────────────────────────────────────────────

/// A message flowing through the proxy, for the dashboard feed.
#[derive(Clone, Debug, Serialize)]
pub struct DashboardMessage {
    pub timestamp: f64,
    pub direction: &'static str,
    pub method: Option<String>,
    pub event: Option<String>,
    pub preview: String,
}

/// Per-client connection info.
#[derive(Clone, Debug, Serialize)]
pub struct ClientInfo {
    pub addr: String,
    pub port_type: &'static str,
    pub connected_at_secs: f64,
    pub messages: u64,
}

/// Read-only proxy state shared with the dashboard.
pub struct DashboardState {
    pub started_at: Instant,
    pub upstream_control_connected: AtomicBool,
    pub upstream_imaging_connected: AtomicBool,
    pub control_client_count: AtomicU32,
    pub imaging_client_count: AtomicU32,
    pub messages_from_clients: AtomicU64,
    pub messages_from_telescope: AtomicU64,
    pub imaging_frames: AtomicU64,

    /// Per-method request counts.
    pub method_counts: RwLock<HashMap<String, u64>>,
    /// Per-event-type counts.
    pub event_counts: RwLock<HashMap<String, u64>>,
    /// Latest telescope state from cached events.
    pub telescope_state: RwLock<HashMap<String, Value>>,
    /// Connected clients.
    pub clients: RwLock<Vec<ClientInfo>>,

    /// Tap into the message stream for WebSocket.
    pub message_tap: broadcast::Sender<DashboardMessage>,
}

impl DashboardState {
    pub fn new() -> Self {
        let (message_tap, _) = broadcast::channel(512);
        Self {
            started_at: Instant::now(),
            upstream_control_connected: AtomicBool::new(false),
            upstream_imaging_connected: AtomicBool::new(false),
            control_client_count: AtomicU32::new(0),
            imaging_client_count: AtomicU32::new(0),
            messages_from_clients: AtomicU64::new(0),
            messages_from_telescope: AtomicU64::new(0),
            imaging_frames: AtomicU64::new(0),
            method_counts: RwLock::new(HashMap::new()),
            event_counts: RwLock::new(HashMap::new()),
            telescope_state: RwLock::new(HashMap::new()),
            clients: RwLock::new(Vec::new()),
            message_tap,
        }
    }

    /// Record a method call (request or response).
    pub async fn record_method(&self, method: &str) {
        let mut counts = self.method_counts.write().await;
        *counts.entry(method.to_string()).or_default() += 1;
    }

    /// Record an event type.
    pub async fn record_event(&self, event_type: &str, full_event: &Value) {
        {
            let mut counts = self.event_counts.write().await;
            *counts.entry(event_type.to_string()).or_default() += 1;
        }
        {
            let mut state = self.telescope_state.write().await;
            state.insert(event_type.to_string(), full_event.clone());
        }
    }

    /// Emit a message to the WebSocket tap.
    pub fn tap(&self, msg: DashboardMessage) {
        let _ = self.message_tap.send(msg);
    }

    /// Add a client to the tracking list.
    pub async fn add_client(&self, addr: SocketAddr, port_type: &'static str) {
        let mut clients = self.clients.write().await;
        clients.push(ClientInfo {
            addr: addr.to_string(),
            port_type,
            connected_at_secs: self.started_at.elapsed().as_secs_f64(),
            messages: 0,
        });
    }

    /// Remove a client from the tracking list.
    pub async fn remove_client(&self, addr: SocketAddr) {
        let mut clients = self.clients.write().await;
        let addr_str = addr.to_string();
        clients.retain(|c| c.addr != addr_str);
    }
}

// ─── HTTP Server ────────────────────────────────────────────────────────

/// Start the dashboard HTTP server.
pub async fn run(addr: SocketAddr, state: Arc<DashboardState>) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/api/status", get(api_status))
        .route("/api/clients", get(api_clients))
        .route("/api/stats", get(api_stats))
        .route("/api/telescope", get(api_telescope))
        .route("/api/ws", get(ws_handler))
        .fallback(get(index_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Dashboard listening on http://{}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

// ─── REST Handlers ──────────────────────────────────────────────────────

#[derive(Serialize)]
struct StatusResponse {
    uptime_secs: f64,
    upstream_control: bool,
    upstream_imaging: bool,
    control_clients: u32,
    imaging_clients: u32,
    messages_from_clients: u64,
    messages_from_telescope: u64,
    imaging_frames: u64,
}

async fn api_status(State(ds): State<Arc<DashboardState>>) -> impl IntoResponse {
    axum::Json(StatusResponse {
        uptime_secs: ds.started_at.elapsed().as_secs_f64(),
        upstream_control: ds.upstream_control_connected.load(Ordering::Relaxed),
        upstream_imaging: ds.upstream_imaging_connected.load(Ordering::Relaxed),
        control_clients: ds.control_client_count.load(Ordering::Relaxed),
        imaging_clients: ds.imaging_client_count.load(Ordering::Relaxed),
        messages_from_clients: ds.messages_from_clients.load(Ordering::Relaxed),
        messages_from_telescope: ds.messages_from_telescope.load(Ordering::Relaxed),
        imaging_frames: ds.imaging_frames.load(Ordering::Relaxed),
    })
}

async fn api_clients(State(ds): State<Arc<DashboardState>>) -> impl IntoResponse {
    let clients = ds.clients.read().await;
    axum::Json(clients.clone())
}

#[derive(Serialize)]
struct StatsResponse {
    methods: HashMap<String, u64>,
    events: HashMap<String, u64>,
}

async fn api_stats(State(ds): State<Arc<DashboardState>>) -> impl IntoResponse {
    let methods = ds.method_counts.read().await.clone();
    let events = ds.event_counts.read().await.clone();
    axum::Json(StatsResponse { methods, events })
}

async fn api_telescope(State(ds): State<Arc<DashboardState>>) -> impl IntoResponse {
    let state = ds.telescope_state.read().await;
    axum::Json(state.clone())
}

// ─── WebSocket Handler ──────────────────────────────────────────────────

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(ds): State<Arc<DashboardState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_ws(socket, ds))
}

async fn handle_ws(mut socket: WebSocket, ds: Arc<DashboardState>) {
    let mut rx = ds.message_tap.subscribe();
    debug!("Dashboard WebSocket client connected");

    loop {
        let msg = match rx.recv().await {
            Ok(msg) => msg,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                debug!("Dashboard WS client lagged {} messages", n);
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        };

        let json = match serde_json::to_string(&msg) {
            Ok(j) => j,
            Err(_) => continue,
        };

        if socket.send(Message::Text(json.into())).await.is_err() {
            break;
        }
    }

    debug!("Dashboard WebSocket client disconnected");
}

// ─── Embedded Frontend ──────────────────────────────────────────────────

async fn index_handler() -> Response {
    Html(DASHBOARD_HTML).into_response()
}

const DASHBOARD_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Seestar Proxy</title>
<style>
  :root { --bg: #0d1117; --card: #161b22; --border: #30363d; --text: #e6edf3; --dim: #8b949e; --green: #3fb950; --red: #f85149; --blue: #58a6ff; --yellow: #d29922; }
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif; background: var(--bg); color: var(--text); font-size: 14px; }
  .header { padding: 16px 24px; border-bottom: 1px solid var(--border); display: flex; align-items: center; gap: 12px; }
  .header h1 { font-size: 18px; font-weight: 600; }
  .header .dot { width: 8px; height: 8px; border-radius: 50%; }
  .header .dot.on { background: var(--green); }
  .header .dot.off { background: var(--red); }
  .grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(300px, 1fr)); gap: 16px; padding: 16px 24px; }
  .card { background: var(--card); border: 1px solid var(--border); border-radius: 8px; padding: 16px; }
  .card h2 { font-size: 13px; text-transform: uppercase; letter-spacing: 0.5px; color: var(--dim); margin-bottom: 12px; }
  .stat { display: flex; justify-content: space-between; padding: 4px 0; }
  .stat .label { color: var(--dim); }
  .stat .value { font-variant-numeric: tabular-nums; }
  .feed-container { padding: 0 24px 24px; }
  .feed-header { display: flex; justify-content: space-between; align-items: center; padding: 8px 0; }
  .feed-header h2 { font-size: 13px; text-transform: uppercase; letter-spacing: 0.5px; color: var(--dim); }
  .feed-header button { background: var(--card); border: 1px solid var(--border); color: var(--dim); padding: 4px 12px; border-radius: 4px; cursor: pointer; font-size: 12px; }
  .feed-header button:hover { color: var(--text); }
  .feed { background: var(--card); border: 1px solid var(--border); border-radius: 8px; height: 400px; overflow-y: auto; font-family: 'SF Mono', 'Fira Code', monospace; font-size: 12px; line-height: 1.6; }
  .feed .msg { padding: 2px 12px; border-bottom: 1px solid var(--border); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .feed .msg:hover { background: #1c2129; white-space: normal; }
  .feed .dir-in { color: var(--green); }
  .feed .dir-out { color: var(--blue); }
  .feed .dir-event { color: var(--yellow); }
  .feed .ts { color: var(--dim); margin-right: 8px; }
  .feed .method { font-weight: 600; }
  table { width: 100%; border-collapse: collapse; }
  table td { padding: 4px 0; }
  table td:last-child { text-align: right; font-variant-numeric: tabular-nums; }
  .badge { display: inline-block; padding: 1px 6px; border-radius: 3px; font-size: 11px; }
  .badge.control { background: #1f3a5f; color: var(--blue); }
  .badge.imaging { background: #3b2e1a; color: var(--yellow); }
</style>
</head>
<body>
<div class="header">
  <h1>Seestar Proxy</h1>
  <div class="dot" id="dot-control"></div>
  <span id="status-text" style="color: var(--dim); font-size: 13px;"></span>
</div>

<div class="grid">
  <div class="card" id="card-status">
    <h2>Status</h2>
    <div class="stat"><span class="label">Uptime</span><span class="value" id="s-uptime">-</span></div>
    <div class="stat"><span class="label">Control clients</span><span class="value" id="s-control">0</span></div>
    <div class="stat"><span class="label">Imaging clients</span><span class="value" id="s-imaging">0</span></div>
    <div class="stat"><span class="label">Client messages</span><span class="value" id="s-msg-client">0</span></div>
    <div class="stat"><span class="label">Telescope messages</span><span class="value" id="s-msg-telescope">0</span></div>
    <div class="stat"><span class="label">Imaging frames</span><span class="value" id="s-frames">0</span></div>
  </div>

  <div class="card">
    <h2>Clients</h2>
    <div id="clients-list"><span style="color: var(--dim);">No clients connected</span></div>
  </div>

  <div class="card">
    <h2>Methods</h2>
    <table id="methods-table"></table>
  </div>

  <div class="card">
    <h2>Events</h2>
    <table id="events-table"></table>
  </div>

  <div class="card" style="grid-column: 1 / -1;">
    <h2>Telescope State</h2>
    <div id="telescope-state" style="font-family: monospace; font-size: 12px; color: var(--dim);">Waiting for events...</div>
  </div>
</div>

<div class="feed-container">
  <div class="feed-header">
    <h2>Live Message Feed</h2>
    <button id="btn-pause">Pause</button>
  </div>
  <div class="feed" id="feed"></div>
</div>

<script>
const $ = (s) => document.getElementById(s);

// ─── Polling ──────────────────────────────────────────────────
function fmtUptime(s) {
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = Math.floor(s % 60);
  return h > 0 ? `${h}h ${m}m ${sec}s` : m > 0 ? `${m}m ${sec}s` : `${sec}s`;
}

async function poll() {
  try {
    const r = await fetch('/api/status');
    const d = await r.json();
    $('s-uptime').textContent = fmtUptime(d.uptime_secs);
    $('s-control').textContent = d.control_clients;
    $('s-imaging').textContent = d.imaging_clients;
    $('s-msg-client').textContent = d.messages_from_clients.toLocaleString();
    $('s-msg-telescope').textContent = d.messages_from_telescope.toLocaleString();
    $('s-frames').textContent = d.imaging_frames.toLocaleString();
    const dot = $('dot-control');
    dot.className = 'dot ' + (d.upstream_control ? 'on' : 'off');
    $('status-text').textContent = d.upstream_control ? 'Connected' : 'Disconnected';
  } catch {}

  try {
    const r = await fetch('/api/clients');
    const clients = await r.json();
    const el = $('clients-list');
    if (clients.length === 0) {
      el.innerHTML = '<span style="color: var(--dim);">No clients connected</span>';
    } else {
      el.innerHTML = clients.map(c =>
        `<div class="stat"><span class="label">${c.addr} <span class="badge ${c.port_type}">${c.port_type}</span></span><span class="value">${c.messages || 0} msgs</span></div>`
      ).join('');
    }
  } catch {}

  try {
    const r = await fetch('/api/stats');
    const d = await r.json();
    const mt = $('methods-table');
    mt.innerHTML = Object.entries(d.methods).sort((a,b) => b[1]-a[1]).map(([m,c]) =>
      `<tr><td style="color: var(--dim);">${m}</td><td>${c.toLocaleString()}</td></tr>`
    ).join('');
    const et = $('events-table');
    et.innerHTML = Object.entries(d.events).sort((a,b) => b[1]-a[1]).map(([e,c]) =>
      `<tr><td style="color: var(--dim);">${e}</td><td>${c.toLocaleString()}</td></tr>`
    ).join('');
  } catch {}

  try {
    const r = await fetch('/api/telescope');
    const d = await r.json();
    const el = $('telescope-state');
    const keys = Object.keys(d);
    if (keys.length === 0) {
      el.textContent = 'Waiting for events...';
    } else {
      const parts = keys.sort().map(k => {
        const v = d[k];
        const summary = [];
        if (k === 'PiStatus') {
          if (v.temp !== undefined) summary.push(`temp=${v.temp}\u00b0C`);
          if (v.battery_capacity !== undefined) summary.push(`batt=${v.battery_capacity}%`);
          if (v.charger_status) summary.push(v.charger_status);
        } else {
          const s = JSON.stringify(v);
          summary.push(s.length > 80 ? s.slice(0, 80) + '...' : s);
        }
        return `<b>${k}</b>: ${summary.join(', ')}`;
      });
      el.innerHTML = parts.join('<br>');
    }
  } catch {}
}

setInterval(poll, 2000);
poll();

// ─── WebSocket Feed ───────────────────────────────────────────
let paused = false;
$('btn-pause').onclick = () => {
  paused = !paused;
  $('btn-pause').textContent = paused ? 'Resume' : 'Pause';
};

const feed = $('feed');
const MAX_MESSAGES = 500;

function connectWs() {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const ws = new WebSocket(`${proto}//${location.host}/api/ws`);
  ws.onmessage = (e) => {
    if (paused) return;
    const msg = JSON.parse(e.data);
    const div = document.createElement('div');
    div.className = 'msg';
    const dir = msg.direction.includes('client') ? (msg.direction.includes('telescope') ? 'dir-out' : 'dir-in') : 'dir-event';
    const arrow = msg.event ? '\u2605' : msg.direction.includes('->') ? '\u2192' : '\u2190';
    const label = msg.method || msg.event || '?';
    const ts = new Date(msg.timestamp * 1000).toLocaleTimeString();
    div.innerHTML = `<span class="ts">${ts}</span><span class="${dir}">${arrow}</span> <span class="method">${label}</span> <span style="color: var(--dim);">${msg.preview}</span>`;
    feed.appendChild(div);
    while (feed.children.length > MAX_MESSAGES) feed.removeChild(feed.firstChild);
    if (!paused) feed.scrollTop = feed.scrollHeight;
  };
  ws.onclose = () => setTimeout(connectWs, 2000);
  ws.onerror = () => ws.close();
}
connectWs();
</script>
</body>
</html>
"##;
