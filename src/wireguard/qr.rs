//! QR code generation for WireGuard client configuration.

use qrcode::QrCode;
use qrcode::render::svg;

/// Generate a WireGuard client configuration string.
pub fn client_config(
    client_private_key: &str,
    client_address: &str,
    server_public_key: &str,
    endpoint: &str,
    allowed_ips: &str,
) -> String {
    format!(
        "[Interface]\n\
         PrivateKey = {}\n\
         Address = {}\n\
         DNS = 1.1.1.1\n\
         \n\
         [Peer]\n\
         PublicKey = {}\n\
         Endpoint = {}\n\
         AllowedIPs = {}\n\
         PersistentKeepalive = 25\n",
        client_private_key, client_address, server_public_key, endpoint, allowed_ips,
    )
}

/// Render a WireGuard config as an SVG QR code.
pub fn config_to_svg(config: &str) -> anyhow::Result<String> {
    let code = QrCode::new(config.as_bytes())?;
    let svg = code
        .render::<svg::Color>()
        .min_dimensions(200, 200)
        .dark_color(svg::Color("#e6edf3"))
        .light_color(svg::Color("#0d1117"))
        .quiet_zone(true)
        .build();
    Ok(svg)
}

/// Render a WireGuard config as Unicode block characters (for terminal display).
pub fn config_to_terminal(config: &str) -> anyhow::Result<String> {
    let code = QrCode::new(config.as_bytes())?;
    let string = code
        .render::<char>()
        .quiet_zone(true)
        .module_dimensions(2, 1)
        .dark_color('#')
        .light_color(' ')
        .build();
    Ok(string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_config_has_all_fields() {
        let config = client_config("PRIVKEY", "10.99.0.2/24", "PUBKEY", "1.2.3.4:51820", "10.0.0.0/24");
        assert!(config.contains("PrivateKey = PRIVKEY"));
        assert!(config.contains("Address = 10.99.0.2/24"));
        assert!(config.contains("PublicKey = PUBKEY"));
        assert!(config.contains("Endpoint = 1.2.3.4:51820"));
        assert!(config.contains("AllowedIPs = 10.0.0.0/24"));
        assert!(config.contains("PersistentKeepalive = 25"));
    }

    #[test]
    fn config_to_svg_produces_valid_svg() {
        let config = client_config("A", "B", "C", "D", "E");
        let svg = config_to_svg(&config).unwrap();
        assert!(svg.contains("<svg"));
        assert!(svg.contains("</svg>"));
    }

    #[test]
    fn config_to_terminal_produces_output() {
        let config = client_config("A", "B", "C", "D", "E");
        let term = config_to_terminal(&config).unwrap();
        assert!(!term.is_empty());
        assert!(term.contains('#')); // Dark modules
    }
}
