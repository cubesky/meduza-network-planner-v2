//! Package group (release) metadata served from GitHub Pages (§13.4).

use serde::{Deserialize, Serialize};

use crate::manifest::ApiRange;

pub const GROUP_FORMAT_VERSION: u32 = 1;

/// `/channels/<channel>.json` — the signed pointer that makes a complete group visible (§15.1 step 9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChannelPointer {
    pub format_version: u32,
    pub channel: String,
    pub group_version: String,
    pub sequence: u64,
    /// Relative path of the group manifest, e.g. `releases/v0.2.0/manifest.json`.
    pub manifest: String,
    pub manifest_sha256: String,
    /// RFC 3339; updater rejects pointers past this time (freeze attack protection).
    pub expires_at: String,
    pub published_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GroupManifest {
    pub format_version: u32,
    pub group_version: String,
    pub sequence: u64,
    pub source_commit: String,
    pub published_at: String,
    pub expires_at: String,
    pub platforms: Vec<String>,
    pub compatibility: Compatibility,
    pub packages: Vec<GroupPackage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Compatibility {
    pub planner_api: ApiRange,
    pub manager_api: ApiRange,
    pub config_schema: ApiRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GroupPackage {
    pub name: String,
    pub version: String,
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reused_from_group: Option<String>,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub conflicts: Vec<String>,
    pub artifacts: Vec<Artifact>,
    /// Upstream binary metadata for download providers (mihomo-pkg): tested versions and assets. Not part of
    /// this package's own artifacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    /// `linux/amd64`, `linux/arm/v7`, `linux/arm64`, `any` for architecture-independent assets, or an
    /// OpenWrt package arch like `openwrt/aarch64_cortex-a53`.
    pub platform: String,
    /// `tar.zst`, `tar.gz`, `ipk`, `apk`, `oci`.
    pub format: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oci: Option<String>,
}

impl GroupManifest {
    pub fn package(&self, name: &str) -> Option<&GroupPackage> {
        self.packages.iter().find(|p| p.name == name)
    }
}

/// `<file>.sig` next to every signed metadata file (§13.4). Each signature covers the exact raw bytes of the
/// file as served (no re-serialization), so verification never depends on JSON canonicalization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SignatureFile {
    pub format_version: u32,
    /// Several signatures allow key rotation: during a rotation the release is signed by the old and new key.
    pub signatures: Vec<Signature>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Signature {
    /// [`key_id`] of the signing public key.
    pub key_id: String,
    /// Only `ed25519` is defined.
    pub algorithm: String,
    /// Base64 (standard, padded) signature over the raw file bytes.
    pub signature: String,
}

/// Release trust roots installed on the node (`/etc/meduza-np/trust/release-keys.json`, shipped by the updater
/// package and the base image). A file is accepted when at least `threshold` distinct trusted keys signed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrustStore {
    pub format_version: u32,
    #[serde(default = "one")]
    pub threshold: u32,
    pub keys: Vec<TrustedKey>,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrustedKey {
    pub key_id: String,
    pub algorithm: String,
    /// Base64 of the 32-byte Ed25519 public key.
    pub public_key: String,
    /// RFC 3339; signatures by this key are not accepted after this time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_after: Option<String>,
    #[serde(default)]
    pub revoked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// Stable key id: the first 16 hex characters of SHA-256 over the raw public key bytes.
pub fn key_id(public_key: &[u8]) -> String {
    let mut h = crate::digest::sha256_hex(public_key);
    h.truncate(16);
    h
}

/// The runnable package set recorded by the updater (`<store>/installed.json`). planner, CLI and dashboards read
/// it to show installed versions; only the updater writes it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstalledSet {
    pub format_version: u32,
    /// Local activation counter; increases with every switch (including rollbacks).
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_sequence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// `store` (container / generic Linux), `opkg` or `apk` (OpenWrt).
    pub backend: String,
    pub platform: String,
    pub packages: std::collections::BTreeMap<String, InstalledPackage>,
    /// Packages the operator selected explicitly (container `MEDUZA_NP_COMPONENTS` or `install`); the rest are
    /// fixed components or dependencies.
    #[serde(default)]
    pub selected: Vec<String>,
    pub activated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstalledPackage {
    pub version: String,
    /// SHA-256 of the verified artifact; empty for `image`-provided packages.
    #[serde(default)]
    pub sha256: String,
    /// `group:<version>`, `image` (built into the base image) or `system` (found installed, not by us).
    pub source: String,
    /// Store object directory name (store backend only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
}

/// Maps a Rust target / CPU to the OCI platform string (§14.1).
pub fn oci_platform_for_arch(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" | "amd64" => Some("linux/amd64"),
        "aarch64" | "arm64" => Some("linux/arm64"),
        "armv7" | "armv7l" => Some("linux/arm/v7"),
        _ => None,
    }
}
