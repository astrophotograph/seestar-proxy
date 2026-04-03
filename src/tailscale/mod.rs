//! Embedded Tailscale integration.
//!
//! When enabled (`--tailscale`), the proxy joins the user's tailnet as a
//! node named `--ts-hostname` (default: "seestar-proxy"). Any device on
//! the tailnet can reach the proxy at its Tailscale IP (100.x.x.x) on
//! the control and imaging ports.
//!
//! Uses libtailscale (Go runtime embedded via C FFI) for the networking.
//! Accept loops bridge connections to `127.0.0.1:<port>` where the
//! existing proxy handlers pick them up.

mod bridge;

use std::net::IpAddr;
use std::path::Path;
use tracing::{error, info, warn};

/// Information about the running Tailscale node, for the dashboard.
#[derive(Clone, Debug)]
pub struct TsInfo {
    pub enabled: bool,
    pub hostname: String,
    pub node_ip: Option<String>,
}

/// Start the embedded Tailscale node and spawn accept loops.
///
/// This blocks while the node authenticates (either via auth key or
/// interactive browser flow). Once up, it spawns background tasks that
/// accept connections on the Tailscale interface and bridge them to the
/// local proxy ports.
pub async fn start(
    hostname: &str,
    authkey: Option<&str>,
    state_dir: &Path,
    control_url: Option<&str>,
    control_port: u16,
    imaging_port: u16,
    dashboard_port: u16,
) -> anyhow::Result<TsInfo> {
    let hostname = hostname.to_string();
    let state_dir_str = state_dir
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("invalid state dir path"))?
        .to_string();
    let authkey = authkey.map(|s| s.to_string());
    let control_url = control_url.map(|s| s.to_string());

    // Phase 1: Create and bring up the node (blocking Go calls).
    // We leak the Tailscale instance so it has 'static lifetime — it
    // should live for the entire process anyway. This lets Listener
    // (which borrows &Tailscale) be sent to spawn_blocking threads.
    let ts: &'static libtailscale::Tailscale = tokio::task::spawn_blocking({
        let hostname = hostname.clone();
        move || -> anyhow::Result<&'static libtailscale::Tailscale> {
            let mut ts = libtailscale::Tailscale::new();

            ts.set_hostname(&hostname)
                .map_err(|e| anyhow::anyhow!("set_hostname: {}", e))?;
            ts.set_dir(&state_dir_str)
                .map_err(|e| anyhow::anyhow!("set_dir: {}", e))?;

            if let Some(ref key) = authkey {
                ts.set_authkey(key)
                    .map_err(|e| anyhow::anyhow!("set_authkey: {}", e))?;
            }
            if let Some(ref url) = control_url {
                ts.set_control_url(url)
                    .map_err(|e| anyhow::anyhow!("set_control_url: {}", e))?;
            }

            info!("Tailscale: bringing up node '{}'...", hostname);
            if authkey.is_none() {
                info!("Tailscale: no auth key — check logs for an auth URL");
            }
            ts.up()
                .map_err(|e| anyhow::anyhow!("tailscale up: {}", e))?;

            Ok(Box::leak(Box::new(ts)))
        }
    })
    .await??;

    // Get the node's Tailscale IPs.
    let node_ip = match ts.get_ips() {
        Ok(ips) => {
            info!("Tailscale: node up, IPs: {:?}", ips);
            ips.into_iter()
                .find(|ip| matches!(ip, IpAddr::V4(_)))
                .map(|ip| ip.to_string())
        }
        Err(e) => {
            warn!("Tailscale: could not get IPs: {}", e);
            None
        }
    };

    // Phase 2: Spawn accept loops in blocking threads.
    // Each listener.accept() blocks (Go runtime), so they run on dedicated threads.
    // Accepted connections are bridged to localhost via async tokio tasks.
    spawn_accept_loop(ts, "tcp", control_port, "control");
    spawn_accept_loop(ts, "tcp", imaging_port, "imaging");
    if dashboard_port != 0 {
        spawn_accept_loop(ts, "tcp", dashboard_port, "dashboard");
    }

    Ok(TsInfo {
        enabled: true,
        hostname,
        node_ip,
    })
}

/// Spawn a blocking accept loop for a single port.
fn spawn_accept_loop(
    ts: &'static libtailscale::Tailscale,
    network: &str,
    port: u16,
    label: &'static str,
) {
    let addr = format!(":{}", port);
    let listener = match ts.listen(network, &addr) {
        Ok(l) => l,
        Err(e) => {
            error!("Tailscale: failed to listen on {} ({}): {}", addr, label, e);
            return;
        }
    };
    info!("Tailscale: listening for {} connections on {}", label, addr);

    tokio::task::spawn_blocking(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let handle = tokio::runtime::Handle::current();
                    handle.spawn(bridge::bridge_to_local(stream, port));
                }
                Err(e) => {
                    error!("Tailscale {} accept error: {}", label, e);
                    break;
                }
            }
        }
    });
}
