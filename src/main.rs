use clap::Parser;
use seestar_proxy::{control, discovery, imaging, protocol};
use seestar_proxy::config::Config;
use seestar_proxy::recorder::Recorder;
use std::net::SocketAddr;
use std::sync::Arc;
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

    // Set up recorder if requested.
    let recorder = if let Some(ref dir) = config.record {
        info!("Recording traffic to: {}", dir.display());
        Some(Arc::new(Recorder::new(dir).await?))
    } else {
        None
    };

    let upstream_control = SocketAddr::new(config.upstream.into(), config.upstream_control_port);
    let upstream_imaging = SocketAddr::new(config.upstream.into(), config.upstream_imaging_port);
    let bind_control = SocketAddr::new(config.bind, config.control_port);
    let bind_imaging = SocketAddr::new(config.bind, config.imaging_port);

    println!("Seestar Proxy");
    println!("=============");
    println!(
        "  Upstream:   {}:{} (control), {}:{} (imaging)",
        config.upstream,
        config.upstream_control_port,
        config.upstream,
        config.upstream_imaging_port,
    );
    println!(
        "  Listening:  {} (control), {} (imaging)",
        bind_control, bind_imaging
    );
    if config.discovery {
        println!(
            "  Discovery:  bridging on UDP port {}",
            protocol::DISCOVERY_PORT
        );
    }
    if let Some(ref dir) = config.record {
        println!("  Recording:  {}", dir.display());
    }
    println!();

    // Spawn all proxy tasks.
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

    // Wait for Ctrl+C.
    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    // Finalize recording if active.
    if let Some(recorder) = recorder {
        recorder.finalize().await;
    }

    // Abort tasks.
    control_handle.abort();
    imaging_handle.abort();
    if let Some(h) = discovery_handle {
        h.abort();
    }

    Ok(())
}
