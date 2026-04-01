use clap::Parser;
use seestar_proxy::{control, dashboard, discovery, imaging, metrics, protocol};
use seestar_proxy::config::Config;
use seestar_proxy::recorder::Recorder;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();

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

    let upstream_host = config.upstream.as_deref().unwrap_or("seestar.local");
    let upstream_ip = resolve_host(upstream_host).await?;

    let upstream_control = SocketAddr::new(upstream_ip, config.upstream_control_port);
    let upstream_imaging = SocketAddr::new(upstream_ip, config.upstream_imaging_port);
    let bind_control = SocketAddr::new(config.bind, config.control_port);
    let bind_imaging = SocketAddr::new(config.bind, config.imaging_port);

    let proxy_metrics = metrics::Metrics::new();

    if config.raw {
        // ─── Raw pipe mode ───────────────────────────────────────────
        println!("Seestar Proxy (RAW PIPE MODE)");
        println!("=============================");
        println!("  Upstream:   {} (control), {} (imaging)", upstream_control, upstream_imaging);

        println!("  Listening:  {} (control), {} (imaging)", bind_control, bind_imaging);
        println!("  Mode:       transparent byte pipe (no parsing, single client)");
        println!();

        let control_handle = tokio::spawn(raw_pipe(bind_control, upstream_control, "control"));
        let imaging_handle = tokio::spawn(raw_pipe(bind_imaging, upstream_imaging, "imaging"));

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
    println!(
        "  Upstream:   {}:{} (control), {}:{} (imaging)",
        upstream_ip, config.upstream_control_port,
        upstream_ip, config.upstream_imaging_port,
    );
    println!(
        "  Listening:  {} (control), {} (imaging)",
        bind_control, bind_imaging
    );
    if config.discovery {
        println!("  Discovery:  bridging on UDP port {}", protocol::DISCOVERY_PORT);
    }
    if let Some(ref dir) = config.record {
        println!("  Recording:  {}", dir.display());
    }

    let dashboard_handle = if config.dashboard_port != 0 {
        let bind_dash = std::net::SocketAddr::new(config.bind, config.dashboard_port);
        let display_host = if config.bind.is_unspecified() { "localhost".to_string() } else { config.bind.to_string() };
        println!("  Dashboard:  http://{}:{}", display_host, config.dashboard_port);
        let metrics_d = proxy_metrics.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = dashboard::run(bind_dash, metrics_d).await {
                error!("Dashboard error: {}", e);
            }
        }))
    } else {
        None
    };
    println!();

    let recorder_c = recorder.clone();
    let metrics_c = proxy_metrics.clone();
    let control_handle = tokio::spawn(async move {
        if let Err(e) = control::run(bind_control, upstream_control, recorder_c, Some(metrics_c)).await {
            error!("Control proxy error: {}", e);
        }
    });

    let recorder_i = recorder.clone();
    let metrics_i = proxy_metrics.clone();
    let imaging_handle = tokio::spawn(async move {
        if let Err(e) = imaging::run(bind_imaging, upstream_imaging, recorder_i, Some(metrics_i)).await {
            error!("Imaging proxy error: {}", e);
        }
    });

    let discovery_handle = if config.discovery {
        let bind = config.bind;
        let upstream = upstream_ip;
        let port = config.control_port;
        Some(tokio::spawn(async move {
            if let Err(e) = discovery::run(bind, upstream, port).await {
                error!("Discovery bridge error: {}", e);
            }
        }))
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
                info!("[{}] client -> telescope: {} bytes (total {})", l1, n, total);
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
                info!("[{}] telescope -> client: {} bytes ({} newlines, total {})", l2, n, newlines, total);
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
