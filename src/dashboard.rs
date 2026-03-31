//! HTTP dashboard — serves a live status page for the proxy.
//!
//! Routes:
//!   GET /           → HTML dashboard page
//!   GET /api/stream → Server-Sent Events (stats + traffic log, 1 Hz)
//!   GET /api/stats  → JSON snapshot (one-shot poll)

use crate::metrics::Metrics;
use axum::{
    extract::State,
    response::{
        sse::{Event, KeepAlive},
        Html, Sse,
    },
    routing::get,
    Router,
};
use futures_util::stream;
use serde_json::json;
use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::info;

static HTML: &str = r##"<!DOCTYPE html>
<html lang="en"><head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Seestar Proxy</title>
<style>
:root {
  --bg:     #07070f;
  --card:   #0d0d1a;
  --border: rgba(42,48,100,.65);
  --text:   #c8d4f0;
  --dim:    #8892b0;
  --muted:  #4a5070;
  --cyan:   #00d4ff;
  --orange: #ff8a00;
  --purple: #b88aff;
  --green:  #00ff9d;
  --red:    #ff4060;
  --mono:   'JetBrains Mono','Fira Code','Cascadia Code','SF Mono',Consolas,monospace;
  --r: 6px;
}
*,*::before,*::after{box-sizing:border-box;margin:0;padding:0}
body{background:var(--bg);color:var(--text);font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',system-ui,sans-serif;font-size:13px;line-height:1.5;padding:16px 20px;min-height:100vh}

/* Header */
.hdr{display:flex;align-items:center;justify-content:space-between;padding-bottom:14px;margin-bottom:16px;border-bottom:1px solid var(--border)}
.hdr-l{display:flex;align-items:center;gap:10px}
.dot{width:8px;height:8px;border-radius:50%;background:var(--cyan);box-shadow:0 0 8px var(--cyan),0 0 16px rgba(0,212,255,.3);animation:pulse 2.5s ease-in-out infinite}
@keyframes pulse{0%,100%{opacity:1;box-shadow:0 0 8px var(--cyan),0 0 16px rgba(0,212,255,.3)}50%{opacity:.5;box-shadow:0 0 4px var(--cyan)}}
.title{font-family:var(--mono);font-size:13px;font-weight:600;letter-spacing:.1em;text-transform:uppercase}
.uptime{font-family:var(--mono);font-size:12px;color:var(--muted);letter-spacing:.05em}

/* Stat cards */
.stats{display:grid;grid-template-columns:repeat(4,1fr);gap:10px;margin-bottom:12px}
@media(max-width:680px){.stats{grid-template-columns:repeat(2,1fr)}}
.card{background:var(--card);border:1px solid var(--border);border-radius:var(--r);padding:14px 16px;position:relative;overflow:hidden}
.card::after{content:'';position:absolute;top:0;left:0;right:0;height:2px;background:var(--ac,var(--cyan));opacity:.75}
.card.cyan{--ac:var(--cyan)}.card.orange{--ac:var(--orange)}.card.purple{--ac:var(--purple)}.card.green{--ac:var(--green)}
.lbl{font-size:10px;font-weight:600;letter-spacing:.12em;text-transform:uppercase;color:var(--muted);margin-bottom:6px}
.val{font-family:var(--mono);font-size:32px;font-weight:700;color:var(--ac,var(--cyan));line-height:1;margin-bottom:4px;font-variant-numeric:tabular-nums}
.sub{font-size:11px;color:var(--muted)}

/* Mid row */
.mid{display:grid;grid-template-columns:1fr 248px;gap:12px;margin-bottom:12px}
@media(max-width:720px){.mid{grid-template-columns:1fr}}
.panel{background:var(--card);border:1px solid var(--border);border-radius:var(--r);padding:14px 16px}
.ptitle{font-size:10px;font-weight:600;letter-spacing:.12em;text-transform:uppercase;color:var(--muted);margin-bottom:12px;display:flex;align-items:center;gap:6px}
.pdot{width:5px;height:5px;border-radius:50%;background:var(--muted)}

/* Chart */
#chart{width:100%;height:80px;display:block;overflow:visible}
.legend{display:flex;gap:14px;margin-top:8px;flex-wrap:wrap}
.leg{display:flex;align-items:center;gap:5px;font-size:11px;color:var(--dim)}
.ldot{width:6px;height:6px;border-radius:50%}

/* Status panel */
.srow{display:flex;align-items:center;justify-content:space-between;padding:7px 0;border-bottom:1px solid var(--border)}
.srow:last-of-type{border-bottom:none}
.sname{font-family:var(--mono);font-size:11px;color:var(--dim)}
.sbadge{display:flex;align-items:center;gap:5px;font-family:var(--mono);font-size:11px;font-weight:600}
.sled{width:6px;height:6px;border-radius:50%}
.sbadge.up{color:var(--green)}.sbadge.up .sled{background:var(--green);box-shadow:0 0 6px var(--green)}
.sbadge.down{color:var(--red)}.sbadge.down .sled{background:var(--red)}
.divider{height:1px;background:var(--border);margin:10px 0}
.crow{display:flex;justify-content:space-between;padding:3px 0}
.clab{font-size:11px;color:var(--muted)}.cval{font-family:var(--mono);font-size:11px;color:var(--dim);font-variant-numeric:tabular-nums}

/* Log */
.log-hdr{display:flex;align-items:center;justify-content:space-between;margin-bottom:8px}
.log-cont{height:200px;overflow-y:auto;scrollbar-width:thin;scrollbar-color:rgba(60,70,140,.5) transparent}
.log-cont::-webkit-scrollbar{width:4px}.log-cont::-webkit-scrollbar-thumb{background:rgba(60,70,140,.6);border-radius:2px}
.row{display:grid;grid-template-columns:72px 52px 1fr;gap:8px;padding:2px 0;border-bottom:1px solid rgba(42,48,100,.25);font-family:var(--mono);font-size:11px;line-height:1.5;animation:fi .15s ease}
@keyframes fi{from{opacity:0;transform:translateX(-4px)}to{opacity:1;transform:none}}
.ts{color:var(--muted);font-variant-numeric:tabular-nums;white-space:nowrap}
.ch{font-weight:600;white-space:nowrap;text-align:right}
.ch.ctrl-rx{color:var(--cyan)}.ch.ctrl-tx{color:var(--orange)}.ch.ctrl-evt{color:var(--purple)}.ch.img{color:var(--green)}
.sm{color:var(--dim);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.empty{color:var(--muted);font-family:var(--mono);font-size:11px;padding:24px 0;text-align:center}

/* Banner */
.banner{display:none;align-items:center;justify-content:center;gap:8px;background:rgba(255,64,96,.08);border:1px solid rgba(255,64,96,.3);border-radius:var(--r);padding:7px 14px;margin-bottom:12px;font-size:12px;color:var(--red);font-family:var(--mono)}
.banner.on{display:flex}
</style>
</head>
<body>
<div id="banner" class="banner">&#9679; dashboard disconnected — reconnecting&hellip;</div>

<div class="hdr">
  <div class="hdr-l"><div class="dot"></div><span class="title">Seestar Proxy</span></div>
  <div class="uptime" id="uptime">—</div>
</div>

<div class="stats">
  <div class="card cyan">
    <div class="lbl">Control Clients</div>
    <div class="val" id="cc">—</div>
    <div class="sub" id="ctrl-total">0 messages</div>
  </div>
  <div class="card orange">
    <div class="lbl">Imaging Clients</div>
    <div class="val" id="ic">—</div>
    <div class="sub" id="img-total">0 frames</div>
  </div>
  <div class="card purple">
    <div class="lbl">Ctrl Msg / s</div>
    <div class="val" id="cr">—</div>
    <div class="sub">from telescope</div>
  </div>
  <div class="card green">
    <div class="lbl">Frames / s</div>
    <div class="val" id="fr">—</div>
    <div class="sub" id="bw">— /s</div>
  </div>
</div>

<div class="mid">
  <div class="panel">
    <div class="ptitle"><span class="pdot"></span>Message Rate &mdash; 60 s window</div>
    <svg id="chart" viewBox="0 0 600 80" preserveAspectRatio="none">
      <defs>
        <linearGradient id="gc" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stop-color="#00d4ff" stop-opacity=".25"/>
          <stop offset="100%" stop-color="#00d4ff" stop-opacity="0"/>
        </linearGradient>
        <linearGradient id="go" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stop-color="#ff8a00" stop-opacity=".2"/>
          <stop offset="100%" stop-color="#ff8a00" stop-opacity="0"/>
        </linearGradient>
      </defs>
      <path id="ca" fill="url(#gc)" d=""/>
      <path id="oa" fill="url(#go)" d=""/>
      <path id="cl" fill="none" stroke="#00d4ff" stroke-width="1.5" stroke-linejoin="round" d=""/>
      <path id="ol" fill="none" stroke="#ff8a00" stroke-width="1.5" stroke-linejoin="round" d=""/>
    </svg>
    <div class="legend">
      <div class="leg"><div class="ldot" style="background:var(--cyan)"></div>ctrl msgs/s</div>
      <div class="leg"><div class="ldot" style="background:var(--orange)"></div>frames/s</div>
    </div>
  </div>
  <div class="panel">
    <div class="ptitle"><span class="pdot"></span>Upstream</div>
    <div class="srow">
      <span class="sname">control :4700</span>
      <span class="sbadge down" id="sc"><span class="sled"></span><span>—</span></span>
    </div>
    <div class="srow">
      <span class="sname">imaging :4800</span>
      <span class="sbadge down" id="si"><span class="sled"></span><span>—</span></span>
    </div>
    <div class="divider"></div>
    <div class="crow"><span class="clab">ctrl &#8592; telescope</span><span class="cval" id="rxc">0</span></div>
    <div class="crow"><span class="clab">ctrl &#8594; clients</span><span class="cval" id="txc">0</span></div>
    <div class="crow"><span class="clab">events broadcast</span><span class="cval" id="evc">0</span></div>
    <div class="crow"><span class="clab">img bytes total</span><span class="cval" id="ibc">0 B</span></div>
  </div>
</div>

<div class="panel">
  <div class="log-hdr">
    <div class="ptitle" style="margin:0"><span class="pdot"></span>Live Traffic</div>
    <span style="font-size:10px;color:var(--muted)">last 100</span>
  </div>
  <div class="log-cont" id="log"><div class="empty">Waiting for traffic&hellip;</div></div>
</div>

<script>
const N = 60;
let ch = Array(N).fill(0), ih = Array(N).fill(0), lastSeq = -1;

function fmt(ms) {
  const s = ms/1000|0, h = s/3600|0, m = (s%3600)/60|0, sec = s%60;
  return `${String(h).padStart(2,'0')}:${String(m).padStart(2,'0')}:${String(sec).padStart(2,'0')}`;
}
function fmtBytes(n) {
  if (n < 1024) return n + ' B';
  if (n < 1048576) return (n/1024).toFixed(1) + ' KB';
  if (n < 1073741824) return (n/1048576).toFixed(1) + ' MB';
  return (n/1073741824).toFixed(2) + ' GB';
}
function fmtSecs(ms) { return (ms/1000).toFixed(3) + 's'; }

function update(s) {
  document.getElementById('uptime').textContent = fmt(s.uptime_ms);
  document.getElementById('cc').textContent = s.control_clients;
  document.getElementById('ic').textContent = s.imaging_clients;
  document.getElementById('cr').textContent = s.control_rx_rate.toFixed(1);
  document.getElementById('fr').textContent = s.imaging_frame_rate.toFixed(1);
  document.getElementById('ctrl-total').textContent = (s.control_rx + s.control_tx).toLocaleString() + ' messages';
  document.getElementById('img-total').textContent = s.imaging_frames.toLocaleString() + ' frames';
  document.getElementById('bw').textContent = fmtBytes(s.imaging_bytes_rate) + '/s';
  document.getElementById('rxc').textContent = s.control_rx.toLocaleString();
  document.getElementById('txc').textContent = s.control_tx.toLocaleString();
  document.getElementById('evc').textContent = s.control_events.toLocaleString();
  document.getElementById('ibc').textContent = fmtBytes(s.imaging_bytes);
  function conn(id, up) {
    const el = document.getElementById(id);
    el.className = 'sbadge ' + (up ? 'up' : 'down');
    el.querySelector('span:last-child').textContent = up ? 'CONNECTED' : 'OFFLINE';
  }
  conn('sc', s.upstream_control_up);
  conn('si', s.upstream_imaging_up);
  ch.push(s.control_rx_rate); if (ch.length > N) ch.shift();
  ih.push(s.imaging_frame_rate); if (ih.length > N) ih.shift();
  drawChart();
  if (s.log_entries && s.log_entries.length) appendLog(s.log_entries);
}

function drawChart() {
  const W=600, H=80, P=3;
  const mx = Math.max(1, ...ch, ...ih);
  function pts(h) { return h.map((v,i) => [(P + i/(N-1)*(W-P*2)).toFixed(1),(H-P-(v/mx)*(H-P*2)).toFixed(1)]); }
  function line(h) { const p=pts(h); return 'M'+p.map(([x,y])=>x+','+y).join('L'); }
  function area(h) { const p=pts(h),l=p[p.length-1]; return line(h)+`L${l[0]},${H}L${P},${H}Z`; }
  document.getElementById('cl').setAttribute('d', line(ch));
  document.getElementById('ca').setAttribute('d', area(ch));
  document.getElementById('ol').setAttribute('d', line(ih));
  document.getElementById('oa').setAttribute('d', area(ih));
}

function appendLog(entries) {
  const log = document.getElementById('log');
  const empty = log.querySelector('.empty');
  if (empty) empty.remove();
  const atBottom = log.scrollHeight - log.clientHeight <= log.scrollTop + 30;
  for (const e of entries) {
    if (e.seq <= lastSeq) continue;
    lastSeq = e.seq;
    const row = document.createElement('div'); row.className = 'row';
    const ts = document.createElement('span'); ts.className = 'ts'; ts.textContent = fmtSecs(e.elapsed_ms);
    const ch = document.createElement('span'); ch.className = 'ch '+e.channel; ch.textContent = e.channel;
    const sm = document.createElement('span'); sm.className = 'sm'; sm.textContent = e.summary;
    row.append(ts, ch, sm);
    log.appendChild(row);
    while (log.children.length > 100) log.removeChild(log.firstChild);
  }
  if (atBottom) log.scrollTop = log.scrollHeight;
}

(function connect() {
  const es = new EventSource('/api/stream');
  document.getElementById('banner').classList.remove('on');
  es.onmessage = e => update(JSON.parse(e.data));
  es.onerror = () => {
    document.getElementById('banner').classList.add('on');
    es.close();
    setTimeout(connect, 3000);
  };
})();
</script>
</body></html>
"##;

struct SseState {
    metrics: Arc<Metrics>,
    next_log_seq: Option<u64>,
    last_ctrl_rx: u64,
    last_img_frames: u64,
    last_img_bytes: u64,
    last_tick: Instant,
}

/// Run the dashboard HTTP server.
pub async fn run(bind: std::net::SocketAddr, metrics: Arc<Metrics>) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/", get(root))
        .route("/api/stream", get(sse_handler))
        .route("/api/stats", get(stats_handler))
        .with_state(metrics);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!("Dashboard listening on http://{}", bind);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn root() -> Html<&'static str> {
    Html(HTML)
}

async fn stats_handler(State(m): State<Arc<Metrics>>) -> axum::Json<serde_json::Value> {
    axum::Json(build_payload(&m, 0.0, 0.0, 0.0, vec![]))
}

async fn sse_handler(
    State(metrics): State<Arc<Metrics>>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let state = SseState {
        next_log_seq: None,
        last_ctrl_rx: 0,
        last_img_frames: 0,
        last_img_bytes: 0,
        last_tick: Instant::now(),
        metrics,
    };

    let s = stream::unfold(state, |mut s| async move {
        tokio::time::sleep(Duration::from_secs(1)).await;

        let now = Instant::now();
        let dt = now.duration_since(s.last_tick).as_secs_f64().max(0.001);

        let ctrl_rx = s.metrics.control_rx.load(Ordering::Relaxed);
        let img_frames = s.metrics.imaging_frames.load(Ordering::Relaxed);
        let img_bytes = s.metrics.imaging_bytes.load(Ordering::Relaxed);

        let ctrl_rate = (ctrl_rx.saturating_sub(s.last_ctrl_rx)) as f64 / dt;
        let img_rate = (img_frames.saturating_sub(s.last_img_frames)) as f64 / dt;
        let img_bps = (img_bytes.saturating_sub(s.last_img_bytes)) as f64 / dt;

        s.last_ctrl_rx = ctrl_rx;
        s.last_img_frames = img_frames;
        s.last_img_bytes = img_bytes;
        s.last_tick = now;

        let entries = s.metrics.log_since(s.next_log_seq);
        s.next_log_seq = Some(entries.last().map(|e| e.seq + 1).unwrap_or(
            s.next_log_seq.unwrap_or(0),
        ));

        let payload = build_payload(
            &s.metrics,
            (ctrl_rate * 10.0).round() / 10.0,
            (img_rate * 10.0).round() / 10.0,
            img_bps,
            entries,
        );

        let data = serde_json::to_string(&payload).unwrap_or_default();
        let event = Event::default().data(data);
        Some((Ok::<Event, Infallible>(event), s))
    });

    Sse::new(s).keep_alive(KeepAlive::default())
}

pub(crate) fn build_payload(
    m: &Metrics,
    ctrl_rx_rate: f64,
    img_frame_rate: f64,
    img_bps: f64,
    log_entries: Vec<crate::metrics::LogEntry>,
) -> serde_json::Value {
    json!({
        "uptime_ms":           m.elapsed_ms(),
        "control_clients":     m.control_clients.load(Ordering::Relaxed),
        "imaging_clients":     m.imaging_clients.load(Ordering::Relaxed),
        "control_rx":          m.control_rx.load(Ordering::Relaxed),
        "control_tx":          m.control_tx.load(Ordering::Relaxed),
        "control_events":      m.control_events.load(Ordering::Relaxed),
        "imaging_frames":      m.imaging_frames.load(Ordering::Relaxed),
        "imaging_bytes":       m.imaging_bytes.load(Ordering::Relaxed),
        "upstream_control_up": m.upstream_control_up.load(Ordering::Relaxed),
        "upstream_imaging_up": m.upstream_imaging_up.load(Ordering::Relaxed),
        "control_rx_rate":     ctrl_rx_rate,
        "imaging_frame_rate":  img_frame_rate,
        "imaging_bytes_rate":  img_bps as u64,
        "log_entries":         log_entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use std::sync::atomic::Ordering;
    use tower::ServiceExt; // for `.oneshot()`

    fn test_app(metrics: Arc<Metrics>) -> Router {
        Router::new()
            .route("/", get(root))
            .route("/api/stream", get(sse_handler))
            .route("/api/stats", get(stats_handler))
            .with_state(metrics)
    }

    // ── build_payload ─────────────────────────────────────────────────────────

    #[test]
    fn build_payload_contains_all_required_fields() {
        let m = Metrics::new();
        let payload = build_payload(&m, 0.0, 0.0, 0.0, vec![]);
        for field in &[
            "uptime_ms", "control_clients", "imaging_clients",
            "control_rx", "control_tx", "control_events",
            "imaging_frames", "imaging_bytes",
            "upstream_control_up", "upstream_imaging_up",
            "control_rx_rate", "imaging_frame_rate", "imaging_bytes_rate",
            "log_entries",
        ] {
            assert!(payload.get(field).is_some(), "missing field: {}", field);
        }
    }

    #[test]
    fn build_payload_reflects_counter_values() {
        let m = Metrics::new();
        m.control_rx.store(42, Ordering::Relaxed);
        m.control_tx.store(7, Ordering::Relaxed);
        m.imaging_frames.store(100, Ordering::Relaxed);
        m.imaging_bytes.store(1_048_576, Ordering::Relaxed);
        m.control_clients.store(3, Ordering::Relaxed);
        m.imaging_clients.store(1, Ordering::Relaxed);

        let p = build_payload(&m, 0.0, 0.0, 0.0, vec![]);
        assert_eq!(p["control_rx"], 42);
        assert_eq!(p["control_tx"], 7);
        assert_eq!(p["imaging_frames"], 100);
        assert_eq!(p["imaging_bytes"], 1_048_576);
        assert_eq!(p["control_clients"], 3);
        assert_eq!(p["imaging_clients"], 1);
    }

    #[test]
    fn build_payload_reflects_upstream_status() {
        let m = Metrics::new();
        m.upstream_control_up.store(true, Ordering::Relaxed);
        m.upstream_imaging_up.store(false, Ordering::Relaxed);

        let p = build_payload(&m, 0.0, 0.0, 0.0, vec![]);
        assert_eq!(p["upstream_control_up"], true);
        assert_eq!(p["upstream_imaging_up"], false);
    }

    #[test]
    fn build_payload_includes_rate_fields() {
        let m = Metrics::new();
        let p = build_payload(&m, 4.2, 1.5, 98304.0, vec![]);
        assert_eq!(p["control_rx_rate"], 4.2);
        assert_eq!(p["imaging_frame_rate"], 1.5);
        assert_eq!(p["imaging_bytes_rate"], 98304_u64);
    }

    #[test]
    fn build_payload_serializes_log_entries() {
        let m = Metrics::new();
        m.push_log("ctrl-rx", "get_device_state".to_string());
        let entries = m.log_since(None);

        let p = build_payload(&m, 0.0, 0.0, 0.0, entries);
        let log = p["log_entries"].as_array().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0]["channel"], "ctrl-rx");
        assert_eq!(log[0]["summary"], "get_device_state");
        assert!(log[0].get("seq").is_some());
        assert!(log[0].get("elapsed_ms").is_some());
    }

    #[test]
    fn build_payload_log_entries_empty_when_no_traffic() {
        let m = Metrics::new();
        let p = build_payload(&m, 0.0, 0.0, 0.0, vec![]);
        assert_eq!(p["log_entries"].as_array().unwrap().len(), 0);
    }

    // ── HTTP route integration tests ──────────────────────────────────────────

    #[tokio::test]
    async fn get_root_returns_200_with_html() {
        let app = test_app(Metrics::new());
        let response = app
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let ct = response.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(ct.contains("text/html"), "expected text/html, got: {}", ct);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(!body.is_empty());
        assert!(body.windows(15).any(|w| w == b"<!DOCTYPE html>"));
    }

    #[tokio::test]
    async fn get_api_stats_returns_200_with_json() {
        let app = test_app(Metrics::new());
        let response = app
            .oneshot(Request::get("/api/stats").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let ct = response.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(ct.contains("application/json"), "expected json, got: {}", ct);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert!(json.get("uptime_ms").is_some());
        assert!(json.get("control_clients").is_some());
        assert!(json.get("imaging_clients").is_some());
        assert!(json.get("upstream_control_up").is_some());
        assert!(json.get("log_entries").is_some());
    }

    #[tokio::test]
    async fn get_api_stats_reflects_live_counter_values() {
        let m = Metrics::new();
        m.control_rx.store(99, Ordering::Relaxed);
        m.imaging_frames.store(7, Ordering::Relaxed);
        m.upstream_control_up.store(true, Ordering::Relaxed);

        let app = test_app(m);
        let response = app
            .oneshot(Request::get("/api/stats").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["control_rx"], 99);
        assert_eq!(json["imaging_frames"], 7);
        assert_eq!(json["upstream_control_up"], true);
    }

    #[tokio::test]
    async fn get_api_stream_returns_event_stream_content_type() {
        let app = test_app(Metrics::new());
        let response = app
            .oneshot(Request::get("/api/stream").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let ct = response.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(
            ct.contains("text/event-stream"),
            "SSE endpoint must return text/event-stream, got: {}",
            ct
        );
    }

    #[tokio::test]
    async fn unknown_route_returns_404() {
        let app = test_app(Metrics::new());
        let response = app
            .oneshot(Request::get("/no-such-page").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
    }
}
