//! CLI and config file configuration.
//!
//! Precedence (highest to lowest):
//!   1. CLI flags
//!   2. Config file (TOML, via `--config`)
//!   3. Environment variables (`SEESTAR_*`)
//!   4. Built-in defaults

use clap::Parser;
use serde::Deserialize;
use std::net::IpAddr;
use std::path::PathBuf;
use tracing::info;

/// TOML config file structure. All fields optional — only set fields override defaults.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    pub upstream: Option<String>,
    pub upstream_control_port: Option<u16>,
    pub upstream_imaging_port: Option<u16>,
    pub bind: Option<IpAddr>,
    pub control_port: Option<u16>,
    pub imaging_port: Option<u16>,
    pub discovery: Option<bool>,
    pub record: Option<PathBuf>,
    pub raw: Option<bool>,
    pub transparent: Option<bool>,
    pub dashboard_port: Option<u16>,
    pub hooks: Option<Vec<PathBuf>>,
    pub wireguard: Option<bool>,
    pub wg_port: Option<u16>,
    pub wg_subnet: Option<String>,
    pub wg_key_file: Option<PathBuf>,
    pub wg_endpoint: Option<String>,
    pub verbose: Option<u8>,
}

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
    /// Path to TOML config file
    #[arg(long, short = 'c', env = "SEESTAR_CONFIG")]
    pub config: Option<PathBuf>,

    /// Seestar telescope IP address or hostname (default: resolves seestar.local)
    #[arg(long, short = 'u', env = "SEESTAR_UPSTREAM")]
    pub upstream: Option<String>,

    /// Seestar control port
    #[arg(long, default_value = "4700", env = "SEESTAR_UPSTREAM_CONTROL_PORT")]
    pub upstream_control_port: u16,

    /// Seestar imaging port
    #[arg(long, default_value = "4800", env = "SEESTAR_UPSTREAM_IMAGING_PORT")]
    pub upstream_imaging_port: u16,

    /// Address to bind the proxy on
    #[arg(long, short, default_value = "0.0.0.0", env = "SEESTAR_BIND")]
    pub bind: IpAddr,

    /// Local control port to listen on
    #[arg(long, default_value = "4700", env = "SEESTAR_CONTROL_PORT")]
    pub control_port: u16,

    /// Local imaging port to listen on
    #[arg(long, default_value = "4800", env = "SEESTAR_IMAGING_PORT")]
    pub imaging_port: u16,

    /// Enable discovery bridging (respond to UDP broadcasts on port 4720)
    #[arg(long, short, env = "SEESTAR_DISCOVERY")]
    pub discovery: bool,

    /// Record traffic to a session directory (for mock telescope testing)
    #[arg(long, short, env = "SEESTAR_RECORD")]
    pub record: Option<PathBuf>,

    /// Raw pipe mode — forward bytes transparently without JSON parsing or
    /// ID remapping. Useful for diagnostics (single client only).
    #[arg(long)]
    pub raw: bool,

    /// Transparent proxy mode — resolve upstream address from iptables REDIRECT
    /// via SO_ORIGINAL_DST instead of --upstream. Linux only.
    #[arg(long, env = "SEESTAR_TRANSPARENT")]
    pub transparent: bool,

    /// Sync system clock via NTP before starting (useful for headless Pi without RTC)
    #[cfg(feature = "ntp")]
    #[arg(long, env = "SEESTAR_NTP_SYNC")]
    pub ntp_sync: bool,

    /// NTP server to query
    #[cfg(feature = "ntp")]
    #[arg(long, default_value = "pool.ntp.org", env = "SEESTAR_NTP_SERVER")]
    pub ntp_server: String,

    /// Enable embedded Tailscale node (requires tailscale feature)
    #[cfg(feature = "tailscale")]
    #[arg(long, env = "SEESTAR_TAILSCALE")]
    pub tailscale: bool,

    /// Tailscale hostname (how this node appears on the tailnet)
    #[cfg(feature = "tailscale")]
    #[arg(long, default_value = "seestar-proxy", env = "SEESTAR_TS_HOSTNAME")]
    pub ts_hostname: String,

    /// Tailscale auth key (for headless/automated setup; omit for interactive browser auth)
    #[cfg(feature = "tailscale")]
    #[arg(long, env = "SEESTAR_TS_AUTHKEY")]
    pub ts_authkey: Option<String>,

    /// Tailscale state directory
    #[cfg(feature = "tailscale")]
    #[arg(
        long,
        default_value = "/var/lib/seestar-proxy/tailscale",
        env = "SEESTAR_TS_STATE_DIR"
    )]
    pub ts_state_dir: PathBuf,

    /// Tailscale control server URL (for Headscale users)
    #[cfg(feature = "tailscale")]
    #[arg(long, env = "SEESTAR_TS_CONTROL_URL")]
    pub ts_control_url: Option<String>,

    /// HTTP dashboard port (0 = disable)
    #[arg(long, default_value = "4090", env = "SEESTAR_DASHBOARD_PORT")]
    pub dashboard_port: u16,

    /// Lua hook script (can be specified multiple times)
    #[arg(long = "hook")]
    pub hooks: Vec<PathBuf>,

    /// Enable WireGuard tunnel endpoint for cross-subnet access
    #[arg(long, env = "SEESTAR_WIREGUARD")]
    pub wireguard: bool,

    /// WireGuard UDP listen port
    #[arg(long, default_value = "51820", env = "SEESTAR_WG_PORT")]
    pub wg_port: u16,

    /// WireGuard tunnel subnet (server gets .1, first client gets .2)
    #[arg(long, default_value = "10.99.0.0/24", env = "SEESTAR_WG_SUBNET")]
    pub wg_subnet: String,

    /// WireGuard key file path
    #[arg(
        long,
        default_value = "~/.seestar-proxy/wg.key",
        env = "SEESTAR_WG_KEY_FILE"
    )]
    pub wg_key_file: PathBuf,

    /// External endpoint for WireGuard client config (e.g., mypi.duckdns.org:51820).
    /// Auto-detected if not specified.
    #[arg(long, env = "SEESTAR_WG_ENDPOINT")]
    pub wg_endpoint: Option<String>,

    /// Verbose logging
    #[arg(long, short, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

impl Config {
    /// Parse CLI args, then layer config file values underneath for any
    /// fields that weren't explicitly set on the command line.
    pub fn load() -> anyhow::Result<Self> {
        let mut config = Config::parse();

        if let Some(path) = config.config.clone() {
            let content = std::fs::read_to_string(&path).map_err(|e| {
                anyhow::anyhow!("Failed to read config file {}: {}", path.display(), e)
            })?;
            let file: FileConfig = toml::from_str(&content).map_err(|e| {
                anyhow::anyhow!("Failed to parse config file {}: {}", path.display(), e)
            })?;
            config.apply_file(file);
            info!("Loaded config from {}", path.display());
        } else {
            // Try default locations.
            for candidate in &[
                PathBuf::from("/etc/seestar-proxy/config.toml"),
                dirs_or_home(".config/seestar-proxy/config.toml"),
            ] {
                if candidate.exists()
                    && let Ok(content) = std::fs::read_to_string(candidate)
                    && let Ok(file) = toml::from_str::<FileConfig>(&content)
                {
                    config.apply_file(file);
                    info!("Loaded config from {}", candidate.display());
                    break;
                }
            }
        }

        Ok(config)
    }

    /// Apply file config values as defaults — only fills in fields that
    /// still have their built-in default (i.e. weren't set by CLI or env).
    fn apply_file(&mut self, file: FileConfig) {
        // For Option fields, only fill if CLI left them as None.
        if self.upstream.is_none() {
            self.upstream = file.upstream;
        }
        if self.wg_endpoint.is_none() {
            self.wg_endpoint = file.wg_endpoint;
        }
        if self.record.is_none() {
            self.record = file.record;
        }

        // For fields with defaults, we can't perfectly distinguish "user passed
        // the default value" from "wasn't set", but file config values for these
        // are useful mainly when CLI isn't used at all (e.g. systemd service).
        // The env var layer (clap `env`) handles the middle ground.
        macro_rules! apply_default {
            ($field:ident, $file_field:expr, $default:expr) => {
                if let Some(v) = $file_field {
                    if self.$field == $default {
                        self.$field = v;
                    }
                }
            };
        }

        apply_default!(upstream_control_port, file.upstream_control_port, 4700);
        apply_default!(upstream_imaging_port, file.upstream_imaging_port, 4800);
        apply_default!(control_port, file.control_port, 4700);
        apply_default!(imaging_port, file.imaging_port, 4800);
        apply_default!(dashboard_port, file.dashboard_port, 4090);
        apply_default!(wg_port, file.wg_port, 51820);

        if let Some(bind) = file.bind {
            let default_bind: IpAddr = [0, 0, 0, 0].into();
            if self.bind == default_bind {
                self.bind = bind;
            }
        }
        if let Some(subnet) = file.wg_subnet
            && self.wg_subnet == "10.99.0.0/24"
        {
            self.wg_subnet = subnet;
        }
        if let Some(key_file) = file.wg_key_file {
            let default_key: PathBuf = "~/.seestar-proxy/wg.key".into();
            if self.wg_key_file == default_key {
                self.wg_key_file = key_file;
            }
        }

        // Bool flags: file can enable them (CLI --flag always wins since it
        // can only set true, never explicitly set false).
        if let Some(true) = file.discovery {
            self.discovery = true;
        }
        if let Some(true) = file.wireguard {
            self.wireguard = true;
        }
        if let Some(true) = file.raw {
            self.raw = true;
        }
        if let Some(true) = file.transparent {
            self.transparent = true;
        }

        // Hooks: append file hooks to CLI hooks.
        if let Some(file_hooks) = file.hooks {
            for h in file_hooks {
                if !self.hooks.contains(&h) {
                    self.hooks.push(h);
                }
            }
        }

        // Verbose: take the higher of CLI and file.
        if let Some(v) = file.verbose
            && v > self.verbose
        {
            self.verbose = v;
        }
    }
}

/// Build a path under $HOME, falling back to a reasonable default.
fn dirs_or_home(relative: &str) -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(relative)
    } else {
        PathBuf::from("/tmp").join(relative)
    }
}
