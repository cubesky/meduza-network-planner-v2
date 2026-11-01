//! Typed node configuration snapshot (§8). One snapshot per node per revision.
//!
//! `services` keeps each package's configuration as JSON owned by that package's own typed model
//! (plugin crates deserialize their entry). Shared objects used by several packages — networks, routing
//! intents, DNS hosts, secrets and update policy — are typed here.

use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;

use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{validate_stable_id, StableId};
use crate::revision::RevisionId;
use crate::Validator;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeConfig {
    pub schema_version: u32,
    pub revision: RevisionId,
    pub node: NodeSettings,
    #[serde(default)]
    pub networks: Networks,
    /// Keyed by package short name (`wireguard`, `tinc`, `frr`, ...). Only installed packages appear.
    #[serde(default)]
    pub services: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<RoutingIntent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns: Option<DnsSettings>,
    /// Only the secrets this node needs, by `secretRef` id. Never logged (see [`SecretValue`]'s Debug).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: BTreeMap<String, SecretValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_policy: Option<UpdatePolicy>,
}

/// Runtime package prerequisite for the mesh or active external connections, even before BGP is configured.
pub fn needs_routing_package(services: &BTreeMap<String, Value>) -> bool {
    let enabled = |v: &Value| v.is_object() && v.get("enabled").and_then(Value::as_bool) != Some(false);
    if let Some(tinc) = services.get("tinc").filter(|v| enabled(v)) {
        if let Some(list) = tinc.get("instances").and_then(Value::as_array) {
            if list.iter().any(enabled) {
                return true;
            }
        } else {
            return true;
        }
    }
    ["wireguard", "openvpn"].iter().any(|key| {
        services
            .get(*key)
            .filter(|v| enabled(v))
            .and_then(|v| v.get("instances"))
            .and_then(Value::as_array)
            .is_some_and(|list| list.iter().any(|v| enabled(v) && v.get("kind").and_then(Value::as_str) != Some("access")))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NodeBehavior {
    #[default]
    Static,
    Roaming,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeSettings {
    pub id: StableId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default)]
    pub behavior: NodeBehavior,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub router_id: Option<std::net::Ipv4Addr>,
    #[serde(default)]
    pub sync: SyncSettings,
}

/// Sync intervals (§7.3). All values get jitter applied by the planner scheduler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncSettings {
    pub heartbeat_seconds: u32,
    pub push_keepalive_seconds: u32,
    pub roaming_poll_seconds: u32,
    pub calibration_seconds: u32,
}

impl Default for SyncSettings {
    fn default() -> Self {
        Self { heartbeat_seconds: 15, push_keepalive_seconds: 30, roaming_poll_seconds: 60, calibration_seconds: 600 }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Networks {
    #[serde(default)]
    pub lan: Vec<IpNet>,
    #[serde(default)]
    pub private_lan: Vec<IpNet>,
    #[serde(default)]
    pub mappings: Vec<NetworkMapping>,
    #[serde(default)]
    pub port_forwards: Vec<PortForward>,
    #[serde(default)]
    pub health_ports: Vec<HealthPort>,
}

/// 1:1 network mapping: `advertised` is the externally visible prefix, `actual` the real local one (§9.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NetworkMapping {
    pub id: StableId,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub advertised: IpNet,
    pub actual: IpNet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TransportProto {
    Tcp,
    Udp,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortForward {
    pub id: StableId,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub proto: TransportProto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_address: Option<IpAddr>,
    pub listen_port: u16,
    pub target_address: IpAddr,
    pub target_port: u16,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_cidrs: Vec<IpNet>,
    #[serde(default)]
    pub snat: bool,
}

/// Plain TCP health listener; its meaning is declared explicitly and is unrelated to service probes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HealthPort {
    pub id: StableId,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_address: Option<IpAddr>,
    pub port: u16,
    /// What "accepting connections" means, e.g. `planner-alive` or `feature:<id>` healthy.
    pub semantics: String,
}

/// Cross-package routing intents compiled by the manager (§9.5). FRR consumes them together with its own
/// `services.frr` entry; Access and mappings refer to the same prefix sets.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RoutingIntent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal_asn: Option<u32>,
    /// Prefix sets this node may originate, by source. Only these are ever advertised.
    #[serde(default)]
    pub originate: OriginateSets,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OriginateSets {
    #[serde(default)]
    pub lan: bool,
    #[serde(default)]
    pub private_lan: bool,
    #[serde(default)]
    pub mappings: bool,
    /// Access pools by OpenVPN instance id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub access_pools: Vec<StableId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<IpNet>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DnsSettings {
    #[serde(default)]
    pub hosts: Vec<HostEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HostEntry {
    pub hostname: String,
    pub ips: Vec<IpAddr>,
}

/// Secret material delivered to exactly the node that needs it. `Debug` never prints the value.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretValue {
    /// e.g. `wireguard-private-key`, `wireguard-psk`, `tinc-ed25519-private`, `pem-private-key`, `pem-certificate`, `openvpn-static-key`, `text`.
    pub kind: String,
    pub value: String,
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretValue").field("kind", &self.kind).field("value", &"<redacted>").finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePolicy {
    #[serde(default = "default_channel")]
    pub channel: String,
    #[serde(default)]
    pub automatic: bool,
    /// `HH:MM-HH:MM`, may wrap midnight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance_window: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default)]
    pub paused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_group_version: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_channel() -> String {
    "stable".into()
}

/// Parses `HH:MM-HH:MM` into minutes since midnight.
pub fn parse_maintenance_window(text: &str) -> Option<(u16, u16)> {
    fn hm(s: &str) -> Option<u16> {
        let (h, m) = s.split_once(':')?;
        if h.len() != 2 || m.len() != 2 {
            return None;
        }
        let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
        (h < 24 && m < 60).then_some(h * 60 + m)
    }
    let (a, b) = text.split_once('-')?;
    Some((hm(a)?, hm(b)?))
}

impl NodeConfig {
    /// Structural checks that do not depend on installed packages. Package-specific checks happen in plugins.
    pub fn validate(&self) -> Validator {
        let mut v = Validator::new();
        v.check(
            self.schema_version == crate::CONFIG_SCHEMA_VERSION,
            "/schemaVersion",
            format!("unsupported schemaVersion {}", self.schema_version),
        );
        for key in self.services.keys() {
            if validate_stable_id(key).is_err() {
                v.error(format!("/services/{key}"), "service key must be a package short name");
            }
        }
        self.networks.validate_into(&mut v);
        if let Some(dns) = &self.dns {
            for (i, host) in dns.hosts.iter().enumerate() {
                if !valid_hostname(&host.hostname) {
                    v.error(format!("/dns/hosts/{i}/hostname"), "invalid hostname");
                }
                v.check(!host.ips.is_empty(), format!("/dns/hosts/{i}/ips"), "at least one address required");
            }
        }
        if let Some(policy) = &self.update_policy {
            if let Some(window) = &policy.maintenance_window {
                v.check(parse_maintenance_window(window).is_some(), "/updatePolicy/maintenanceWindow", "expected HH:MM-HH:MM");
            }
        }
        let sync = &self.node.sync;
        v.check(sync.heartbeat_seconds >= 5, "/node/sync/heartbeatSeconds", "must be >= 5");
        v.check(sync.push_keepalive_seconds >= 10, "/node/sync/pushKeepaliveSeconds", "must be >= 10");
        v.check(sync.roaming_poll_seconds >= 15, "/node/sync/roamingPollSeconds", "must be >= 15");
        v
    }
}

impl Networks {
    pub fn validate_into(&self, v: &mut Validator) {
        let mut ids = std::collections::BTreeSet::new();
        for (i, m) in self.mappings.iter().enumerate() {
            let path = format!("/networks/mappings/{i}");
            v.check(ids.insert(m.id.clone()), format!("{path}/id"), "duplicate id");
            let same_family = matches!((m.advertised, m.actual), (IpNet::V4(_), IpNet::V4(_)) | (IpNet::V6(_), IpNet::V6(_)));
            v.check(same_family, &path, "advertised and actual must have the same address family");
            v.check(m.advertised.prefix_len() == m.actual.prefix_len(), &path, "advertised and actual must have the same prefix length");
            v.check(m.advertised.trunc() == m.advertised, format!("{path}/advertised"), "host bits must be zero");
            v.check(m.actual.trunc() == m.actual, format!("{path}/actual"), "host bits must be zero");
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut listeners = std::collections::BTreeSet::new();
        for (i, pf) in self.port_forwards.iter().enumerate() {
            let path = format!("/networks/portForwards/{i}");
            v.check(ids.insert(pf.id.clone()), format!("{path}/id"), "duplicate id");
            v.check(pf.listen_port != 0 && pf.target_port != 0, &path, "ports must be non-zero");
            if pf.enabled {
                for proto in match pf.proto {
                    TransportProto::Both => vec!["tcp", "udp"],
                    TransportProto::Tcp => vec!["tcp"],
                    TransportProto::Udp => vec!["udp"],
                } {
                    v.check(
                        listeners.insert((proto, pf.listen_address, pf.listen_port)),
                        format!("{path}/listenPort"),
                        format!("{proto} listener already used by another port forward"),
                    );
                }
            }
            if let Some(listen) = pf.listen_address {
                v.check(listen.is_ipv4() == pf.target_address.is_ipv4(), &path, "listen and target address family differ");
            }
        }
        for (field, nets) in [("lan", &self.lan), ("privateLan", &self.private_lan)] {
            for (i, net) in nets.iter().enumerate() {
                v.check(net.trunc() == *net, format!("/networks/{field}/{i}"), "host bits must be zero");
            }
        }
    }
}

/// RFC 1123 hostname (labels 1-63, total ≤ 253, letters/digits/hyphen, no leading/trailing hyphen).
pub fn valid_hostname(name: &str) -> bool {
    let name = name.strip_suffix('.').unwrap_or(name);
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_package_follows_mesh_and_external_connections() {
        let needs = |value: Value| needs_routing_package(&serde_json::from_value(value).unwrap());
        assert!(needs(serde_json::json!({"tinc":{"enabled":true}})));
        assert!(needs(serde_json::json!({"tinc":{"instances":[{"id":"mesh"}]}})));
        assert!(needs(serde_json::json!({"wireguard":{"instances":[{"id":"external"}]}})));
        assert!(needs(serde_json::json!({"openvpn":{"instances":[{"kind":"site2site"}]}})));
        assert!(!needs(serde_json::json!({"tinc":{"enabled":false}})));
        assert!(!needs(serde_json::json!({"tinc":{"instances":[{"enabled":false}]}})));
        assert!(!needs(serde_json::json!({"wireguard":{"enabled":false,"instances":[{}]}})));
        assert!(!needs(serde_json::json!({"wireguard":{"instances":[]}})));
        assert!(!needs(serde_json::json!({"openvpn":{"instances":[{"kind":"access"}]}})));
    }
    use serde_json::json;

    fn sample() -> Value {
        json!({
          "schemaVersion": 1,
          "revision": "rev-000042",
          "node": {"id": "gw-a", "routerId": "10.42.1.1", "behavior": "static"},
          "networks": {
            "lan": ["10.42.10.0/24"],
            "privateLan": ["10.99.10.0/24"],
            "mappings": [{"id": "map-office", "advertised": "10.100.1.0/24", "actual": "192.168.1.0/24"}],
            "portForwards": []
          },
          "services": {
            "tinc": {"enabled": true, "netname": "mesh", "device": "tnc0", "address": "10.42.1.1/24", "privateKeyRef": "tinc-local"},
            "frr": {"enabled": true, "localAsn": 65010, "internalNeighbors": [], "externalNeighbors": [], "anycast": {"enabled": false}},
            "dnsmasq": {"enabled": true, "fallbackServers": ["119.29.29.29", "1.0.0.1"]}
          },
          "updatePolicy": {"channel": "stable", "automatic": true, "maintenanceWindow": "02:00-04:00", "timezone": "Asia/Shanghai"}
        })
    }

    #[test]
    fn spec_example_parses_and_validates() {
        let cfg: NodeConfig = serde_json::from_value(sample()).unwrap();
        assert_eq!(cfg.node.behavior, NodeBehavior::Static);
        assert_eq!(cfg.node.sync.heartbeat_seconds, 15);
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate().errors());
    }

    #[test]
    fn mapping_prefix_mismatch_rejected() {
        let mut raw = sample();
        raw["networks"]["mappings"][0]["actual"] = json!("192.168.0.0/23");
        let cfg: NodeConfig = serde_json::from_value(raw).unwrap();
        let errors = cfg.validate();
        assert!(errors.errors().iter().any(|e| e.message.contains("prefix length")));
    }

    #[test]
    fn secrets_are_redacted_in_debug() {
        let s = SecretValue { kind: "text".into(), value: "hunter2".into() };
        assert!(!format!("{s:?}").contains("hunter2"));
    }

    #[test]
    fn windows() {
        assert_eq!(parse_maintenance_window("02:00-04:30"), Some((120, 270)));
        assert_eq!(parse_maintenance_window("23:00-01:00"), Some((1380, 60)));
        assert_eq!(parse_maintenance_window("2:00-04:00"), None);
        assert_eq!(parse_maintenance_window("24:00-01:00"), None);
    }

    #[test]
    fn hostnames() {
        assert!(valid_hostname("printer.local"));
        assert!(valid_hostname("a-b.example."));
        assert!(!valid_hostname("-bad.example"));
        assert!(!valid_hostname("bad_name"));
    }
}
