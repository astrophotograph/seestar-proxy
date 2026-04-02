//! Embedded Tailscale integration (feature-gated).
//!
//! When enabled, the proxy can join a Tailscale network directly,
//! making it accessible from anywhere on the tailnet without separate
//! VPN configuration. Uses libtailscale (C FFI) to embed a Tailscale
//! node in-process.
//!
//! Planned features:
//! - `--tailscale` flag to enable
//! - One-time browser auth to join tailnet
//! - Proxy appears as a Tailscale node (e.g. seestar-proxy.tailnet.ts.net)
//! - MagicDNS for discovery
//! - ACL-based access control via Tailscale admin console
//!
//! Binary size impact: ~15-20 MB (libtailscale embeds a Go runtime).
//! This is why it's feature-gated — the default binary stays small for
//! deployment on the Seestar's own embedded Linux.

// TODO: Add libtailscale dependency and FFI bindings.
// See: https://github.com/tailscale/libtailscale
