//! HTTP dashboard — serves a live status page for the proxy.
//!
//! Routes:
//!   GET /           → HTML dashboard page
//!   GET /api/stream → Server-Sent Events (stats + traffic log, 1 Hz)
//!   GET /api/stats  → JSON snapshot (one-shot poll)

use crate::metrics::Metrics;
use axum::{
    Router,
    extract::State,
    response::{
        Html, IntoResponse, Sse,
        sse::{Event, KeepAlive},
    },
    routing::get,
};
use futures_util::stream;
use serde_json::json;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tracing::info;

/// Shared state for the dashboard (metrics + optional WireGuard info).
#[derive(Clone)]
pub struct DashboardState {
    pub metrics: Arc<Metrics>,
    pub wg_config: Option<String>,
    pub wg_qr_svg: Option<String>,
    pub wg_enabled: bool,
    pub wg_endpoint: Option<String>,
}

static HTML: &str = r##"<!DOCTYPE html>
<html lang="en"><head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Seestar Proxy</title>
<link rel="icon" type="image/svg+xml" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Ccircle cx='16' cy='16' r='16' fill='%2307070f'/%3E%3Ccircle cx='16' cy='16' r='9' fill='none' stroke='%2300d4ff' stroke-width='1.5'/%3E%3Ccircle cx='16' cy='16' r='4' fill='%2300d4ff' opacity='.9'/%3E%3Cline x1='16' y1='4' x2='16' y2='7' stroke='%2300d4ff' stroke-width='1.5' stroke-linecap='round'/%3E%3Cline x1='16' y1='25' x2='16' y2='28' stroke='%2300d4ff' stroke-width='1.5' stroke-linecap='round'/%3E%3Cline x1='4' y1='16' x2='7' y2='16' stroke='%2300d4ff' stroke-width='1.5' stroke-linecap='round'/%3E%3Cline x1='25' y1='16' x2='28' y2='16' stroke='%2300d4ff' stroke-width='1.5' stroke-linecap='round'/%3E%3C/svg%3E">
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
.mid{display:grid;grid-template-columns:1fr 220px 200px;gap:12px;margin-bottom:12px}
@media(max-width:900px){.mid{grid-template-columns:1fr 220px}}
@media(max-width:720px){.mid{grid-template-columns:1fr}}
.panel{background:var(--card);border:1px solid var(--border);border-radius:var(--r);padding:14px 16px}
.ptitle{font-size:10px;font-weight:600;letter-spacing:.12em;text-transform:uppercase;color:var(--muted);margin-bottom:12px;display:flex;align-items:center;gap:6px}
.pdot{width:5px;height:5px;border-radius:50%;background:var(--muted)}

/* Chart */
.chart-panel{display:flex;flex-direction:column}
#chart{width:100%;flex:1;min-height:60px;display:block;overflow:visible}
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
.log-cont{height:320px;overflow-y:auto;scrollbar-width:thin;scrollbar-color:rgba(60,70,140,.5) transparent}
.log-cont::-webkit-scrollbar{width:4px}.log-cont::-webkit-scrollbar-thumb{background:rgba(60,70,140,.6);border-radius:2px}
.col-hdr{display:grid;grid-template-columns:72px 60px 1fr;gap:8px;padding:0 0 4px;border-bottom:1px solid var(--border);margin-bottom:2px}
.col-hdr span{font-size:9px;font-weight:700;letter-spacing:.1em;text-transform:uppercase;color:var(--muted);cursor:pointer;user-select:none}
.col-hdr span:hover{color:var(--dim)}.col-hdr span.sorted{color:var(--cyan)}
.col-hdr span.sorted::after{content:' ▲'}.col-hdr span.sorted.desc::after{content:' ▼'}
.entry{border-bottom:1px solid rgba(42,48,100,.25)}
.entry.has-payload{cursor:pointer}.entry.has-payload:hover .row{background:rgba(42,48,100,.15)}
.row{display:grid;grid-template-columns:72px 60px 1fr;gap:8px;padding:3px 0;font-family:var(--mono);font-size:11px;line-height:1.5;border-radius:3px}
.ts{color:var(--muted);font-variant-numeric:tabular-nums;white-space:nowrap}
.ch{font-weight:600;white-space:nowrap;text-align:right}
.ch.ctrl-rx{color:var(--cyan)}.ch.ctrl-tx{color:var(--orange)}.ch.ctrl-evt{color:var(--purple)}.ch.img{color:var(--green)}
.sm{color:var(--dim);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.json-detail{display:none;margin:0 0 4px 148px;padding:6px 10px;background:rgba(0,0,0,.35);border-left:2px solid var(--border);border-radius:0 3px 3px 0;font-family:var(--mono);font-size:10px;line-height:1.6;color:var(--dim);white-space:pre;overflow-x:auto;max-height:260px;overflow-y:auto}
.entry.open .json-detail{display:block}
.empty{color:var(--muted);font-family:var(--mono);font-size:11px;padding:24px 0;text-align:center}
.log-controls{display:flex;align-items:center;gap:6px}
.btn{font-family:var(--mono);font-size:10px;font-weight:600;letter-spacing:.06em;padding:3px 10px;border-radius:3px;border:1px solid;cursor:pointer;background:transparent;transition:background .15s}
.btn-rec{color:var(--red);border-color:rgba(255,64,96,.4)}.btn-rec:hover{background:rgba(255,64,96,.12)}
.btn-rec.recording{background:rgba(255,64,96,.15);border-color:var(--red)}
.btn-exp{color:var(--green);border-color:rgba(0,255,157,.3)}.btn-exp:hover{background:rgba(0,255,157,.08)}
.btn-exp:disabled{opacity:.35;cursor:default;pointer-events:none}
.rec-dot{width:6px;height:6px;border-radius:50%;background:var(--red);display:inline-block;margin-right:4px;animation:pulse 1s ease-in-out infinite}
.rec-info{font-family:var(--mono);font-size:10px;color:var(--red)}

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
  <div class="panel chart-panel">
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
  <div class="panel">
    <div class="ptitle"><span class="pdot"></span>Telescope</div>
    <div class="srow">
      <span class="sname">status</span>
      <span id="ts-status" class="sbadge down"><span class="sled"></span><span>—</span></span>
    </div>
    <div class="divider"></div>
    <div class="crow"><span class="clab">battery</span><span class="cval" id="ts-batt">—</span></div>
    <div class="crow"><span class="clab">charger</span><span class="cval" id="ts-charge">—</span></div>
    <div class="crow"><span class="clab">temperature</span><span class="cval" id="ts-temp">—</span></div>
    <div class="crow"><span class="clab">tracking</span><span class="cval" id="ts-track">—</span></div>
    <div class="crow"><span class="clab">view mode</span><span class="cval" id="ts-view">—</span></div>
    <div class="crow"><span class="clab">stack frames</span><span class="cval" id="ts-stack">—</span></div>
    <div class="divider"></div>
    <div class="crow"><span class="clab">last event</span><span class="cval" id="ts-evt">—</span></div>
    <div class="crow"><span class="clab">event age</span><span class="cval" id="ts-age">—</span></div>
  </div>
</div>

<!-- WG_SECTION -->

<div class="panel">
  <div class="log-hdr">
    <div class="ptitle" style="margin:0"><span class="pdot"></span>Live Traffic</div>
    <div class="log-controls">
      <span id="rec-info" class="rec-info" style="display:none"></span>
      <span id="log-count" style="font-size:10px;color:var(--muted)"></span>
      <button id="btn-rec" class="btn btn-rec">&#9679; Record</button>
      <button id="btn-exp" class="btn btn-exp" disabled>&#8595; Export</button>
    </div>
  </div>
  <div class="col-hdr">
    <span id="sort-ts" class="sorted desc" data-col="ts">Time</span>
    <span id="sort-ch" data-col="ch">Channel</span>
    <span id="sort-sm" data-col="sm">Summary</span>
  </div>
  <div class="log-cont" id="log"><div class="empty">Waiting for traffic&hellip;</div></div>
</div>

<script>
const N = 60;
let ch = Array(N).fill(0), ih = Array(N).fill(0), lastSeq = -1;

// Live traffic store: keeps entries within the rolling window.
const LOG_WINDOW_MS = 5 * 60 * 1000; // 5 minutes — adjust as needed
let logStore = [];           // all entries within window, oldest-first
let sortCol = 'ts';          // current sort column: 'ts' | 'ch' | 'sm'
let sortDesc = true;         // descending = newest first

// Recording state.
let recording = false;
let recordStore = [];        // captured entries for export, oldest-first
let recordStart = null;

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
function fmtTime(ts_ms) {
  const d = new Date(ts_ms);
  return String(d.getHours()).padStart(2,'0') + ':' +
         String(d.getMinutes()).padStart(2,'0') + ':' +
         String(d.getSeconds()).padStart(2,'0');
}

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
  if (s.telescope_status) updateTelescopeStatus(s.telescope_status);
}

function updateTelescopeStatus(ts) {
  const statusEl = document.getElementById('ts-status');
  function badge(cls, color, label) {
    statusEl.className = 'sbadge';
    statusEl.style.color = color ? 'var(--' + color + ')' : '';
    const glow = color ? ';box-shadow:0 0 6px var(--' + color + ')' : '';
    const bg = color ? 'background:var(--' + color + ')' + glow : '';
    statusEl.innerHTML = '<span class="sled" style="' + bg + '"></span><span>' + label + '</span>';
  }
  if (ts.is_goto) {
    badge('', 'orange', 'GoTo');
  } else if (ts.is_stacking) {
    badge('', 'purple', 'Stacking');
  } else if (ts.is_homing) {
    badge('', 'cyan', 'Homing');
  } else if (ts.last_event) {
    const label = ts.tracking ? 'Tracking' : 'Idle';
    badge('up', 'green', label);
  } else {
    badge('down', null, '—');
  }
  document.getElementById('ts-batt').textContent =
    ts.battery != null ? ts.battery + '%' : '—';
  document.getElementById('ts-charge').textContent = ts.charger_status || '—';
  document.getElementById('ts-temp').textContent =
    ts.temperature != null ? ts.temperature.toFixed(1) + '\u00b0C' : '—';
  document.getElementById('ts-track').textContent =
    ts.last_event ? (ts.tracking ? 'on' : 'off') : '—';
  document.getElementById('ts-view').textContent = ts.view_mode || '—';
  document.getElementById('ts-stack').textContent =
    ts.is_stacking ? ts.stack_count : '—';
  document.getElementById('ts-evt').textContent = ts.last_event || '—';
  if (ts.last_event_ts_ms) {
    const age = Math.round((Date.now() - ts.last_event_ts_ms) / 1000);
    document.getElementById('ts-age').textContent =
      age < 60 ? age + 's ago' : Math.round(age/60) + 'm ago';
  } else {
    document.getElementById('ts-age').textContent = '—';
  }
}

function drawChart() {
  const W=600, H=80, P=3;
  const mx = Math.max(1, ...ch, ...ih);
  const mn = Math.min(...ch, ...ih);
  // Autoscale: floor is the min minus 10% of the range, capped at 0.
  // This keeps data filling the chart when traffic is steady (no zero values).
  const lo = Math.max(0, mn - (mx - mn) * 0.1);
  const range = (mx - lo) || 1;
  function pts(h) { return h.map((v,i) => [(P + i/(N-1)*(W-P*2)).toFixed(1),(H-P-((v-lo)/range)*(H-P*2)).toFixed(1)]); }
  function line(h) { const p=pts(h); return 'M'+p.map(([x,y])=>x+','+y).join('L'); }
  function area(h) { const p=pts(h),l=p[p.length-1]; return line(h)+`L${l[0]},${H}L${P},${H}Z`; }
  document.getElementById('cl').setAttribute('d', line(ch));
  document.getElementById('ca').setAttribute('d', area(ch));
  document.getElementById('ol').setAttribute('d', line(ih));
  document.getElementById('oa').setAttribute('d', area(ih));
}

function buildEntry(e) {
  const entry = document.createElement('div'); entry.className = 'entry'; entry.dataset.seq = e.seq;
  const row = document.createElement('div'); row.className = 'row';
  const ts = document.createElement('span'); ts.className = 'ts'; ts.textContent = fmtTime(e.timestamp_ms);
  const ch = document.createElement('span'); ch.className = 'ch ' + e.channel; ch.textContent = e.channel;
  const sm = document.createElement('span'); sm.className = 'sm'; sm.textContent = e.summary;
  row.append(ts, ch, sm);
  entry.appendChild(row);
  if (e.payload) {
    entry.classList.add('has-payload');
    const detail = document.createElement('div'); detail.className = 'json-detail';
    try { detail.textContent = JSON.stringify(JSON.parse(e.payload), null, 2); }
    catch { detail.textContent = e.payload; }
    entry.appendChild(detail);
    entry.addEventListener('click', () => entry.classList.toggle('open'));
  }
  return entry;
}

function renderLog() {
  const log = document.getElementById('log');
  // Preserve expanded state across re-renders.
  const openSeqs = new Set([...log.querySelectorAll('.entry.open')].map(el => +el.dataset.seq));
  // Sort a shallow copy; logStore stays oldest-first.
  const sorted = logStore.slice().sort((a, b) => {
    let av, bv;
    if (sortCol === 'ts')      { av = a.timestamp_ms; bv = b.timestamp_ms; }
    else if (sortCol === 'ch') { av = a.channel;      bv = b.channel; }
    else                       { av = a.summary;      bv = b.summary; }
    if (av < bv) return sortDesc ? 1 : -1;
    if (av > bv) return sortDesc ? -1 : 1;
    return 0;
  });
  log.innerHTML = '';
  if (sorted.length === 0) {
    log.innerHTML = '<div class="empty">Waiting for traffic&hellip;</div>';
  } else {
    for (const e of sorted) {
      const el = buildEntry(e);
      if (openSeqs.has(e.seq)) el.classList.add('open');
      log.appendChild(el);
    }
  }
  document.getElementById('log-count').textContent =
    sorted.length + ' entr' + (sorted.length === 1 ? 'y' : 'ies') +
    ' / ' + Math.round(LOG_WINDOW_MS / 60000) + ' min window';
}

function appendLog(entries) {
  const now = Date.now();
  const cutoff = now - LOG_WINDOW_MS;
  let changed = false;
  for (const e of entries) {
    if (e.seq <= lastSeq) continue;
    lastSeq = e.seq;
    logStore.push(e);
    if (recording) recordStore.push(e);
    changed = true;
  }
  // Evict entries outside the window.
  if (changed) {
    logStore = logStore.filter(e => e.timestamp_ms >= cutoff);
    renderLog();
    if (recording) updateRecInfo();
  }
}

function updateRecInfo() {
  const el = document.getElementById('rec-info');
  const secs = Math.round((Date.now() - recordStart) / 1000);
  el.innerHTML = '<span class="rec-dot"></span>' + recordStore.length + ' (' + secs + 's)';
}

document.getElementById('btn-rec').addEventListener('click', () => {
  const btn = document.getElementById('btn-rec');
  const btnExp = document.getElementById('btn-exp');
  const recInfo = document.getElementById('rec-info');
  if (!recording) {
    recording = true;
    recordStore = [];
    recordStart = Date.now();
    btn.textContent = '&#9632; Stop';
    btn.classList.add('recording');
    recInfo.style.display = '';
    updateRecInfo();
    btnExp.disabled = true;
  } else {
    recording = false;
    btn.innerHTML = '&#9679; Record';
    btn.classList.remove('recording');
    recInfo.style.display = 'none';
    btnExp.disabled = recordStore.length === 0;
  }
});

document.getElementById('btn-exp').addEventListener('click', () => {
  if (recordStore.length === 0) return;
  const ts = new Date(recordStart).toISOString().replace(/[:.]/g,'-').slice(0,19);
  const lines = recordStore.map(e => JSON.stringify({
    time:    new Date(e.timestamp_ms).toISOString(),
    channel: e.channel,
    summary: e.summary,
    payload: e.payload ? JSON.parse(e.payload) : null,
  }));
  const blob = new Blob([lines.join('\n') + '\n'], {type:'application/x-ndjson'});
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = 'seestar-traffic-' + ts + '.jsonl';
  a.click();
  URL.revokeObjectURL(a.href);
});

// Column-sort click handlers.
['sort-ts','sort-ch','sort-sm'].forEach(id => {
  document.getElementById(id).addEventListener('click', () => {
    const col = id.replace('sort-', '');
    if (sortCol === col) { sortDesc = !sortDesc; }
    else { sortCol = col; sortDesc = col === 'ts'; } // default: ts desc, others asc
    document.querySelectorAll('.col-hdr span').forEach(el => {
      el.classList.remove('sorted','desc');
      if (el.id === id) { el.classList.add('sorted'); if (sortDesc) el.classList.add('desc'); }
    });
    renderLog();
  });
});

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
    let state = DashboardState {
        metrics,
        wg_config: None,
        wg_qr_svg: None,
        wg_enabled: false,
        wg_endpoint: None,
    };
    run_with_state(bind, state).await
}

/// Start the dashboard with WireGuard info included.
#[cfg(feature = "wireguard")]
pub async fn run_with_wg(
    bind: std::net::SocketAddr,
    metrics: Arc<Metrics>,
    wg_info: &crate::wireguard::WgInfo,
) -> anyhow::Result<()> {
    let state = DashboardState {
        metrics,
        wg_config: Some(wg_info.client_config.clone()),
        wg_qr_svg: Some(wg_info.client_config_svg.clone()),
        wg_enabled: true,
        wg_endpoint: Some(wg_info.endpoint.clone()),
    };
    run_with_state(bind, state).await
}

async fn run_with_state(bind: std::net::SocketAddr, state: DashboardState) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/", get(root))
        .route("/api/stream", get(sse_handler))
        .route("/api/stats", get(stats_handler))
        .route("/api/wg-config", get(wg_config_handler))
        .route("/api/wg-qr", get(wg_qr_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!("Dashboard listening on http://{}", bind);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn root(State(ds): State<DashboardState>) -> Html<String> {
    let html = if ds.wg_enabled {
        // Inject WireGuard section into the HTML.
        let wg_section = format!(
            r#"<div class="card" id="wg-card" style="grid-column:1/-1">
<div class="card-title">WireGuard Tunnel</div>
<div style="display:flex;gap:24px;align-items:flex-start;flex-wrap:wrap">
<div style="flex:0 0 auto">{}</div>
<div style="flex:1;min-width:300px">
<div class="stat-label">Status</div><div class="stat-value" style="color:var(--green)">Enabled</div>
<div class="stat-label" style="margin-top:8px">Endpoint</div><div class="stat-value">{}</div>
<div class="stat-label" style="margin-top:8px">Config</div>
<pre style="background:rgba(0,0,0,.3);padding:8px;border-radius:4px;font-size:10px;overflow-x:auto;max-width:400px;white-space:pre-wrap;color:var(--dim)">{}</pre>
<div style="margin-top:8px;font-size:11px;color:var(--muted)">Scan the QR code with the WireGuard app, or copy the config above.</div>
</div></div></div>"#,
            ds.wg_qr_svg.as_deref().unwrap_or(""),
            ds.wg_endpoint.as_deref().unwrap_or("?"),
            ds.wg_config.as_deref().unwrap_or(""),
        );
        HTML.replace("<!-- WG_SECTION -->", &wg_section)
    } else {
        HTML.to_string()
    };
    Html(html)
}

async fn wg_config_handler(State(ds): State<DashboardState>) -> impl IntoResponse {
    match ds.wg_config {
        Some(config) => {
            ([(axum::http::header::CONTENT_TYPE, "text/plain")], config).into_response()
        }
        None => (axum::http::StatusCode::NOT_FOUND, "WireGuard not enabled").into_response(),
    }
}

async fn wg_qr_handler(State(ds): State<DashboardState>) -> impl IntoResponse {
    match ds.wg_qr_svg {
        Some(svg) => ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], svg).into_response(),
        None => (axum::http::StatusCode::NOT_FOUND, "WireGuard not enabled").into_response(),
    }
}

async fn stats_handler(State(ds): State<DashboardState>) -> axum::Json<serde_json::Value> {
    axum::Json(build_payload(&ds.metrics, 0.0, 0.0, 0.0, vec![]))
}

async fn sse_handler(
    State(ds): State<DashboardState>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let metrics = ds.metrics;
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
        s.next_log_seq = Some(
            entries
                .last()
                .map(|e| e.seq + 1)
                .unwrap_or(s.next_log_seq.unwrap_or(0)),
        );

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
        "telescope_status":    m.telescope_status(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use std::sync::atomic::Ordering;
    use tower::ServiceExt; // for `.oneshot()`

    fn test_app(metrics: Arc<Metrics>) -> Router {
        let state = DashboardState {
            metrics,
            wg_config: None,
            wg_qr_svg: None,
            wg_enabled: false,
            wg_endpoint: None,
        };
        Router::new()
            .route("/", get(root))
            .route("/api/stream", get(sse_handler))
            .route("/api/stats", get(stats_handler))
            .route("/api/wg-config", get(wg_config_handler))
            .route("/api/wg-qr", get(wg_qr_handler))
            .with_state(state)
    }

    // ── build_payload ─────────────────────────────────────────────────────────

    #[test]
    fn build_payload_contains_all_required_fields() {
        let m = Metrics::new();
        let payload = build_payload(&m, 0.0, 0.0, 0.0, vec![]);
        for field in &[
            "uptime_ms",
            "control_clients",
            "imaging_clients",
            "control_rx",
            "control_tx",
            "control_events",
            "imaging_frames",
            "imaging_bytes",
            "upstream_control_up",
            "upstream_imaging_up",
            "control_rx_rate",
            "imaging_frame_rate",
            "imaging_bytes_rate",
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
        let ct = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.contains("text/html"), "expected text/html, got: {}", ct);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
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
        let ct = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("application/json"),
            "expected json, got: {}",
            ct
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
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

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
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
        let ct = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
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

    // ── WireGuard endpoints (disabled) ────────────────────────────────────────

    #[tokio::test]
    async fn wg_config_returns_404_when_not_enabled() {
        let app = test_app(Metrics::new());
        let response = app
            .oneshot(Request::get("/api/wg-config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
    }

    #[tokio::test]
    async fn wg_qr_returns_404_when_not_enabled() {
        let app = test_app(Metrics::new());
        let response = app
            .oneshot(Request::get("/api/wg-qr").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
    }

    // ── WireGuard endpoints (enabled) ─────────────────────────────────────────

    fn test_app_with_wg(metrics: Arc<Metrics>) -> Router {
        let state = DashboardState {
            metrics,
            wg_config: Some("[Interface]\nPrivateKey = abc\n".to_string()),
            wg_qr_svg: Some("<svg>test</svg>".to_string()),
            wg_enabled: true,
            wg_endpoint: Some("mypi.example.com:51820".to_string()),
        };
        Router::new()
            .route("/", get(root))
            .route("/api/stream", get(sse_handler))
            .route("/api/stats", get(stats_handler))
            .route("/api/wg-config", get(wg_config_handler))
            .route("/api/wg-qr", get(wg_qr_handler))
            .with_state(state)
    }

    #[tokio::test]
    async fn wg_config_returns_text_when_enabled() {
        let app = test_app_with_wg(Metrics::new());
        let response = app
            .oneshot(Request::get("/api/wg-config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let ct = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("text/plain"),
            "expected text/plain, got: {}",
            ct
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.starts_with(b"[Interface]"));
    }

    #[tokio::test]
    async fn wg_config_returns_404_for_empty_config() {
        let state = DashboardState {
            metrics: Metrics::new(),
            wg_config: None,
            wg_qr_svg: None,
            wg_enabled: true, // enabled flag, but no config loaded yet
            wg_endpoint: None,
        };
        let app = Router::new()
            .route("/api/wg-config", get(wg_config_handler))
            .with_state(state);
        let response = app
            .oneshot(Request::get("/api/wg-config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
    }

    #[tokio::test]
    async fn wg_qr_returns_svg_when_enabled() {
        let app = test_app_with_wg(Metrics::new());
        let response = app
            .oneshot(Request::get("/api/wg-qr").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let ct = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("image/svg+xml"),
            "expected image/svg+xml, got: {}",
            ct
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.starts_with(b"<svg>"));
    }

    #[tokio::test]
    async fn wg_qr_returns_404_for_empty_svg() {
        let state = DashboardState {
            metrics: Metrics::new(),
            wg_config: None,
            wg_qr_svg: None,
            wg_enabled: true,
            wg_endpoint: None,
        };
        let app = Router::new()
            .route("/api/wg-qr", get(wg_qr_handler))
            .with_state(state);
        let response = app
            .oneshot(Request::get("/api/wg-qr").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
    }

    #[tokio::test]
    async fn root_with_wg_enabled_injects_wg_section() {
        let app = test_app_with_wg(Metrics::new());
        let response = app
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(
            html.contains("WireGuard Tunnel"),
            "WG section should appear in HTML"
        );
        assert!(
            html.contains("mypi.example.com:51820"),
            "endpoint must appear in HTML"
        );
        assert!(html.contains("[Interface]"), "config must appear in HTML");
        assert!(html.contains("<svg>"), "QR SVG must appear in HTML");
    }

    /// run() binds a real TCP listener — verify the port is reachable.
    #[tokio::test]
    async fn run_binds_and_accepts_connections() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Mimic what run_with_state does — serve the app from a background task.
        let m = Metrics::new();
        let state = DashboardState {
            metrics: m,
            wg_config: None,
            wg_qr_svg: None,
            wg_enabled: false,
            wg_endpoint: None,
        };
        let app = Router::new()
            .route("/api/stats", get(stats_handler))
            .with_state(state);
        let server_task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // Give the server a moment to start.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // Make a raw HTTP/1.1 GET request.
        let mut conn = TcpStream::connect(addr).await.unwrap();
        conn.write_all(b"GET /api/stats HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();

        let mut resp = String::new();
        conn.read_to_string(&mut resp).await.unwrap();
        server_task.abort();

        assert!(
            resp.starts_with("HTTP/1.1 200"),
            "expected 200 OK, got: {}",
            &resp[..resp.len().min(40)]
        );
        assert!(
            resp.contains("control_rx"),
            "response must contain stats JSON"
        );
    }

    #[tokio::test]
    async fn root_without_wg_does_not_inject_wg_section() {
        let app = test_app(Metrics::new());
        let response = app
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(
            !html.contains("WireGuard Tunnel"),
            "WG section must NOT appear when wg_enabled=false"
        );
    }
}
