//! Node status report (§12.1, §12.2).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Generic service state. Services add protocol sub-states in [`InstanceStatus::detail`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ServiceState {
    Disabled,
    Pending,
    Starting,
    Up,
    Degraded,
    Down,
    #[default]
    Unknown,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeReport {
    pub report_version: u32,
    pub node_id: String,
    /// Changes on every planner process start; together with `sequence` orders reports (§12.1).
    pub boot_id: String,
    pub sequence: u64,
    /// RFC 3339 UTC. Informational only: ordering uses (bootId, sequence) plus manager receive time.
    pub observed_at: String,
    pub sync: SyncStatus,
    #[serde(default)]
    pub packages: Vec<PackageStatus>,
    #[serde(default)]
    pub features: Vec<FeatureStatus>,
    #[serde(default)]
    pub instances: Vec<InstanceStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<MaintenanceStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SyncMode {
    Push,
    Poll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SyncTransport {
    Websocket,
    Http2Events,
    Https,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SyncState {
    /// Never synced since enrollment.
    Unsynced,
    InSync,
    /// A newer desired revision is known but not yet applied.
    Behind,
    Applying,
    /// Last apply failed; running the last successful revision.
    ApplyFailed,
    /// Cannot reach manager; running the last successful revision.
    Offline,
    /// 401/403 or TLS trust failure; waiting for credential repair.
    CredentialError,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    pub mode: SyncMode,
    pub transport: SyncTransport,
    pub behavior: crate::config::NodeBehavior,
    pub state: SyncState,
    #[serde(flatten)]
    pub revisions: crate::revision::RevisionPointers,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply_stage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Failure while restoring the last successful revision, separate from the desired revision's apply error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_error: Option<String>,
    #[serde(default)]
    pub backoff: BackoffStatus,
}

/// Visible state of the reconnect scheduler (§7.5.3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackoffStatus {
    pub failure_level: u32,
    pub window_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_attempt_in_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_cooldown_ms: Option<u64>,
    #[serde(default)]
    pub probing: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RegistrationState {
    Registered,
    Incompatible,
    Conflict,
    MissingDependency,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PackageStatus {
    pub name: String,
    pub version: String,
    pub role: crate::manifest::PackageRole,
    pub manifest_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_digest: Option<String>,
    pub registration: RegistrationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<String>,
}

/// One capability of an installed package as it stands on this node (§12.1: installed → registered →
/// enabled → available → running → healthy are distinct).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeatureStatus {
    pub id: String,
    pub package: String,
    pub installed: bool,
    pub registered: bool,
    pub enabled: bool,
    pub available: bool,
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub healthy: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Why an enabled, running capability does not have its full effect here, e.g. `bridge-network`: routing or
    /// TPROXY that only acts inside a bridge-network container's namespace (§14.3.3). Not a failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limitation_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limitation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceStatus {
    /// `<package-short>/<instance>[/<sub>]`, e.g. `wireguard/uplink/peer-b`.
    pub id: String,
    pub kind: String,
    pub state: ServiceState,
    /// RFC 3339; when this was last measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Protocol specific fields: handshake, rx/tx bytes, BGP state, etc.
    #[serde(flatten)]
    pub detail: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResourceSummary {
    /// Measured package footprints. Absent on older nodes; unknown measurements are omitted, never zero-filled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<PackageResources>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planner_rss_kib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_available_kib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_total_kib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_disk_free_kib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load1_milli: Option<u64>,
    #[serde(default)]
    pub supervised_processes: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PackageResources {
    pub package: String,
    /// Filesystem allocated bytes for package files, including retained store versions (not download archives).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
    /// Separately measured persistent service/provider data; not included in diskBytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_bytes: Option<u64>,
    /// Unknown if any running process could not be measured. RSS may include shared pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_rss_bytes: Option<u64>,
    pub disk_observed_at: String,
    pub memory_observed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default)]
    pub processes: Vec<ProcessResources>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProcessResources {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    pub pid: u32,
    /// PID namespace in which this PID was measured (e.g. planner or updater).
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_rss_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceStatus {
    pub active: bool,
    pub reason: String,
    pub by: String,
    /// RFC 3339 expiry; auto-reconcile resumes afterwards.
    pub until: String,
    #[serde(default)]
    pub apply_paused: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SystemInfo {
    pub arch: String,
    pub os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_release: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    #[serde(default)]
    pub openwrt: bool,
    #[serde(default)]
    pub container: bool,
    /// Container network mode the deployment declared (`bridge`, `host` or `macvlan`, `MEDUZA_NP_NETWORK_MODE`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_mode: Option<String>,
    /// Effective Linux capabilities of the planner process without the `CAP_` prefix (`NET_ADMIN`, `SYS_ADMIN`, …).
    /// Reported inside containers only, where the deployment chooses them (§14.3.4); the manager warns before
    /// publishing a feature the container cannot run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spec_report_parses() {
        let raw = json!({
          "reportVersion": 1,
          "nodeId": "gw-a",
          "bootId": "boot-example",
          "sequence": 123,
          "observedAt": "2026-10-23T08:00:00Z",
          "sync": {"mode": "push", "transport": "websocket", "behavior": "static", "desiredRevision": "rev-000042", "receivedRevision": "rev-000042", "appliedRevision": "rev-000042", "state": "in-sync"},
          "features": [
            {"id": "wireguard.interfaces", "package": "meduza-np-wireguard", "installed": true, "registered": true, "enabled": true, "available": true, "provider": "system"}
          ],
          "instances": [
            {"id": "wireguard/uplink/peer-b", "kind": "wireguard-peer", "state": "up", "latestHandshake": "2026-10-23T07:59:50Z", "rxBytes": 1024, "txBytes": 2048}
          ]
        });
        let report: NodeReport = serde_json::from_value(raw).unwrap();
        assert_eq!(report.sync.state, SyncState::InSync);
        assert_eq!(report.instances[0].detail["rxBytes"], json!(1024));
        let back = serde_json::to_value(&report).unwrap();
        assert_eq!(back["sync"]["appliedRevision"], json!("rev-000042"));
    }
}
