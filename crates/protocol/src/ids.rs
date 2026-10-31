//! Stable identifiers. Display names never double as file paths, interface names or ownership keys (§8.1).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ValidationError;

/// Maximum Linux interface name length in bytes (IFNAMSIZ - 1).
pub const IFNAME_MAX: usize = 15;

/// A stable object ID: lowercase ASCII letters, digits, `-` and `_`, 1..=63 bytes, starting with a letter or digit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct StableId(String);

impl StableId {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        validate_stable_id(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub fn validate_stable_id(value: &str) -> Result<(), ValidationError> {
    let ok = !value.is_empty()
        && value.len() <= 63
        && value.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && value.as_bytes()[0].is_ascii_alphanumeric();
    if ok {
        Ok(())
    } else {
        Err(ValidationError::new(
            "",
            format!("invalid id {value:?}: use 1-63 lowercase letters, digits, '-' or '_', starting alphanumeric"),
        ))
    }
}

impl TryFrom<String> for StableId {
    type Error = ValidationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<StableId> for String {
    fn from(value: StableId) -> Self {
        value.0
    }
}

impl fmt::Display for StableId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for StableId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Validates a Linux network interface name: 1..=15 bytes, no `/`, whitespace, `:` or NUL, not `.`/`..`.
pub fn validate_ifname(name: &str) -> Result<(), ValidationError> {
    let ok = !name.is_empty()
        && name.len() <= IFNAME_MAX
        && name != "."
        && name != ".."
        && name.bytes().all(|b| b.is_ascii_graphic() && b != b'/' && b != b':');
    if ok {
        Ok(())
    } else {
        Err(ValidationError::new("", format!("invalid interface name {name:?}: 1-{IFNAME_MAX} printable bytes without '/', ':' or spaces")))
    }
}

/// Derives a deterministic interface name from a prefix and a stable ID.
///
/// The ID is kept verbatim when it fits; otherwise it is shortened and suffixed with a short hash, so two
/// different IDs never collapse to the same name by plain truncation (§9.1, §9.4).
pub fn derive_ifname(prefix: &str, id: &str) -> String {
    let candidate = format!("{prefix}{id}");
    let sanitized: String = candidate.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    if sanitized.len() <= IFNAME_MAX {
        return sanitized;
    }
    // "<head>-<4 hex>"; callers still run a cross-instance collision check on the result.
    let hash = crate::digest::sha256_hex(id.as_bytes());
    let head: String = sanitized.chars().take(IFNAME_MAX - 5).collect();
    format!("{head}-{}", &hash[..4])
}

impl schemars::JsonSchema for StableId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "StableId".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type": "string", "pattern": "^[a-z0-9][a-z0-9_-]{0,62}$", "description": "Stable object id"})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_ids() {
        assert!(StableId::new("gw-a").is_ok());
        assert!(StableId::new("office_1").is_ok());
        assert!(StableId::new("").is_err());
        assert!(StableId::new("-x").is_err());
        assert!(StableId::new("Upper").is_err());
        assert!(StableId::new("a/b").is_err());
        assert!(StableId::new("a".repeat(64)).is_err());
    }

    #[test]
    fn ifnames() {
        assert!(validate_ifname("tun-office").is_ok());
        assert!(validate_ifname("a234567890123456").is_err());
        assert!(validate_ifname("wg 0").is_err());
        assert_eq!(derive_ifname("wg-", "uplink"), "wg-uplink");
        let a = derive_ifname("tun-", "very-long-instance-one");
        let b = derive_ifname("tun-", "very-long-instance-two");
        assert!(a.len() <= IFNAME_MAX && b.len() <= IFNAME_MAX);
        assert_ne!(a, b);
        assert!(validate_ifname(&a).is_ok());
    }
}
