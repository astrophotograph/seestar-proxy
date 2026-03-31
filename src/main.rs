use clap::Parser;
use seestar_proxy::{control, discovery, imaging, protocol};
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

    let upstream_control = SocketAddr::new(config.upstream.into(), config.upstream_control_port);
    let upstream_imaging = SocketAddr::new(config.upstream.into(), config.upstream_imaging_port);
    let bind_control = SocketAddr::new(config.bind, config.control_port);
    let bind_imaging = SocketAddr::new(config.bind, config.imaging_port);

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
        config.upstream, config.upstream_control_port,
        config.upstream, config.upstream_imaging_port,
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
    println!();

    let recorder_c = recorder.clone();
    let control_handle = tokio::spawn(async move {
        if let Err(e) = control::run(bind_control, upstream_control, recorder_c).await {
            error!("Control proxy error: {}", e);
        }
    });

    let recorder_i = recorder.clone();
    let imaging_handle = tokio::spawn(async move {
        if let Err(e) = imaging::run(bind_imaging, upstream_imaging, recorder_i).await {
            error!("Imaging proxy error: {}", e);
        }
    });

    let discovery_handle = if config.discovery {
        let bind = config.bind;
        let upstream = config.upstream;
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
    if let Some(h) = discovery_handle {
        h.abort();
    }

    Ok(())
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
