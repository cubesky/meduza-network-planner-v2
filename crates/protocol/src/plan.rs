//! Generation plan returned by a service plugin's `plan` operation (§5.2). Plugins never touch the system;
//! planner validates ownership/paths/conflicts and applies the plan.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Validator;

/// Placeholders planner expands in `argv`, file content is never expanded.
pub const PLACEHOLDERS: &[&str] = &["${GENERATION_ROOT}", "${STATE_DIR}", "${RUN_DIR}", "${PROVIDER_BIN}", "${LIBEXEC_DIR}"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GenerationPlan {
    pub protocol_version: u32,
    pub request_id: String,
    /// `<package-short>/<instance-id>`; a plugin may return several plans, one per instance.
    pub instance: String,
    #[serde(default)]
    pub files: Vec<PlannedFile>,
    #[serde(default)]
    pub processes: Vec<ProcessSpec>,
    #[serde(default)]
    pub resources: Vec<NetworkResource>,
    /// Other instances (`frr/main`, `dnsmasq/main`) that must be applied first.
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub probes: Vec<ProbeSpec>,
    /// Native syntax checks planner runs against staged files before switching generations.
    #[serde(default)]
    pub checks: Vec<CommandSpec>,
    /// Commands run after the new generation is active when the process does not need a restart.
    #[serde(default)]
    pub reload: Vec<CommandSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlannedFile {
    /// Relative to the generation directory.
    pub path: String,
    /// Octal string, e.g. `0600`.
    pub mode: String,
    pub content: String,
    #[serde(default)]
    pub sensitive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Never,
    #[default]
    OnFailure,
    Always,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProcessSpec {
    pub id: String,
    /// Binary comes from the resolved provider of this slot (`${PROVIDER_BIN}`); mutually exclusive with `program`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_slot: Option<String>,
    /// Absolute path of a program shipped by this project (e.g. a bridge helper).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub restart: RestartPolicy,
    #[serde(default = "default_stop_timeout")]
    pub stop_timeout_seconds: u32,
    /// Signal used for reload instead of restart, if the daemon supports it for the changed files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reload_signal: Option<String>,
    /// Digest of the inputs that require a restart when they change; planner restarts only on change (§11.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_digest: Option<String>,
    /// Run as this user/group if the platform supports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Id of an earlier process of the same plan this one depends on at runtime (e.g. FRR daemons on zebra): it is
    /// restarted whenever that process starts a new process (crash restart, spec change, not adopted after a
    /// planner restart), and is never adopted when that process was started anew.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_to: Option<String>,
}

fn default_stop_timeout() -> u32 {
    10
}

/// A one-shot command (syntax check, reload). argv\[0] is a provider slot reference or an absolute path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommandSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_slot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    pub argv: Vec<String>,
    #[serde(default = "default_cmd_timeout")]
    pub timeout_seconds: u32,
    /// If false, failure is reported but does not abort the apply.
    #[serde(default = "default_true")]
    pub required: bool,
}

fn default_cmd_timeout() -> u32 {
    10
}

fn default_true() -> bool {
    true
}

fn is_true(v: &bool) -> bool {
    *v
}

/// Network resources applied by planner's network backend; owned and journaled per instance (§9.6, §11.3).
/// There is deliberately no "run this shell command" resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum NetworkResource {
    /// A link planner creates and deletes (dummy/bridge/wireguard). Links created by daemons (tun/tap) are not listed.
    #[serde(rename_all = "camelCase")]
    Link {
        device: String,
        link_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mtu: Option<u32>,
        /// Bring the link up after creation. `false` leaves it down for a guard process to raise (Anycast VIPs).
        #[serde(default = "default_true", skip_serializing_if = "is_true")]
        up: bool,
    },
    /// Address planner assigns to a device; removed only if planner added it.
    #[serde(rename_all = "camelCase")]
    Address { device: String, cidr: String },
    /// MTU / up state of a device created by a daemon.
    #[serde(rename_all = "camelCase")]
    LinkSettings {
        device: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mtu: Option<u32>,
        #[serde(default)]
        up: bool,
    },
    #[serde(rename_all = "camelCase")]
    Route {
        destination: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        table: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metric: Option<u32>,
        /// `local` for TPROXY tables, otherwise unicast.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        route_type: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Rule {
        priority: u32,
        fwmark: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mask: Option<u32>,
        table: u32,
        family: IpFamily,
    },
    /// A complete nftables table owned exclusively by this instance (`meduza_np_<name>`). Applied atomically
    /// with `nft -f`, deleted as a whole. Never touches other tables (§9.6).
    #[serde(rename_all = "camelCase")]
    NftTable { family: String, name: String, content: String },
    #[serde(rename_all = "camelCase")]
    Sysctl { key: String, value: String },
    /// Reservation of a fwmark / routing table number so two features never pick the same one.
    #[serde(rename_all = "camelCase")]
    Reservation { namespace: String, value: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum IpFamily {
    Ipv4,
    Ipv6,
}

/// How planner decides the instance really works, not only "process exists" (§9.4, §9.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ProbeSpec {
    /// Ask the plugin's `probe` operation; the plugin reads native status (wg show dump, FRR JSON, ...).
    #[serde(rename_all = "camelCase")]
    Plugin {
        interval_seconds: u32,
        #[serde(default)]
        params: Value,
    },
    #[serde(rename_all = "camelCase")]
    Tcp { host: String, port: u16, interval_seconds: u32 },
    #[serde(rename_all = "camelCase")]
    Dns { server: String, port: u16, query: String, interval_seconds: u32 },
    #[serde(rename_all = "camelCase")]
    Http {
        url: String,
        interval_seconds: u32,
        #[serde(default)]
        expect_status: Option<u16>,
    },
}

impl GenerationPlan {
    /// Checks paths, modes, placeholders and resource ownership naming.
    pub fn validate(&self) -> Validator {
        let mut v = Validator::new();
        v.check(self.protocol_version == crate::PLUGIN_PROTOCOL_VERSION, "/protocolVersion", "unsupported protocol version");
        v.check(valid_instance_ref(&self.instance), "/instance", "instance must be <package>/<id>");
        let mut seen = std::collections::BTreeSet::new();
        for (i, f) in self.files.iter().enumerate() {
            let path = format!("/files/{i}");
            v.check(valid_relative_path(&f.path), format!("{path}/path"), "path must be relative, normalized and without '..'");
            v.check(seen.insert(f.path.as_str()), format!("{path}/path"), "duplicate file path");
            match parse_mode(&f.mode) {
                Some(mode) => {
                    v.check(mode & 0o7022 == 0, format!("{path}/mode"), "setuid/setgid/sticky and group/world write are not allowed");
                    if f.sensitive {
                        v.check(mode & 0o077 == 0, format!("{path}/mode"), "sensitive files must not be group/world accessible");
                    }
                }
                None => v.error(format!("{path}/mode"), "mode must be an octal string like 0600"),
            }
        }
        let mut ids = std::collections::BTreeSet::new();
        for (i, p) in self.processes.iter().enumerate() {
            let path = format!("/processes/{i}");
            v.check(ids.insert(p.id.as_str()), format!("{path}/id"), "duplicate process id");
            v.check(valid_instance_ref(&p.id), format!("{path}/id"), "process id must be <package>/<id>");
            v.check(p.provider_slot.is_some() != p.program.is_some(), &path, "exactly one of providerSlot or program is required");
            if let Some(program) = &p.program {
                v.check(program.starts_with('/') && !program.contains(".."), format!("{path}/program"), "program must be absolute");
            }
            for (j, arg) in p.argv.iter().enumerate() {
                v.check(placeholders_ok(arg), format!("{path}/argv/{j}"), "unknown placeholder");
            }
            if let Some(b) = &p.bound_to {
                // `ids` holds this and every earlier process: the target must be started before its dependents.
                v.check(
                    *b != p.id && ids.contains(b.as_str()),
                    format!("{path}/boundTo"),
                    "boundTo must name an earlier process of this plan",
                );
            }
        }
        for (i, c) in self.checks.iter().chain(self.reload.iter()).enumerate() {
            let path = format!("/commands/{i}");
            v.check(c.provider_slot.is_some() != c.program.is_some(), &path, "exactly one of providerSlot or program is required");
            for arg in &c.argv {
                v.check(placeholders_ok(arg), format!("{path}/argv"), "unknown placeholder");
            }
        }
        for (i, r) in self.resources.iter().enumerate() {
            let path = format!("/resources/{i}");
            match r {
                NetworkResource::Link { device, .. }
                | NetworkResource::Address { device, .. }
                | NetworkResource::LinkSettings { device, .. } => {
                    v.check(crate::ids::validate_ifname(device).is_ok(), format!("{path}/device"), "invalid interface name");
                }
                NetworkResource::NftTable { family, name, .. } => {
                    v.check(matches!(family.as_str(), "inet" | "ip" | "ip6"), format!("{path}/family"), "family must be inet, ip or ip6");
                    v.check(name.starts_with("meduza_np_"), format!("{path}/name"), "owned nft tables must be named meduza_np_*");
                }
                NetworkResource::Sysctl { key, .. } => {
                    let charset_ok = key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-');
                    v.check(key.starts_with("net.") && charset_ok, format!("{path}/key"), "only net.* sysctls are allowed");
                }
                _ => {}
            }
        }
        v
    }
}

/// `<package>/<instance>[/<sub>]` built from stable ids.
pub fn valid_instance_ref(s: &str) -> bool {
    let parts: Vec<&str> = s.split('/').collect();
    (2..=3).contains(&parts.len()) && parts.iter().all(|p| crate::ids::validate_stable_id(p).is_ok())
}

pub fn valid_relative_path(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p.contains('\0')
        && p.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

pub fn parse_mode(mode: &str) -> Option<u32> {
    if mode.len() == 4 && mode.starts_with('0') {
        u32::from_str_radix(mode, 8).ok()
    } else {
        None
    }
}

fn placeholders_ok(arg: &str) -> bool {
    let mut rest = arg;
    while let Some(start) = rest.find("${") {
        let Some(end) = rest[start..].find('}') else { return false };
        let token = &rest[start..start + end + 1];
        if !PLACEHOLDERS.contains(&token) {
            return false;
        }
        rest = &rest[start + end + 1..];
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spec_plan_valid() {
        let plan: GenerationPlan = serde_json::from_value(json!({
          "protocolVersion": 1,
          "requestId": "req-example",
          "instance": "openvpn/office",
          "files": [{"path": "openvpn/office/config.ovpn", "mode": "0600", "content": "dev tun-office\n", "sensitive": true}],
          "processes": [{
              "id": "openvpn/office", "providerSlot": "openvpn.binary",
              "argv": ["--config", "${GENERATION_ROOT}/openvpn/office/config.ovpn"],
              "restart": "on-failure", "stopTimeoutSeconds": 10
          }],
          "resources": [],
          "dependsOn": []
        }))
        .unwrap();
        assert!(plan.validate().is_empty(), "{:?}", plan.validate().errors());
    }

    #[test]
    fn rejects_escapes_and_bad_modes() {
        let plan: GenerationPlan = serde_json::from_value(json!({
          "protocolVersion": 1, "requestId": "r", "instance": "x/y",
          "files": [
            {"path": "../etc/passwd", "mode": "0644", "content": ""},
            {"path": "a/key", "mode": "0644", "content": "", "sensitive": true},
            {"path": "b", "mode": "4755", "content": ""}
          ],
          "processes": [{"id": "x/y", "program": "/bin/sh", "argv": ["${HOME}"]}],
          "resources": [{"kind": "nft-table", "family": "inet", "name": "fw4", "content": ""},
                        {"kind": "sysctl", "key": "kernel.core_pattern", "value": "|x"}]
        }))
        .unwrap();
        let errs = plan.validate();
        let paths: Vec<_> = errs.errors().iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"/files/0/path"));
        assert!(paths.contains(&"/files/1/mode"));
        assert!(paths.contains(&"/files/2/mode"));
        assert!(paths.contains(&"/processes/0/argv/0"));
        assert!(paths.contains(&"/resources/0/name"));
        assert!(paths.contains(&"/resources/1/key"));
    }

    #[test]
    fn bound_to_names_an_earlier_process() {
        let plan = |procs: serde_json::Value| -> GenerationPlan {
            serde_json::from_value(json!({"protocolVersion": 1, "requestId": "r", "instance": "frr/main", "files": [],
                "processes": procs, "resources": []}))
            .unwrap()
        };
        let p = |id: &str, bound: Option<&str>| json!({"id": id, "program": "/usr/lib/frr/x", "argv": [], "boundTo": bound});
        assert!(plan(json!([p("frr/zebra", None), p("frr/bgpd", Some("frr/zebra"))])).validate().is_empty());
        for bad in [
            json!([p("frr/bgpd", Some("frr/zebra")), p("frr/zebra", None)]),
            json!([p("frr/bgpd", Some("frr/bgpd"))]),
            json!([p("frr/bgpd", Some("frr/none"))]),
        ] {
            let errs = plan(bad).validate();
            assert!(errs.errors().iter().any(|e| e.path.ends_with("/boundTo")), "{:?}", errs.errors());
        }
    }

    #[test]
    fn instance_refs() {
        assert!(valid_instance_ref("wireguard/uplink"));
        assert!(valid_instance_ref("wireguard/uplink/peer-b"));
        assert!(!valid_instance_ref("wireguard"));
        assert!(!valid_instance_ref("a/b/c/d"));
        assert!(!valid_instance_ref("a//b"));
    }
}
