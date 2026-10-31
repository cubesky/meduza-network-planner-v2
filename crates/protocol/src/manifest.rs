//! Package manifest (§5.1) and package-set rules (§3.2).

use serde::{Deserialize, Serialize};

use crate::Validator;

pub const MANIFEST_VERSION: u32 = 1;
pub const PACKAGE_PREFIX: &str = "meduza-np-";

/// Pairs of packages that must never be installed/registered together (§3.2). Symmetric.
pub const CONFLICTS: &[(&str, &str)] = &[
    ("meduza-np-mihomo-openwrt", "meduza-np-mihomo"),
    ("meduza-np-mihomo-openwrt", "meduza-np-mihomo-pkg"),
    ("meduza-np-dashboard-openwrt", "meduza-np-dashboard"),
];

/// All distributable packages (§3).
pub const ALL_PACKAGES: &[&str] = &[
    "meduza-np-manager",
    "meduza-np-planner",
    "meduza-np-openvpn",
    "meduza-np-openvpn-pkg",
    "meduza-np-wireguard",
    "meduza-np-frr",
    "meduza-np-mihomo",
    "meduza-np-mihomo-pkg",
    "meduza-np-mihomo-openwrt",
    "meduza-np-mosdns",
    "meduza-np-mosdns-pkg",
    "meduza-np-dashboard",
    "meduza-np-dashboard-openwrt",
    "meduza-np-cli",
    "meduza-np-updater",
    "meduza-np-dnsmasq",
    "meduza-np-base-container",
    "meduza-np-avahi",
    "meduza-np-tinc",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PackageRole {
    /// The central manager; never registered on a node.
    Manager,
    /// The node controller itself.
    Controller,
    ServiceAdapter,
    BinaryProvider,
    UiClient,
    Cli,
    Updater,
    ContainerBase,
}

impl PackageRole {
    /// Roles planner may invoke as plugins. Everything else is registered for capability display only.
    pub fn is_plugin(self) -> bool {
        matches!(self, PackageRole::ServiceAdapter | PackageRole::BinaryProvider)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ApiRange {
    pub min: u32,
    pub max: u32,
}

impl ApiRange {
    pub fn contains(&self, v: u32) -> bool {
        self.min <= v && v <= self.max
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SchemaRefs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiRef {
    pub descriptor: String,
    pub version: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SystemDependencyKind {
    /// An executable looked up in PATH or at `path`.
    Binary,
    KernelModule,
    /// A capability such as `nftables` or `tproxy` probed by planner.
    KernelFeature,
    /// A system service that must exist (e.g. `dbus`).
    Service,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SystemDependency {
    pub kind: SystemDependencyKind,
    pub name: String,
    /// Provider slot this binary can be served by (system default, overridable by a `-pkg`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_slot: Option<String>,
    /// Only needed when the given capability is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_version: Option<String>,
}

/// A capability that is only enabled when a switch in the package's service configuration is on (e.g.
/// `mihomo.dns` ↔ `/dns/enabled`). Capabilities without a switch are enabled whenever the service is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapabilitySwitch {
    pub capability: String,
    /// JSON pointer into `services.<short name>`. A missing value means off (the schema default must be off).
    pub pointer: String,
    /// Value the switch must equal. Without it the value must be `true`, a non-empty string, array or object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<serde_json::Value>,
    /// The value is an array of instances: on while one of them has all these fields with these values and is not
    /// `"enabled": false` (e.g. `openvpn.access` ↔ `/instances` holding `{"kind": "access"}`). Replaces `equals`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<serde_json::Map<String, serde_json::Value>>,
}

impl CapabilitySwitch {
    pub fn is_on(&self, service: &serde_json::Value) -> bool {
        use serde_json::Value;
        if let Some(item) = &self.item {
            let matches = |i: &Value| i.get("enabled") != Some(&Value::Bool(false)) && item.iter().all(|(k, want)| i.get(k) == Some(want));
            return service.pointer(&self.pointer).and_then(Value::as_array).is_some_and(|a| a.iter().any(matches));
        }
        match (service.pointer(&self.pointer), &self.equals) {
            (None, _) => false,
            (Some(v), Some(want)) => v == want,
            (Some(Value::Bool(b)), None) => *b,
            (Some(Value::String(s)), None) => !s.is_empty(),
            (Some(Value::Array(a)), None) => !a.is_empty(),
            (Some(Value::Object(o)), None) => !o.is_empty(),
            (Some(_), None) => false,
        }
    }
}

/// Declaration of a binary a `-pkg` package provides for a provider slot (§3.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDecl {
    pub slot: String,
    /// Absolute path of the binary, or a path relative to `dataDir` for download/manual providers.
    pub binary: String,
    /// Arguments that print the version (`["--version"]`), run with a short timeout during checks.
    #[serde(default)]
    pub version_args: Vec<String>,
    /// Provider must be prepared (downloaded/imported) by the package's `prepare` operation before it can register.
    #[serde(default)]
    pub requires_preparation: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PackageManifest {
    pub manifest_version: u32,
    pub package: String,
    pub version: String,
    pub role: PackageRole,
    pub planner_api: ApiRange,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub conflicts: Vec<String>,
    #[serde(default)]
    pub provides: Vec<String>,
    /// Capabilities of `provides` that a configuration switch turns on separately from the service.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capability_switches: Vec<CapabilitySwitch>,
    /// Provider slot this service adapter consumes (e.g. `openvpn.binary`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_slot: Option<String>,
    /// Binaries this package provides, for `binary-provider` packages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderDecl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schemas: Option<SchemaRefs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<UiRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
    #[serde(default)]
    pub operations: Vec<String>,
    #[serde(default)]
    pub system_dependencies: Vec<SystemDependency>,
    /// Configuration paths this package consumes (`services.wireguard`, `networks.lan`, ...); used for the
    /// dependency graph instead of string searching (§8.2).
    #[serde(default)]
    pub consumes: Vec<String>,
    /// Platforms (`openwrt`, `linux`, `container`) where this package may be installed. Empty = all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
    /// Capabilities of `provides` that act on the host's or LAN's traffic (routing, TPROXY, mDNS, NAT). In a
    /// bridge-network container they only act inside the container's namespace (§14.3.3); planner and manager say so.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_network_capabilities: Vec<String>,
}

/// Short name: `meduza-np-wireguard` → `wireguard`.
pub fn short_name(package: &str) -> &str {
    package.strip_prefix(PACKAGE_PREFIX).unwrap_or(package)
}

pub fn packages_conflict(a: &str, b: &str) -> bool {
    CONFLICTS.iter().any(|(x, y)| (*x == a && *y == b) || (*x == b && *y == a))
}

impl PackageManifest {
    pub fn validate(&self) -> Validator {
        let mut v = Validator::new();
        v.check(self.manifest_version == MANIFEST_VERSION, "/manifestVersion", "unsupported manifest version");
        v.check(ALL_PACKAGES.contains(&self.package.as_str()), "/package", format!("unknown package {}", self.package));
        v.check(is_semver(&self.version), "/version", "version must be MAJOR.MINOR.PATCH[-pre]");
        v.check(self.planner_api.min <= self.planner_api.max, "/plannerApi", "min > max");
        for (i, dep) in self.requires.iter().enumerate() {
            v.check(ALL_PACKAGES.contains(&dep.as_str()), format!("/requires/{i}"), format!("unknown package {dep}"));
            v.check(dep != &self.package, format!("/requires/{i}"), "package cannot require itself");
        }
        // The manifest's own conflict list must at least contain the global rule set.
        for (a, b) in CONFLICTS {
            let other = if *a == self.package {
                Some(b)
            } else if *b == self.package {
                Some(a)
            } else {
                None
            };
            if let Some(other) = other {
                v.check(self.conflicts.iter().any(|c| c == other), "/conflicts", format!("must declare conflict with {other}"));
            }
        }
        if self.role.is_plugin() && self.role == PackageRole::ServiceAdapter {
            v.check(self.entrypoint.is_some(), "/entrypoint", "service adapters need an entrypoint");
            v.check(self.ui.is_some(), "/ui", "service adapters need a UI descriptor");
            v.check(self.schemas.as_ref().and_then(|s| s.config.as_ref()).is_some(), "/schemas/config", "config schema required");
        }
        if self.role == PackageRole::BinaryProvider {
            v.check(!self.providers.is_empty(), "/providers", "binary providers must declare at least one provider");
        }
        if let Some(ep) = &self.entrypoint {
            v.check(ep.starts_with('/') && !ep.contains(".."), "/entrypoint", "entrypoint must be an absolute path without '..'");
        }
        for (i, s) in self.capability_switches.iter().enumerate() {
            v.check(self.provides.contains(&s.capability), format!("/capabilitySwitches/{i}/capability"), "capability is not in provides");
            v.check(s.pointer.starts_with('/'), format!("/capabilitySwitches/{i}/pointer"), "pointer must start with '/'");
            v.check(s.item.is_none() || s.equals.is_none(), format!("/capabilitySwitches/{i}/item"), "item and equals exclude each other");
        }
        for (i, c) in self.host_network_capabilities.iter().enumerate() {
            v.check(self.provides.contains(c), format!("/hostNetworkCapabilities/{i}"), "capability is not in provides");
        }
        for (i, op) in self.operations.iter().enumerate() {
            v.check(
                op.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'),
                format!("/operations/{i}"),
                "operation names are lowercase words separated by '-'",
            );
        }
        v
    }
}

fn is_semver(v: &str) -> bool {
    let core = v.split_once('-').map_or(v, |(c, _)| c);
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Parses `MAJOR.MINOR.PATCH` (pre-release suffix ignored) for ordering.
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.split_once('-').map_or(v, |(c, _)| c);
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    let r = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spec_manifest_valid() {
        let m: PackageManifest = serde_json::from_value(json!({
          "manifestVersion": 1,
          "package": "meduza-np-openvpn",
          "version": "0.1.0",
          "role": "service-adapter",
          "plannerApi": {"min": 1, "max": 1},
          "requires": ["meduza-np-planner"],
          "conflicts": [],
          "provides": ["openvpn.site2site", "openvpn.access", "openvpn.keygen"],
          "providerSlot": "openvpn.binary",
          "schemas": {"config": "schema/config.json", "status": "schema/status.json"},
          "ui": {"descriptor": "ui/descriptor.json", "version": 1},
          "entrypoint": "/usr/libexec/meduza-np/plugins/openvpn",
          "operations": ["validate", "plan", "probe", "generate-keys"]
        }))
        .unwrap();
        assert!(m.validate().is_empty(), "{:?}", m.validate().errors());
    }

    #[test]
    fn conflicts_are_symmetric_and_enforced() {
        assert!(packages_conflict("meduza-np-mihomo", "meduza-np-mihomo-openwrt"));
        assert!(packages_conflict("meduza-np-dashboard-openwrt", "meduza-np-dashboard"));
        assert!(!packages_conflict("meduza-np-mihomo", "meduza-np-mihomo-pkg"));
        let m: PackageManifest = serde_json::from_value(json!({
          "manifestVersion": 1, "package": "meduza-np-dashboard", "version": "0.1.0", "role": "ui-client",
          "plannerApi": {"min": 1, "max": 1}
        }))
        .unwrap();
        assert!(m.validate().errors().iter().any(|e| e.path == "/conflicts"));
    }

    #[test]
    fn capability_switches() {
        let dns = CapabilitySwitch { capability: "mihomo.dns".into(), pointer: "/dns/enabled".into(), equals: None, item: None };
        assert!(!dns.is_on(&json!({})));
        assert!(!dns.is_on(&json!({"dns": {"enabled": false}})));
        assert!(dns.is_on(&json!({"dns": {"enabled": true}})));
        let subs =
            CapabilitySwitch { capability: "mihomo.subscriptions".into(), pointer: "/subscriptions".into(), equals: None, item: None };
        assert!(!subs.is_on(&json!({"subscriptions": []})));
        assert!(subs.is_on(&json!({"subscriptions": [{"id": "a"}]})));
        let tp =
            CapabilitySwitch { capability: "mihomo.tproxy".into(), pointer: "/mode".into(), equals: Some(json!("tproxy")), item: None };
        assert!(!tp.is_on(&json!({"mode": "proxy"})) && tp.is_on(&json!({"mode": "tproxy"})));
        let access = CapabilitySwitch {
            capability: "openvpn.access".into(),
            pointer: "/instances".into(),
            equals: None,
            item: json!({"kind": "access"}).as_object().cloned(),
        };
        assert!(!access.is_on(&json!({})));
        assert!(!access.is_on(&json!({"instances": [{"id": "ext", "kind": "site2site"}, {"id": "x"}]})));
        assert!(!access.is_on(&json!({"instances": [{"id": "a", "kind": "access", "enabled": false}]})), "disabled instance");
        assert!(access.is_on(&json!({"instances": [{"id": "ext"}, {"id": "a", "kind": "access"}]})));
        let mut m: PackageManifest = serde_json::from_value(json!({
          "manifestVersion": 1, "package": "meduza-np-avahi", "version": "0.1.0", "role": "ui-client",
          "plannerApi": {"min": 1, "max": 1}, "provides": ["mdns.bridge"]
        }))
        .unwrap();
        m.capability_switches = vec![dns];
        assert!(m.validate().errors().iter().any(|e| e.path == "/capabilitySwitches/0/capability"));
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2.3-rc.1"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(short_name("meduza-np-wireguard"), "wireguard");
    }
}
