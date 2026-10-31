//! Canonical JSON and SHA-256 digests.
//!
//! Canonical form: object keys sorted by UTF-16 code units (same order as JavaScript's default sort, which
//! equals byte order for the ASCII keys used in this project), no insignificant whitespace, numbers as
//! serde_json prints them. The manager implements the identical algorithm in TypeScript
//! (`packages/meduza-np-manager/server/src/canonical.ts`); configuration documents only use integers so
//! the number formatting of both sides agrees.

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Serializes `value` to canonical JSON bytes.
pub fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    let value = serde_json::to_value(value)?;
    let mut out = Vec::with_capacity(256);
    write_canonical(&value, &mut out);
    Ok(out)
}

/// SHA-256 over the canonical JSON form, hex encoded.
pub fn canonical_digest<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    Ok(sha256_hex(&canonical_json(value)?))
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push(b'{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(serde_json::to_string(key).expect("string serializes").as_bytes());
                out.push(b':');
                write_canonical(&map[*key], out);
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        other => out.extend_from_slice(serde_json::to_string(other).expect("scalar serializes").as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_sorts_keys_and_strips_whitespace() {
        let v = json!({"b": 1, "a": {"d": [1, 2, {"z": true, "y": null}], "c": "x"}});
        let bytes = canonical_json(&v).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), r#"{"a":{"c":"x","d":[1,2,{"y":null,"z":true}]},"b":1}"#);
    }

    #[test]
    fn digest_is_stable() {
        let a = json!({"x": 1, "y": [1, 2]});
        let b = json!({"y": [1, 2], "x": 1});
        assert_eq!(canonical_digest(&a).unwrap(), canonical_digest(&b).unwrap());
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
