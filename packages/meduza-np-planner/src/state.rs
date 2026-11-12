//! Persistent planner state (§11.3): last successful configuration (secrets kept in a separate 0600 file),
//! what was applied per instance, sync pointers and backoff level. All writes are atomic.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use meduza_np_protocol::config::{NodeConfig, SecretValue};
use meduza_np_protocol::revision::RevisionPointers;
use meduza_np_runtime::fsutil;
use meduza_np_runtime::paths::NodePaths;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_CONFIG_BYTES: u64 = 8 * 1024 * 1024;

/// What the last successful apply put in place, per instance (no secret content, only digests).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppliedState {
    pub revision: Option<String>,
    pub config_digest: Option<String>,
    #[serde(default)]
    pub instances: BTreeMap<String, AppliedInstance>,
    /// Instance-scoped failures do not roll back unrelated tunnels or the applied revision.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub instance_errors: BTreeMap<String, InstanceApplyError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceApplyError {
    pub package: String,
    pub kind: String,
    pub code: String,
    pub message: String,
    pub observed_at: String,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub retry_at_unix: u64,
}

impl InstanceApplyError {
    pub fn new(package: &str, kind: &str, code: &str, message: &str, config: &NodeConfig) -> Self {
        let mut message = message.to_string();
        for secret in config.secrets.values().filter(|s| !s.value.is_empty()) {
            message = message.replace(&secret.value, "[redacted]");
        }
        Self {
            package: package.into(),
            kind: kind.into(),
            code: code.into(),
            message: message.chars().take(600).collect(),
            observed_at: meduza_np_plugin_sdk::now_rfc3339(),
            attempt: 0,
            retry_at_unix: 0,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppliedInstance {
    pub package: String,
    pub plan_digest: String,
    /// Process id → restart digest.
    #[serde(default)]
    pub processes: BTreeMap<String, String>,
}

impl AppliedState {
    /// Merge into existing protocol rows, retaining probe details and avoiding duplicate instance IDs.
    pub fn overlay_instance_errors(&self, statuses: &mut Vec<meduza_np_protocol::report::InstanceStatus>) {
        use meduza_np_protocol::report::{InstanceStatus, ServiceState};
        for (id, error) in &self.instance_errors {
            let row = if let Some(index) = statuses.iter().position(|s| &s.id == id) {
                &mut statuses[index]
            } else {
                statuses.push(InstanceStatus {
                    id: id.clone(),
                    kind: error.kind.clone(),
                    state: ServiceState::Down,
                    observed_at: None,
                    reason: None,
                    detail: BTreeMap::new(),
                });
                statuses.last_mut().unwrap()
            };
            row.kind = error.kind.clone();
            row.state = if self.instances.contains_key(id) { ServiceState::Degraded } else { ServiceState::Down };
            row.reason = Some(format!("{}: {}", error.code, error.message));
            row.observed_at = Some(error.observed_at.clone());
            row.detail.insert("applyError".into(), error.message.clone().into());
            row.detail.insert("applyErrorCode".into(), error.code.clone().into());
            row.detail.insert("retryAtUnix".into(), error.retry_at_unix.into());
            row.detail.insert("attempt".into(), error.attempt.into());
        }
    }
    pub fn load(paths: &NodePaths) -> io::Result<Self> {
        Ok(fsutil::read_json(&paths.state.join("applied.json"), MAX_CONFIG_BYTES)?.unwrap_or_default())
    }
    pub fn save(&self, paths: &NodePaths) -> io::Result<()> {
        fsutil::write_json_atomic(&paths.state.join("applied.json"), self, 0o600)
    }
}

/// Last successfully applied configuration, kept as the exact JSON received (digest stays verifiable).
pub struct LastGood {
    pub raw: Value,
    pub config: NodeConfig,
    pub digest: String,
    /// Features that were enabled but held back when it applied (waiting for a package, binary or key, D47).
    pub held_back: Vec<String>,
}

pub fn save_last_good(paths: &NodePaths, raw: &Value, digest: &str, held_back: &[String]) -> io::Result<()> {
    let mut without = raw.clone();
    let secrets = without.as_object_mut().and_then(|o| o.remove("secrets")).unwrap_or(Value::Object(Default::default()));
    fsutil::ensure_dir(paths.last_good_config().parent().unwrap(), 0o700)?;
    // Secrets first: a crash between the two writes leaves the old config with possibly newer secrets, which
    // the digest check on load detects (and then we resync from the manager).
    fsutil::write_json_atomic(&paths.last_good_secrets(), &secrets, 0o600)?;
    fsutil::write_json_atomic(
        &paths.last_good_config(),
        &serde_json::json!({"digest": digest, "heldBack": held_back, "config": without}),
        0o600,
    )
}

pub fn load_last_good(paths: &NodePaths) -> io::Result<Option<LastGood>> {
    let Some(stored): Option<Value> = fsutil::read_json(&paths.last_good_config(), MAX_CONFIG_BYTES)? else { return Ok(None) };
    let mut raw = stored.get("config").cloned().unwrap_or(Value::Null);
    let digest = stored.get("digest").and_then(Value::as_str).unwrap_or_default().to_string();
    let held_back = stored.get("heldBack").cloned().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    let secrets: Option<BTreeMap<String, SecretValue>> = fsutil::read_json(&paths.last_good_secrets(), MAX_CONFIG_BYTES)?;
    if let (Some(obj), Some(secrets)) = (raw.as_object_mut(), secrets) {
        if !secrets.is_empty() {
            obj.insert("secrets".into(), serde_json::to_value(secrets).map_err(io::Error::other)?);
        }
    }
    let actual = meduza_np_protocol::digest::canonical_digest(&raw).map_err(io::Error::other)?;
    if actual != digest {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "last good config digest mismatch (interrupted write); resync required"));
    }
    let config: NodeConfig = serde_json::from_value(raw.clone()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(LastGood { raw, config, digest, held_back }))
}

/// Sync bookkeeping persisted at low frequency (§7.5.3: no flash write per tick).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncPersisted {
    #[serde(default)]
    pub pointers: RevisionPointers,
    #[serde(default)]
    pub event_cursor: Option<String>,
    #[serde(default)]
    pub backoff_level: u32,
    /// Revision that failed to apply, with its digest, so we do not retry it in a loop.
    #[serde(default)]
    pub failed: Option<(String, String)>,
}

impl SyncPersisted {
    pub fn load(path: &Path) -> Self {
        fsutil::read_json(path, 64 * 1024).ok().flatten().unwrap_or_default()
    }
    pub fn save(&self, path: &Path) -> io::Result<()> {
        fsutil::write_json_atomic(path, self, 0o600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meduza_np_protocol::report::{InstanceStatus, ServiceState};
    use serde_json::json;

    #[test]
    fn instance_errors_redact_secrets_and_overlay_existing_rows() {
        let config: NodeConfig = serde_json::from_value(json!({"schemaVersion":1,"revision":"rev-000001","node":{"id":"node"},
            "secrets":{"key":{"kind":"opaque","value":"private-material"}}}))
        .unwrap();
        let error =
            InstanceApplyError::new("meduza-np-openvpn", "openvpn-site2site", "syntax-check-failed", "bad private-material", &config);
        assert!(!error.message.contains("private-material"));
        let mut applied = AppliedState::default();
        applied.instances.insert("openvpn/good".into(), AppliedInstance::default());
        applied.instance_errors.insert("openvpn/good".into(), error.clone());
        applied.instance_errors.insert("openvpn/new".into(), error);
        let mut rows = vec![InstanceStatus {
            id: "openvpn/good".into(),
            kind: "openvpn-site2site".into(),
            state: ServiceState::Up,
            observed_at: None,
            reason: None,
            detail: [("device".into(), json!("tun-good"))].into(),
        }];
        applied.overlay_instance_errors(&mut rows);
        applied.overlay_instance_errors(&mut rows);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].state, ServiceState::Degraded);
        assert_eq!(rows[0].detail["device"], json!("tun-good"));
        assert_eq!(rows[1].state, ServiceState::Down);
        let saved = serde_json::to_value(&applied).unwrap();
        assert!(!saved.to_string().contains("private-material"));
        assert_eq!(serde_json::from_value::<AppliedState>(saved).unwrap(), applied);
        let legacy: AppliedState = serde_json::from_value(json!({"revision":null,"configDigest":null,"instances":{}})).unwrap();
        assert!(legacy.instance_errors.is_empty());
    }
}
