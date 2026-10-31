//! Release signatures (§13.4): Ed25519 over the exact bytes served, `<file>.sig` = [`SignatureFile`], trust roots
//! = [`TrustStore`]. Verification is pure Rust so nodes need no signing CLI or container tooling.

use std::collections::BTreeSet;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ed25519_dalek::{Signature as EdSignature, Signer, SigningKey, VerifyingKey};
use meduza_np_protocol::group::{key_id, Signature, SignatureFile, TrustStore, TrustedKey, GROUP_FORMAT_VERSION};

pub const MAX_SIG_BYTES: u64 = 16 * 1024;

/// Verifies `sig_file` over `bytes`. Returns the ids of the trusted keys whose signatures are valid, or why the
/// file is not trusted. Revoked keys and keys past `notAfter` never count.
pub fn verify(bytes: &[u8], sig_file: &[u8], trust: &TrustStore, now_unix: u64) -> Result<Vec<String>, String> {
    let sf: SignatureFile = serde_json::from_slice(sig_file).map_err(|e| format!("signature file unreadable: {e}"))?;
    if sf.format_version != GROUP_FORMAT_VERSION {
        return Err(format!("unsupported signature format {}", sf.format_version));
    }
    let mut valid = BTreeSet::new();
    let mut seen_untrusted = Vec::new();
    for s in &sf.signatures {
        if s.algorithm != "ed25519" {
            continue;
        }
        let Some(k) = trust.keys.iter().find(|k| k.key_id == s.key_id && k.algorithm == "ed25519") else {
            seen_untrusted.push(s.key_id.clone());
            continue;
        };
        if k.revoked {
            continue;
        }
        if let Some(na) = &k.not_after {
            match meduza_np_plugin_sdk::rfc3339_to_unix(na) {
                Some(t) if now_unix <= t => {}
                _ => continue,
            }
        }
        let Ok(vk) = verifying_key(k) else { continue };
        let Ok(raw) = B64.decode(s.signature.as_bytes()) else { continue };
        let Ok(raw): Result<[u8; 64], _> = raw.try_into() else { continue };
        if vk.verify_strict(bytes, &EdSignature::from_bytes(&raw)).is_ok() {
            valid.insert(k.key_id.clone());
        }
    }
    let need = trust.threshold.max(1) as usize;
    if valid.len() >= need {
        Ok(valid.into_iter().collect())
    } else if valid.is_empty() && !seen_untrusted.is_empty() {
        Err(format!("signed only by untrusted key(s) {}", seen_untrusted.join(", ")))
    } else {
        Err(format!("{} valid trusted signature(s), {need} required", valid.len()))
    }
}

fn verifying_key(k: &TrustedKey) -> Result<VerifyingKey, String> {
    let raw = B64.decode(k.public_key.as_bytes()).map_err(|e| e.to_string())?;
    let raw: [u8; 32] = raw.try_into().map_err(|_| "public key must be 32 bytes".to_string())?;
    if key_id(&raw) != k.key_id {
        return Err(format!("key id {} does not match its public key", k.key_id));
    }
    VerifyingKey::from_bytes(&raw).map_err(|e| e.to_string())
}

/// Signs `bytes` with every key (release tooling; rotation = sign with old and new key).
pub fn sign(bytes: &[u8], keys: &[SigningKey]) -> SignatureFile {
    SignatureFile {
        format_version: GROUP_FORMAT_VERSION,
        signatures: keys
            .iter()
            .map(|k| Signature {
                key_id: key_id(k.verifying_key().as_bytes()),
                algorithm: "ed25519".into(),
                signature: B64.encode(k.sign(bytes).to_bytes()),
            })
            .collect(),
    }
}

pub fn generate_key() -> Result<SigningKey, String> {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|e| e.to_string())?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Private key file: base64 of the 32-byte seed.
pub fn encode_secret(k: &SigningKey) -> String {
    B64.encode(k.to_bytes())
}

pub fn decode_secret(text: &str) -> Result<SigningKey, String> {
    let raw = B64.decode(text.trim().as_bytes()).map_err(|e| format!("bad key file: {e}"))?;
    let raw: [u8; 32] = raw.try_into().map_err(|_| "key file must hold a 32-byte seed".to_string())?;
    Ok(SigningKey::from_bytes(&raw))
}

pub fn trusted_key(k: &VerifyingKey, comment: Option<String>) -> TrustedKey {
    TrustedKey {
        key_id: key_id(k.as_bytes()),
        algorithm: "ed25519".into(),
        public_key: B64.encode(k.as_bytes()),
        not_after: None,
        revoked: false,
        comment,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(keys: &[&SigningKey], threshold: u32) -> TrustStore {
        TrustStore { format_version: 1, threshold, keys: keys.iter().map(|k| trusted_key(&k.verifying_key(), None)).collect() }
    }

    #[test]
    fn sign_verify_tamper_and_rotation() {
        let old = SigningKey::from_bytes(&[1; 32]);
        let new = SigningKey::from_bytes(&[2; 32]);
        let other = SigningKey::from_bytes(&[3; 32]);
        let body = br#"{"formatVersion":1}"#;
        let sig = serde_json::to_vec(&sign(body, &[old.clone(), new.clone()])).unwrap();

        // A node that only trusts the old key and one that already trusts the new key both accept.
        assert!(verify(body, &sig, &store(&[&old], 1), 0).is_ok());
        assert!(verify(body, &sig, &store(&[&new], 1), 0).is_ok());
        assert_eq!(verify(body, &sig, &store(&[&old, &new], 2), 0).unwrap().len(), 2);
        // Tampered bytes, untrusted signer, wrong threshold.
        assert!(verify(br#"{"formatVersion":2}"#, &sig, &store(&[&old], 1), 0).is_err());
        let err = verify(body, &sig, &store(&[&other], 1), 0).unwrap_err();
        assert!(err.contains("untrusted"), "{err}");
        let only_old = serde_json::to_vec(&sign(body, std::slice::from_ref(&old))).unwrap();
        assert!(verify(body, &only_old, &store(&[&old, &new], 2), 0).is_err());
    }

    #[test]
    fn revoked_and_expired_keys_do_not_count() {
        let k = SigningKey::from_bytes(&[7; 32]);
        let body = b"payload";
        let sig = serde_json::to_vec(&sign(body, std::slice::from_ref(&k))).unwrap();
        let mut t = store(&[&k], 1);
        t.keys[0].revoked = true;
        assert!(verify(body, &sig, &t, 0).is_err());
        let mut t = store(&[&k], 1);
        t.keys[0].not_after = Some("2026-01-01T00:00:00Z".into());
        let jan = meduza_np_plugin_sdk::rfc3339_to_unix("2025-12-31T00:00:00Z").unwrap();
        assert!(verify(body, &sig, &t, jan).is_ok());
        assert!(verify(body, &sig, &t, jan + 3 * 86_400).is_err());
        // A trust entry whose id does not match its key is ignored.
        let mut t = store(&[&k], 1);
        t.keys[0].public_key = trusted_key(&SigningKey::from_bytes(&[8; 32]).verifying_key(), None).public_key;
        assert!(verify(body, &sig, &t, 0).is_err());
    }

    #[test]
    fn secret_roundtrip() {
        let k = generate_key().unwrap();
        assert_eq!(decode_secret(&encode_secret(&k)).unwrap().to_bytes(), k.to_bytes());
    }
}
