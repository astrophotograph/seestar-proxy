use seestar_proxy::config::Config;
use seestar_proxy::recorder::Recorder;
use seestar_proxy::{control, dashboard, discovery, hooks, imaging, metrics, protocol};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::load()?;

    // Set up logging.
    let log_level = match config.verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level)),
        )
        .init();

    // Sync system clock via NTP if requested (must happen before TLS-dependent
    // services like Tailscale, which fail if the clock is wildly off).
    #[cfg(feature = "ntp")]
    if config.ntp_sync {
        match seestar_proxy::ntp::sync(&config.ntp_server) {
            Ok(Some(offset)) => info!("NTP: clock corrected by {:.1}s", offset),
            Ok(None) => info!("NTP: clock already accurate"),
            Err(e) => warn!("NTP sync failed: {} (continuing with current time)", e),
        }
    }

    // Resolve upstream address — either from config or dynamically via SO_ORIGINAL_DST.
    let (upstream_control, upstream_imaging) = if config.transparent && config.upstream.is_none() {
        // Transparent mode without --upstream: address resolved at runtime from
        // the first redirected client connection via SO_ORIGINAL_DST.
        (None, None)
    } else {
        let upstream_host = config.upstream.as_deref().unwrap_or("seestar.local");
        let upstream_ip = resolve_host(upstream_host).await?;
        (
            Some(SocketAddr::new(upstream_ip, config.upstream_control_port)),
            Some(SocketAddr::new(upstream_ip, config.upstream_imaging_port)),
        )
    };
    let bind_control = SocketAddr::new(config.bind, config.control_port);
    let bind_imaging = SocketAddr::new(config.bind, config.imaging_port);

    let proxy_metrics = metrics::Metrics::new();

    if config.raw {
        // ─── Raw pipe mode ───────────────────────────────────────────
        let uc = upstream_control.expect("--upstream is required for raw mode");
        let ui = upstream_imaging.expect("--upstream is required for raw mode");
        println!("Seestar Proxy (RAW PIPE MODE)");
        println!("=============================");
        println!("  Upstream:   {} (control), {} (imaging)", uc, ui);
        println!(
            "  Listening:  {} (control), {} (imaging)",
            bind_control, bind_imaging
        );
        println!("  Mode:       transparent byte pipe (no parsing, single client)");
        println!();

        let control_handle = tokio::spawn(raw_pipe(bind_control, uc, "control"));
        let imaging_handle = tokio::spawn(raw_pipe(bind_imaging, ui, "imaging"));

        tokio::signal::ctrl_c().await?;
        control_handle.abort();
        imaging_handle.abort();
        return Ok(());
    }

    // ─── Normal multiplexing mode ─────────────────────────────────
    let recorder = if let Some(ref dir) = config.record {
        info!("Recording traffic to: {}", dir.display());
        Some(Arc::new(Recorder::new(dir).await?))
    } else {
        None
    };

    println!("Seestar Proxy");
    println!("=============");
    if let Some(uc) = upstream_control {
        println!(
            "  Upstream:   {} (control), {} (imaging)",
            uc,
            upstream_imaging.unwrap()
        );
    } else {
        println!("  Upstream:   (transparent mode — resolved from first client)");
    }
    println!(
        "  Listening:  {} (control), {} (imaging)",
        bind_control, bind_imaging
    );
    if config.transparent {
        println!("  Mode:       transparent (SO_ORIGINAL_DST)");
    }
    if config.discovery {
        println!(
            "  Discovery:  bridging on UDP port {}",
            protocol::DISCOVERY_PORT
        );
    }
    if let Some(ref dir) = config.record {
        println!("  Recording:  {}", dir.display());
    }

    // Dashboard is spawned after WireGuard (below) so it can include WG info.

    // Load hook scripts.
    let hook_engine = if !config.hooks.is_empty() {
        println!("  Hooks:      {} script(s)", config.hooks.len());
        match hooks::HookEngine::new(&config.hooks) {
            Ok(engine) => Some(Arc::new(engine)),
            Err(e) => {
                error!("Failed to load hook scripts: {}", e);
                return Err(e);
            }
        }
    } else {
        None
    };

    // Start WireGuard tunnel if enabled.
    #[cfg(feature = "wireguard")]
    let _wg_info = if config.wireguard {
        let (control_inject_tx, _control_inject_rx) = tokio::sync::mpsc::channel::<TcpStream>(16);
        let (imaging_inject_tx, _imaging_inject_rx) = tokio::sync::mpsc::channel::<TcpStream>(16);

        // Expand ~ in key file path.
        let key_file = if config.wg_key_file.starts_with("~") {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            std::path::PathBuf::from(config.wg_key_file.to_string_lossy().replacen('~', &home, 1))
        } else {
            config.wg_key_file.clone()
        };

        let wg_upstream_ip = upstream_control.map(|s| s.ip()).unwrap_or_else(|| {
            warn!(
                "WireGuard requires --upstream in transparent mode; using localhost as placeholder"
            );
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        });
        match seestar_proxy::wireguard::start(
            config.bind,
            config.wg_port,
            &key_file,
            config.wg_endpoint.clone(),
            wg_upstream_ip,
            config.upstream_control_port,
            config.upstream_imaging_port,
            control_inject_tx,
            imaging_inject_tx,
        )
        .await
        {
            Ok(info) => {
                println!("  WireGuard:  udp://{}:{}", config.bind, config.wg_port);
                Some(info)
            }
            Err(e) => {
                error!("Failed to start WireGuard: {}", e);
                None
            }
        }
    } else {
        None
    };

    #[cfg(not(feature = "wireguard"))]
    let _wg_info: Option<()> = if config.wireguard {
        error!("WireGuard support not compiled in. Rebuild with: cargo build --features wireguard");
        return Err(anyhow::anyhow!("WireGuard feature not enabled"));
    } else {
        None
    };

    // Start embedded Tailscale node if enabled.
    #[cfg(feature = "tailscale")]
    let _ts_info = if config.tailscale {
        // Create state directory if needed.
        if let Err(e) = std::fs::create_dir_all(&config.ts_state_dir) {
            warn!(
                "Could not create Tailscale state dir {}: {}",
                config.ts_state_dir.display(),
                e
            );
        }
        match seestar_proxy::tailscale::start(
            &config.ts_hostname,
            config.ts_authkey.as_deref(),
            &config.ts_state_dir,
            config.ts_control_url.as_deref(),
            config.control_port,
            config.imaging_port,
            config.dashboard_port,
        )
        .await
        {
            Ok(info) => {
                if let Some(ref ip) = info.node_ip {
                    println!(
                        "  Tailscale:  {}:{} (control), {}:{} (imaging)",
                        ip, config.control_port, ip, config.imaging_port
                    );
                } else {
                    println!("  Tailscale:  {} (waiting for IP)", info.hostname);
                }
                Some(info)
            }
            Err(e) => {
                error!("Failed to start Tailscale: {}", e);
                None
            }
        }
    } else {
        None
    };

    // (When tailscale feature is disabled, the --tailscale flag doesn't exist in Config,
    // so no runtime check is needed.)

    // Spawn dashboard (after WireGuard/Tailscale so it can include their info).
    let dashboard_handle = if config.dashboard_port != 0 {
        let bind_dash = std::net::SocketAddr::new(config.bind, config.dashboard_port);
        let display_host = if config.bind.is_unspecified() {
            "localhost".to_string()
        } else {
            config.bind.to_string()
        };
        println!(
            "  Dashboard:  http://{}:{}",
            display_host, config.dashboard_port
        );
        let metrics_d = proxy_metrics.clone();

        #[cfg(feature = "wireguard")]
        let dash_handle = if let Some(ref wg) = _wg_info {
            let wg = wg.clone();
            Some(tokio::spawn(async move {
                if let Err(e) = dashboard::run_with_wg(bind_dash, metrics_d, &wg).await {
                    error!("Dashboard error: {}", e);
                }
            }))
        } else {
            Some(tokio::spawn(async move {
                if let Err(e) = dashboard::run(bind_dash, metrics_d).await {
                    error!("Dashboard error: {}", e);
                }
            }))
        };

        #[cfg(not(feature = "wireguard"))]
        let dash_handle = Some(tokio::spawn(async move {
            if let Err(e) = dashboard::run(bind_dash, metrics_d).await {
                error!("Dashboard error: {}", e);
            }
        }));

        dash_handle
    } else {
        None
    };
    println!();

    let transparent = config.transparent;
    let recorder_c = recorder.clone();
    let metrics_c = proxy_metrics.clone();
    let hooks_c = hook_engine.clone();
    let control_handle = tokio::spawn(async move {
        if let Err(e) = control::run(
            bind_control,
            upstream_control,
            transparent,
            recorder_c,
            Some(metrics_c),
            hooks_c,
        )
        .await
        {
            error!("Control proxy error: {}", e);
        }
    });

    let recorder_i = recorder.clone();
    let metrics_i = proxy_metrics.clone();
    let imaging_handle = tokio::spawn(async move {
        if let Err(e) = imaging::run(
            bind_imaging,
            upstream_imaging,
            transparent,
            recorder_i,
            Some(metrics_i),
        )
        .await
        {
            error!("Imaging proxy error: {}", e);
        }
    });

    let discovery_handle = if config.discovery {
        let bind = config.bind;
        let port = config.control_port;
        let telescope_sn = config.telescope_sn.clone();
        let telescope_model = config.telescope_model.clone();
        let telescope_bssid = config.telescope_bssid.clone();
        if let Some(upstream_ip) = upstream_control.map(|s| s.ip()) {
            Some(tokio::spawn(async move {
                if let Err(e) =
                    discovery::run(bind, upstream_ip, port, telescope_sn, telescope_model, telescope_bssid).await
                {
                    error!("Discovery bridge error: {}", e);
                }
            }))
        } else {
            warn!("Discovery requires --upstream in transparent mode; skipping");
            None
        }
    } else {
        None
    };

    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    if let Some(recorder) = recorder {
        recorder.finalize().await;
    }

    control_handle.abort();
    imaging_handle.abort();
    if let Some(h) = dashboard_handle {
        h.abort();
    }
    if let Some(h) = discovery_handle {
        h.abort();
    }

    Ok(())
}

/// Resolve a hostname or IP string to an [`IpAddr`].
///
/// If the input is already a valid IP address it is returned as-is.
/// Otherwise a DNS lookup is performed and the first result is used.
async fn resolve_host(host: &str) -> anyhow::Result<std::net::IpAddr> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(ip);
    }
    let addrs: Vec<_> = tokio::net::lookup_host(format!("{}:0", host))
        .await
        .map_err(|e| anyhow::anyhow!("Could not resolve '{}': {}", host, e))?
        .collect();
    let ips: Vec<_> = addrs.into_iter().map(|a| a.ip()).collect();
    // Prefer IPv4 — mDNS names like seestar.local often resolve to both
    // an IPv4 and an IPv6 address, and the telescope only listens on IPv4.
    ips.iter()
        .find(|ip| ip.is_ipv4())
        .or_else(|| ips.first())
        .copied()
        .ok_or_else(|| anyhow::anyhow!("No addresses found for '{}'", host))
}

/// Raw transparent pipe: accept one client, connect to upstream,
/// copy bytes in both directions with logging.
async fn raw_pipe(bind: SocketAddr, upstream: SocketAddr, label: &'static str) {
    let listener = match TcpListener::bind(bind).await {
        Ok(l) => l,
        Err(e) => {
            error!("[{}] Failed to bind: {}", label, e);
            return;
        }
    };
    info!("[{}] Listening on {}", label, bind);

    loop {
        let (client, client_addr) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                error!("[{}] Accept error: {}", label, e);
                continue;
            }
        };
        info!("[{}] Client connected: {}", label, client_addr);

        // Connect to upstream on-demand.
        let telescope = match TcpStream::connect(upstream).await {
            Ok(t) => t,
            Err(e) => {
                error!("[{}] Failed to connect upstream: {}", label, e);
                continue;
            }
        };
        info!("[{}] Connected to upstream {}", label, upstream);

        let (mut client_reader, mut client_writer) = client.into_split();
        let (mut telescope_reader, mut telescope_writer) = telescope.into_split();

        let l1 = label;
        let l2 = label;

        // Client → Telescope
        let c2t = tokio::spawn(async move {
            let mut buf = [0u8; 32 * 1024];
            let mut total: u64 = 0;
            loop {
                let n = match client_reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                total += n as u64;
                info!(
                    "[{}] client -> telescope: {} bytes (total {})",
                    l1, n, total
                );
                if telescope_writer.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
            info!("[{}] client -> telescope pipe closed ({} bytes)", l1, total);
        });

        // Telescope → Client
        let t2c = tokio::spawn(async move {
            let mut buf = [0u8; 32 * 1024];
            let mut total: u64 = 0;
            loop {
                let n = match telescope_reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                total += n as u64;
                let newlines = buf[..n].iter().filter(|&&b| b == b'\n').count();
                info!(
                    "[{}] telescope -> client: {} bytes ({} newlines, total {})",
                    l2, n, newlines, total
                );
                if client_writer.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
            info!("[{}] telescope -> client pipe closed ({} bytes)", l2, total);
        });

        let _ = tokio::join!(c2t, t2c);
        info!("[{}] Session ended for {}", label, client_addr);
    }
}

