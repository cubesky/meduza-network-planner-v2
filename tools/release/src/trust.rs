//! Prepare the public trust roots before fingerprints and packaging; never write the signing seed to disk.

use std::path::Path;

use meduza_np_protocol::group::{TrustStore, GROUP_FORMAT_VERSION};
use meduza_np_updater::sig;

pub fn prepare(secret: &str, destination: &Path, now: u64) -> Result<String, String> {
    if secret.trim().is_empty() {
        return Err("MEDUZA_NP_RELEASE_KEY is missing; configure the release signing secret before tagging".into());
    }
    let signer = sig::decode_secret(secret)?;
    let exists = destination.exists();
    let trust: TrustStore = if exists {
        let bytes = std::fs::read(destination).map_err(|e| format!("{}: {e}", destination.display()))?;
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", destination.display()))?
    } else {
        TrustStore { format_version: GROUP_FORMAT_VERSION, threshold: 1, keys: vec![sig::trusted_key(&signer.verifying_key(), None)] }
    };
    if trust.format_version != GROUP_FORMAT_VERSION {
        return Err(format!("unsupported trust store format {}", trust.format_version));
    }
    let probe = b"Meduza NP release signing preflight";
    let signature = serde_json::to_vec(&sig::sign(probe, std::slice::from_ref(&signer))).map_err(|e| e.to_string())?;
    let ids = sig::verify(probe, &signature, &trust, now)
        .map_err(|e| format!("release signing secret does not satisfy {}: {e}", destination.display()))?;
    if !exists {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut bytes = serde_json::to_vec_pretty(&trust).map_err(|e| e.to_string())?;
        bytes.push(b'\n');
        std::fs::write(destination, bytes).map_err(|e| format!("{}: {e}", destination.display()))?;
    }
    Ok(ids.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_public_roots_preserves_existing_files_and_refuses_incompatible_signers() {
        let dir = std::env::temp_dir().join(format!("mnp-release-trust-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trust/release-keys.json");
        let signer = sig::generate_key().unwrap();
        let secret = sig::encode_secret(&signer);
        prepare(&secret, &path, 0).unwrap();
        let original = std::fs::read(&path).unwrap();
        let store: TrustStore = serde_json::from_slice(&original).unwrap();
        assert_eq!(store.keys, vec![sig::trusted_key(&signer.verifying_key(), None)]);
        assert!(!String::from_utf8_lossy(&original).contains(&secret));
        prepare(&secret, &path, 0).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let other = sig::encode_secret(&sig::generate_key().unwrap());
        assert!(prepare(&other, &path, 0).unwrap_err().contains("does not satisfy"));
        assert_eq!(std::fs::read(&path).unwrap(), original);

        for invalid in ["", "not-a-signing-seed"] {
            let absent = dir.join("absent.json");
            assert!(prepare(invalid, &absent, 0).is_err());
            assert!(!absent.exists());
        }
        let mut revoked = store.clone();
        revoked.keys[0].revoked = true;
        std::fs::write(&path, serde_json::to_vec(&revoked).unwrap()).unwrap();
        assert!(prepare(&secret, &path, 0).is_err());
        let mut expired = store.clone();
        expired.keys[0].not_after = Some("1970-01-01T00:00:00Z".into());
        std::fs::write(&path, serde_json::to_vec(&expired).unwrap()).unwrap();
        assert!(prepare(&secret, &path, 1).is_err());
        let mut threshold = store;
        threshold.threshold = 2;
        std::fs::write(&path, serde_json::to_vec(&threshold).unwrap()).unwrap();
        assert!(prepare(&secret, &path, 0).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
