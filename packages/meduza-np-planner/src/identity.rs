//! Node machine identity (D3): an Ed25519 key generated on the node; the private key never leaves it.

use std::io;
use std::path::Path;

use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use meduza_np_runtime::fsutil;

pub struct Identity {
    key: SigningKey,
}

const KEY_FILE: &str = "node.ed25519";
const ROTATE_FILE: &str = "node.ed25519.next";

impl Identity {
    /// Loads the key, creating one on first use (0600 in a 0700 directory).
    pub fn load_or_create(dir: &Path) -> io::Result<Self> {
        fsutil::ensure_dir(dir, 0o700)?;
        let path = dir.join(KEY_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => Self::from_bytes(&bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let id = Self::generate()?;
                fsutil::write_atomic(&path, &id.key.to_bytes(), 0o600)?;
                Ok(id)
            }
            Err(e) => Err(e),
        }
    }

    /// Replaces the identity (re-enrollment). The previous key is overwritten atomically.
    pub fn regenerate(dir: &Path) -> io::Result<Self> {
        fsutil::ensure_dir(dir, 0o700)?;
        let id = Self::generate()?;
        fsutil::write_atomic(&dir.join(KEY_FILE), &id.key.to_bytes(), 0o600)?;
        let _ = std::fs::remove_file(dir.join(ROTATE_FILE));
        Ok(id)
    }

    fn generate() -> io::Result<Self> {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).map_err(|e| io::Error::other(e.to_string()))?;
        Ok(Self { key: SigningKey::from_bytes(&seed) })
    }

    fn from_bytes(bytes: &[u8]) -> io::Result<Self> {
        let seed: [u8; 32] = bytes.try_into().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "node key must be 32 bytes"))?;
        Ok(Self { key: SigningKey::from_bytes(&seed) })
    }

    /// Standard base64 of the 32-byte public key, as sent at enrollment.
    pub fn public_key_b64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.key.verifying_key().to_bytes())
    }

    pub fn sign_b64(&self, data: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.key.sign(data).to_bytes())
    }

    #[cfg(test)]
    pub fn for_tests(seed: [u8; 32]) -> Self {
        Self { key: SigningKey::from_bytes(&seed) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    #[test]
    fn sign_and_persist() {
        let dir = std::env::temp_dir().join(format!("mnp-identity-{}", std::process::id()));
        let a = Identity::load_or_create(&dir).unwrap();
        let b = Identity::load_or_create(&dir).unwrap();
        assert_eq!(a.public_key_b64(), b.public_key_b64(), "persisted");
        let input = meduza_np_protocol::api::session_signing_input("gw-a", "nonce");
        let sig = base64::engine::general_purpose::STANDARD.decode(a.sign_b64(&input)).unwrap();
        let pk = base64::engine::general_purpose::STANDARD.decode(a.public_key_b64()).unwrap();
        let vk = VerifyingKey::from_bytes(&pk.try_into().unwrap()).unwrap();
        vk.verify(&input, &Signature::from_slice(&sig).unwrap()).unwrap();
        let c = Identity::regenerate(&dir).unwrap();
        assert_ne!(a.public_key_b64(), c.public_key_b64());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
