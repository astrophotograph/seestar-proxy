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
    pub telescope_sn: Option<String>,
    pub telescope_model: Option<String>,
    pub telescope_bssid: Option<String>,
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

    /// Telescope serial number for discovery responses (skips UDP probe).
    /// Find it in the ZWO Seestar app under device info, e.g. "SS50A1234567".
    #[arg(long, env = "SEESTAR_TELESCOPE_SN")]
    pub telescope_sn: Option<String>,

    /// Telescope product model for discovery responses (default: "Seestar S50").
    /// Used together with --telescope-sn to skip the UDP discovery probe.
    #[arg(long, env = "SEESTAR_TELESCOPE_MODEL")]
    pub telescope_model: Option<String>,

    /// Telescope WiFi BSSID for discovery responses (e.g. "c2:f5:35:2f:17:26").
    /// Required for the iOS app to connect via home network rather than AP-switch.
    #[arg(long, env = "SEESTAR_TELESCOPE_BSSID")]
    pub telescope_bssid: Option<String>,
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
        if self.telescope_sn.is_none() {
            self.telescope_sn = file.telescope_sn;
        }
        if self.telescope_model.is_none() {
            self.telescope_model = file.telescope_model;
        }
        if self.telescope_bssid.is_none() {
            self.telescope_bssid = file.telescope_bssid;
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn default_config() -> Config {
        Config::parse_from(["seestar-proxy"])
    }

    // ── FileConfig ────────────────────────────────────────────────────────────

    #[test]
    fn file_config_default_all_none() {
        let fc = FileConfig::default();
        assert!(fc.upstream.is_none());
        assert!(fc.upstream_control_port.is_none());
        assert!(fc.control_port.is_none());
        assert!(fc.discovery.is_none());
        assert!(fc.hooks.is_none());
        assert!(fc.verbose.is_none());
    }

    #[test]
    fn file_config_deserializes_from_toml() {
        let toml = r#"
            upstream = "192.168.1.50"
            control_port = 9000
            discovery = true
            verbose = 2
        "#;
        let fc: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(fc.upstream.as_deref(), Some("192.168.1.50"));
        assert_eq!(fc.control_port, Some(9000));
        assert_eq!(fc.discovery, Some(true));
        assert_eq!(fc.verbose, Some(2));
    }

    #[test]
    fn file_config_deserializes_hooks_as_paths() {
        let toml = r#"hooks = ["/a.lua", "/b.lua"]"#;
        let fc: FileConfig = toml::from_str(toml).unwrap();
        let hooks = fc.hooks.unwrap();
        assert_eq!(hooks.len(), 2);
        assert_eq!(hooks[0], PathBuf::from("/a.lua"));
    }

    #[test]
    fn file_config_empty_toml_gives_all_none() {
        let fc: FileConfig = toml::from_str("").unwrap();
        assert!(fc.upstream.is_none());
        assert!(fc.control_port.is_none());
    }

    // ── apply_file: Option fields ─────────────────────────────────────────────

    #[test]
    fn apply_file_fills_none_upstream() {
        let mut cfg = default_config();
        assert!(cfg.upstream.is_none());
        cfg.apply_file(FileConfig {
            upstream: Some("192.168.1.100".to_string()),
            ..Default::default()
        });
        assert_eq!(cfg.upstream.as_deref(), Some("192.168.1.100"));
    }

    #[test]
    fn apply_file_does_not_overwrite_set_upstream() {
        let mut cfg = Config::parse_from(["seestar-proxy", "--upstream", "cli-host"]);
        cfg.apply_file(FileConfig {
            upstream: Some("file-host".to_string()),
            ..Default::default()
        });
        assert_eq!(cfg.upstream.as_deref(), Some("cli-host"));
    }

    #[test]
    fn apply_file_fills_none_wg_endpoint() {
        let mut cfg = default_config();
        cfg.apply_file(FileConfig {
            wg_endpoint: Some("mypi.example.com:51820".to_string()),
            ..Default::default()
        });
        assert_eq!(cfg.wg_endpoint.as_deref(), Some("mypi.example.com:51820"));
    }

    #[test]
    fn apply_file_does_not_overwrite_set_wg_endpoint() {
        let mut cfg =
            Config::parse_from(["seestar-proxy", "--wg-endpoint", "cli.example.com:51820"]);
        cfg.apply_file(FileConfig {
            wg_endpoint: Some("file.example.com:51820".to_string()),
            ..Default::default()
        });
        assert_eq!(cfg.wg_endpoint.as_deref(), Some("cli.example.com:51820"));
    }

    #[test]
    fn apply_file_fills_none_record() {
        let mut cfg = default_config();
        cfg.apply_file(FileConfig {
            record: Some("/tmp/sessions".into()),
            ..Default::default()
        });
        assert_eq!(cfg.record, Some(PathBuf::from("/tmp/sessions")));
    }

    #[test]
    fn apply_file_fills_none_telescope_sn() {
        let mut cfg = default_config();
        cfg.apply_file(FileConfig {
            telescope_sn: Some("4ddb0535".to_string()),
            ..Default::default()
        });
        assert_eq!(cfg.telescope_sn.as_deref(), Some("4ddb0535"));
    }

    #[test]
    fn apply_file_does_not_overwrite_set_telescope_sn() {
        let mut cfg = Config::parse_from(["seestar-proxy", "--telescope-sn", "cli-sn"]);
        cfg.apply_file(FileConfig {
            telescope_sn: Some("file-sn".to_string()),
            ..Default::default()
        });
        assert_eq!(cfg.telescope_sn.as_deref(), Some("cli-sn"));
    }

    #[test]
    fn apply_file_fills_none_telescope_model() {
        let mut cfg = default_config();
        cfg.apply_file(FileConfig {
            telescope_model: Some("Seestar S30".to_string()),
            ..Default::default()
        });
        assert_eq!(cfg.telescope_model.as_deref(), Some("Seestar S30"));
    }

    #[test]
    fn apply_file_does_not_overwrite_set_telescope_model() {
        let mut cfg = Config::parse_from(["seestar-proxy", "--telescope-model", "cli-model"]);
        cfg.apply_file(FileConfig {
            telescope_model: Some("file-model".to_string()),
            ..Default::default()
        });
        assert_eq!(cfg.telescope_model.as_deref(), Some("cli-model"));
    }

    #[test]
    fn apply_file_fills_none_telescope_bssid() {
        let mut cfg = default_config();
        cfg.apply_file(FileConfig {
            telescope_bssid: Some("aa:bb:cc:dd:ee:ff".to_string()),
            ..Default::default()
        });
        assert_eq!(cfg.telescope_bssid.as_deref(), Some("aa:bb:cc:dd:ee:ff"));
    }

    #[test]
    fn apply_file_does_not_overwrite_set_telescope_bssid() {
        let mut cfg =
            Config::parse_from(["seestar-proxy", "--telescope-bssid", "11:22:33:44:55:66"]);
        cfg.apply_file(FileConfig {
            telescope_bssid: Some("aa:bb:cc:dd:ee:ff".to_string()),
            ..Default::default()
        });
        assert_eq!(cfg.telescope_bssid.as_deref(), Some("11:22:33:44:55:66"));
    }

    // ── apply_file: default-value ports ──────────────────────────────────────

    #[test]
    fn apply_file_overrides_default_ports() {
        let mut cfg = default_config();
        cfg.apply_file(FileConfig {
            upstream_control_port: Some(9901),
            upstream_imaging_port: Some(9902),
            control_port: Some(9903),
            imaging_port: Some(9904),
            dashboard_port: Some(9905),
            wg_port: Some(9906),
            ..Default::default()
        });
        assert_eq!(cfg.upstream_control_port, 9901);
        assert_eq!(cfg.upstream_imaging_port, 9902);
        assert_eq!(cfg.control_port, 9903);
        assert_eq!(cfg.imaging_port, 9904);
        assert_eq!(cfg.dashboard_port, 9905);
        assert_eq!(cfg.wg_port, 9906);
    }

    #[test]
    fn apply_file_does_not_override_non_default_control_port() {
        let mut cfg = Config::parse_from(["seestar-proxy", "--control-port", "9000"]);
        cfg.apply_file(FileConfig {
            control_port: Some(1234),
            ..Default::default()
        });
        assert_eq!(cfg.control_port, 9000);
    }

    #[test]
    fn apply_file_does_not_override_non_default_imaging_port() {
        let mut cfg = Config::parse_from(["seestar-proxy", "--imaging-port", "9001"]);
        cfg.apply_file(FileConfig {
            imaging_port: Some(1234),
            ..Default::default()
        });
        assert_eq!(cfg.imaging_port, 9001);
    }

    // ── apply_file: bind address ──────────────────────────────────────────────

    #[test]
    fn apply_file_overrides_default_bind() {
        let mut cfg = default_config();
        cfg.apply_file(FileConfig {
            bind: Some("127.0.0.1".parse().unwrap()),
            ..Default::default()
        });
        assert_eq!(cfg.bind, "127.0.0.1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn apply_file_does_not_override_non_default_bind() {
        let mut cfg = Config::parse_from(["seestar-proxy", "--bind", "10.0.0.1"]);
        cfg.apply_file(FileConfig {
            bind: Some("127.0.0.1".parse().unwrap()),
            ..Default::default()
        });
        assert_eq!(cfg.bind, "10.0.0.1".parse::<IpAddr>().unwrap());
    }

    // ── apply_file: wg_subnet ─────────────────────────────────────────────────

    #[test]
    fn apply_file_overrides_default_wg_subnet() {
        let mut cfg = default_config();
        cfg.apply_file(FileConfig {
            wg_subnet: Some("10.1.0.0/24".to_string()),
            ..Default::default()
        });
        assert_eq!(cfg.wg_subnet, "10.1.0.0/24");
    }

    #[test]
    fn apply_file_does_not_override_non_default_wg_subnet() {
        let mut cfg = Config::parse_from(["seestar-proxy", "--wg-subnet", "172.16.0.0/24"]);
        cfg.apply_file(FileConfig {
            wg_subnet: Some("10.1.0.0/24".to_string()),
            ..Default::default()
        });
        assert_eq!(cfg.wg_subnet, "172.16.0.0/24");
    }

    // ── apply_file: wg_key_file ───────────────────────────────────────────────

    #[test]
    fn apply_file_overrides_default_wg_key_file() {
        let mut cfg = default_config();
        cfg.apply_file(FileConfig {
            wg_key_file: Some("/custom/wg.key".into()),
            ..Default::default()
        });
        assert_eq!(cfg.wg_key_file, PathBuf::from("/custom/wg.key"));
    }

    #[test]
    fn apply_file_does_not_override_non_default_wg_key_file() {
        let mut cfg = Config::parse_from(["seestar-proxy", "--wg-key-file", "/my/wg.key"]);
        cfg.apply_file(FileConfig {
            wg_key_file: Some("/other/wg.key".into()),
            ..Default::default()
        });
        assert_eq!(cfg.wg_key_file, PathBuf::from("/my/wg.key"));
    }

    // ── apply_file: bool flags ────────────────────────────────────────────────

    #[test]
    fn apply_file_enables_bool_flags_from_file() {
        let mut cfg = default_config();
        assert!(!cfg.discovery);
        assert!(!cfg.wireguard);
        assert!(!cfg.raw);
        assert!(!cfg.transparent);
        cfg.apply_file(FileConfig {
            discovery: Some(true),
            wireguard: Some(true),
            raw: Some(true),
            transparent: Some(true),
            ..Default::default()
        });
        assert!(cfg.discovery);
        assert!(cfg.wireguard);
        assert!(cfg.raw);
        assert!(cfg.transparent);
    }

    #[test]
    fn apply_file_false_does_not_disable_cli_enabled_flag() {
        let mut cfg = Config::parse_from(["seestar-proxy", "--discovery"]);
        assert!(cfg.discovery);
        cfg.apply_file(FileConfig {
            discovery: Some(false),
            ..Default::default()
        });
        assert!(
            cfg.discovery,
            "CLI --discovery flag must not be overridden by file false"
        );
    }

    #[test]
    fn apply_file_none_does_not_change_bool_flags() {
        let mut cfg = Config::parse_from(["seestar-proxy", "--raw"]);
        cfg.apply_file(FileConfig::default());
        assert!(cfg.raw, "unset file field must not disable CLI --raw");
    }

    // ── apply_file: hooks ─────────────────────────────────────────────────────

    #[test]
    fn apply_file_appends_file_hooks_to_cli_hooks() {
        let mut cfg = Config::parse_from(["seestar-proxy", "--hook", "/a.lua"]);
        cfg.apply_file(FileConfig {
            hooks: Some(vec!["/b.lua".into(), "/c.lua".into()]),
            ..Default::default()
        });
        assert_eq!(cfg.hooks.len(), 3);
        assert!(cfg.hooks.contains(&PathBuf::from("/a.lua")));
        assert!(cfg.hooks.contains(&PathBuf::from("/b.lua")));
        assert!(cfg.hooks.contains(&PathBuf::from("/c.lua")));
    }

    #[test]
    fn apply_file_does_not_duplicate_hooks_already_in_cli() {
        let mut cfg = Config::parse_from(["seestar-proxy", "--hook", "/a.lua"]);
        cfg.apply_file(FileConfig {
            hooks: Some(vec!["/a.lua".into(), "/b.lua".into()]),
            ..Default::default()
        });
        assert_eq!(
            cfg.hooks.len(),
            2,
            "duplicate hook /a.lua must not be added twice"
        );
    }

    #[test]
    fn apply_file_hooks_none_leaves_cli_hooks_unchanged() {
        let mut cfg = Config::parse_from(["seestar-proxy", "--hook", "/a.lua"]);
        cfg.apply_file(FileConfig::default());
        assert_eq!(cfg.hooks, vec![PathBuf::from("/a.lua")]);
    }

    // ── apply_file: verbose ───────────────────────────────────────────────────

    #[test]
    fn apply_file_verbose_takes_file_value_when_cli_is_zero() {
        let mut cfg = default_config();
        assert_eq!(cfg.verbose, 0);
        cfg.apply_file(FileConfig {
            verbose: Some(2),
            ..Default::default()
        });
        assert_eq!(cfg.verbose, 2);
    }

    #[test]
    fn apply_file_verbose_keeps_cli_value_when_higher() {
        let mut cfg = Config::parse_from(["seestar-proxy", "-vvv"]);
        assert_eq!(cfg.verbose, 3);
        cfg.apply_file(FileConfig {
            verbose: Some(1),
            ..Default::default()
        });
        assert_eq!(cfg.verbose, 3);
    }

    #[test]
    fn apply_file_verbose_none_leaves_cli_unchanged() {
        let mut cfg = Config::parse_from(["seestar-proxy", "-v"]);
        cfg.apply_file(FileConfig::default());
        assert_eq!(cfg.verbose, 1);
    }

    // ── dirs_or_home ──────────────────────────────────────────────────────────
    // Env-var tests must be serialized: mutating HOME from parallel threads is
    // a data race. Use a process-wide mutex so the two tests don't interfere.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn dirs_or_home_uses_home_env_var() {
        let _guard = HOME_LOCK.lock().unwrap();
        let orig = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", "/custom/home") };
        let p = dirs_or_home("foo/bar.toml");
        match orig {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        assert_eq!(p, PathBuf::from("/custom/home/foo/bar.toml"));
    }

    #[test]
    fn dirs_or_home_falls_back_to_tmp_when_home_missing() {
        let _guard = HOME_LOCK.lock().unwrap();
        let orig = std::env::var("HOME").ok();
        unsafe { std::env::remove_var("HOME") };
        let p = dirs_or_home("sub/file.toml");
        if let Some(v) = orig {
            unsafe { std::env::set_var("HOME", v) };
        }
        assert_eq!(p, PathBuf::from("/tmp/sub/file.toml"));
    }
}
