//! WireGuard key management — generate, persist, and load x25519 keypairs.

use base64::prelude::*;
use rand_core::OsRng;
use std::path::Path;
use tracing::info;
use x25519_dalek::{PublicKey, StaticSecret};

/// A WireGuard keypair (private + derived public key).
pub struct WgKeypair {
    pub private: StaticSecret,
    pub public: PublicKey,
}

impl WgKeypair {
    /// Generate a new random keypair.
    pub fn generate() -> Self {
        let private = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&private);
        Self { private, public }
    }

    /// Load a keypair from a key file, or generate and save if it doesn't exist.
    pub fn load_or_generate(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            let contents = std::fs::read_to_string(path)?;
            let decoded = BASE64_STANDARD.decode(contents.trim())?;
            if decoded.len() != 32 {
                anyhow::bail!("Invalid key file: expected 32 bytes, got {}", decoded.len());
            }
            let mut key_bytes = [0u8; 32];
            key_bytes.copy_from_slice(&decoded);
            let private = StaticSecret::from(key_bytes);
            let public = PublicKey::from(&private);
            info!("Loaded WireGuard key from {}", path.display());
            Ok(Self { private, public })
        } else {
            let keypair = Self::generate();
            // Create parent directory if needed.
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let encoded = BASE64_STANDARD.encode(keypair.private.as_bytes());
            std::fs::write(path, format!("{}\n", encoded))?;
            // Restrict permissions on Unix.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
            info!("Generated new WireGuard key at {}", path.display());
            Ok(keypair)
        }
    }

    /// Base64-encoded public key (for config files and display).
    pub fn public_key_b64(&self) -> String {
        BASE64_STANDARD.encode(self.public.as_bytes())
    }
}

/// Generate a client keypair (ephemeral, included in the QR config).
pub fn generate_client_keypair() -> (StaticSecret, PublicKey) {
    let private = StaticSecret::random_from_rng(OsRng);
    let public = PublicKey::from(&private);
    (private, public)
}

/// Format a private key as base64.
pub fn private_key_b64(key: &StaticSecret) -> String {
    BASE64_STANDARD.encode(key.as_bytes())
}

/// Format a public key as base64.
pub fn public_key_b64(key: &PublicKey) -> String {
    BASE64_STANDARD.encode(key.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn generate_produces_valid_keypair() {
        let kp = WgKeypair::generate();
        assert_eq!(kp.public, PublicKey::from(&kp.private));
    }

    #[test]
    fn load_or_generate_creates_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wg.key");
        assert!(!path.exists());

        let kp = WgKeypair::load_or_generate(&path).unwrap();
        assert!(path.exists());

        // Load again — should get the same key.
        let kp2 = WgKeypair::load_or_generate(&path).unwrap();
        assert_eq!(kp.public, kp2.public);
    }

    #[test]
    fn public_key_b64_roundtrips() {
        let kp = WgKeypair::generate();
        let b64 = kp.public_key_b64();
        let decoded = BASE64_STANDARD.decode(&b64).unwrap();
        assert_eq!(decoded.len(), 32);
        assert_eq!(&decoded[..], kp.public.as_bytes());
    }
}
