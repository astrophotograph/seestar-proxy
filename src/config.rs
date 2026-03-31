//! CLI configuration.

use clap::Parser;
use std::net::IpAddr;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "seestar-proxy",
    about = "TCP proxy and multiplexer for Seestar smart telescopes",
    long_about = "Proxies and multiplexes connections between multiple clients and a single Seestar telescope.\n\n\
                  Handles port 4700 (JSON-RPC control) with request/response ID remapping,\n\
                  port 4800 (binary imaging) with fan-out to all clients, and optionally\n\
                  bridges UDP discovery broadcasts."
)]
pub struct Config {
    /// Seestar telescope IP address
    #[arg(long, short = 'u')]
    pub upstream: IpAddr,

    /// Seestar control port
    #[arg(long, default_value = "4700")]
    pub upstream_control_port: u16,

    /// Seestar imaging port
    #[arg(long, default_value = "4800")]
    pub upstream_imaging_port: u16,

    /// Address to bind the proxy on
    #[arg(long, short, default_value = "0.0.0.0")]
    pub bind: IpAddr,

    /// Local control port to listen on
    #[arg(long, default_value = "4700")]
    pub control_port: u16,

    /// Local imaging port to listen on
    #[arg(long, default_value = "4800")]
    pub imaging_port: u16,

    /// Enable discovery bridging (respond to UDP broadcasts on port 4720)
    #[arg(long, short)]
    pub discovery: bool,

    /// Record traffic to a session directory (for mock telescope testing)
    #[arg(long, short)]
    pub record: Option<std::path::PathBuf>,

    /// Enable the web dashboard on the given port (e.g., --dashboard 8080)
    #[arg(long)]
    pub dashboard: Option<u16>,

    /// Raw pipe mode — forward bytes transparently without JSON parsing or
    /// ID remapping. Useful for diagnostics (single client only).
    #[arg(long)]
    pub raw: bool,

    /// Verbose logging
    #[arg(long, short, action = clap::ArgAction::Count)]
    pub verbose: u8,
}
