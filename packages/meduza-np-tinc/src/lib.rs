//! Tinc plugin (AGENT.md §9.1, §10.3).
//!
//! One mesh per node (D22): `services.tinc` holds this node's member settings directly (no network list, no
//! netname). Configurations of earlier versions (`services.tinc.instances[0]`) are still read. Internally the mesh is
//! the single instance `tinc/mesh` in its own config directory inside the generation:
//! `tinc.conf`, `hosts/<name>` for this node and every peer (public material only), the private key (0600),
//! `tinc-up`/`tinc-down` (own the interface address — netifd never configures this device) and
//! `host-up`/`host-down` which record peer reachability for the probe. `tincd -c <dir> -D` is supervised;
//! peer additions/removals reload with SIGHUP, own key or core settings changes restart.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use ipnet::IpNet;
use meduza_np_plugin_sdk::{PluginError, PluginRequest};
use meduza_np_protocol::digest::sha256_hex;
use meduza_np_protocol::ids::{derive_ifname, validate_ifname, StableId};
use meduza_np_protocol::plan::{GenerationPlan, PlannedFile, ProbeSpec, ProcessSpec, RestartPolicy};
use meduza_np_protocol::report::{InstanceStatus, ServiceState};
use meduza_np_protocol::{ValidationError, Validator, PLUGIN_PROTOCOL_VERSION};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SLOT: &str = "tinc.binary";

/// Internal view of `services.tinc`: the mesh as (at most) one instance.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TincConfig {
    pub enabled: bool,
    pub instances: Vec<Network>,
    /// Read from the earlier `instances` list shape: problem paths keep its `/instances/<i>` prefix.
    pub legacy: bool,
}

/// `services.tinc` of earlier versions: a list of networks.
#[derive(Deserialize)]
struct LegacyConfig {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    instances: Vec<Network>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Switch,
    Router,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AddressFamily {
    #[default]
    Ipv4,
    Ipv6,
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum KeyFormat {
    /// tinc 1.0 (`PrivateKeyFile`, RSA PEM).
    #[default]
    Rsa,
    /// tinc 1.1 (`Ed25519PrivateKeyFile`, `Ed25519PublicKey`).
    Ed25519,
}

/// This node's member settings of the mesh — the whole of `services.tinc`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(title = "Tinc")]
pub struct Network {
    /// Internal instance id (`mesh`); only the earlier list shape set it.
    #[serde(default = "mesh_id")]
    #[schemars(skip)]
    pub id: StableId,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Earlier list shape only; the single mesh is always `mesh`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub netname: Option<String>,
    /// This node's tinc name: `[A-Za-z0-9_]`. Defaults to the node id normalized (`gw-a` → `gw_a`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub address_family: AddressFamily,
    #[serde(default)]
    pub mode: Mode,
    /// Address configured on the tinc device by `tinc-up`, e.g. `10.42.1.1/24`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<IpNet>,
    /// Subnets this node announces (router mode); its own address is always included.
    #[serde(default)]
    pub subnets: Vec<IpNet>,
    /// Public `host` or `host:port` other members use to reach this node.
    #[serde(default)]
    pub public_addresses: Vec<String>,
    #[serde(default)]
    pub key_format: KeyFormat,
    pub private_key_ref: String,
    /// Public key text (PEM for RSA, base64 for Ed25519); filled by the manager from the key secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cipher: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default)]
    pub tcp_only: bool,
    /// Peers to actively connect to (names). Empty = every peer with a public address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_to: Option<Vec<String>>,
    /// Mesh membership resolved by the manager: public material of every other member.
    #[serde(default)]
    pub hosts: Vec<HostEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HostEntry {
    pub name: String,
    pub public_key: String,
    #[serde(default)]
    pub addresses: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default)]
    pub subnets: Vec<IpNet>,
    /// Managed node id this entry came from (informational).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

fn default_true() -> bool {
    true
}

fn mesh_id() -> StableId {
    StableId::new("mesh").expect("valid id")
}

fn default_port() -> u16 {
    655
}

const CIPHERS: &[&str] = &["aes-256-cbc", "aes-128-cbc", "aes-256-gcm", "chacha20-poly1305", "blowfish", "none"];
const DIGESTS: &[&str] = &["sha256", "sha512", "sha1", "none"];

pub fn valid_tinc_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Normalizes a node id into a tinc name; the manager shows the preview and checks collisions.
pub fn normalize_name(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
}

impl Network {
    pub fn device(&self) -> String {
        self.device.clone().unwrap_or_else(|| derive_ifname("tnc-", self.id.as_str()))
    }
    pub fn name(&self, node_id: &str) -> String {
        self.name.clone().unwrap_or_else(|| normalize_name(node_id))
    }
    pub fn netname(&self) -> String {
        self.netname.clone().unwrap_or_else(|| self.id.to_string())
    }
}

fn valid_address(a: &str) -> bool {
    let host = match a.rsplit_once(' ') {
        Some((h, p)) => {
            if p.parse::<u16>().is_err() {
                return false;
            }
            h
        }
        None => a,
    };
    host.parse::<IpAddr>().is_ok() || meduza_np_protocol::config::valid_hostname(host)
}

/// Tinc's `Address = host [port]` form from `host`, `host:port`, `[v6]:port`.
pub fn tinc_address(a: &str) -> String {
    if let Some(rest) = a.strip_prefix('[') {
        if let Some((h, p)) = rest.split_once("]:") {
            return format!("{h} {p}");
        }
    }
    if a.matches(':').count() == 1 {
        if let Some((h, p)) = a.split_once(':') {
            return format!("{h} {p}");
        }
    }
    a.to_string()
}

pub fn load(req: &PluginRequest) -> Result<TincConfig, PluginError> {
    let Some(raw) = req.service::<Value>("tinc")? else { return Ok(TincConfig::default()) };
    if raw.get("instances").is_some() {
        let l: LegacyConfig = req.service("tinc")?.expect("present");
        return Ok(TincConfig { enabled: l.enabled, instances: l.instances, legacy: true });
    }
    let mesh: Network = req.service("tinc")?.expect("present");
    Ok(TincConfig { enabled: mesh.enabled, instances: vec![mesh], legacy: false })
}

pub fn validate(cfg: &TincConfig, req: &PluginRequest) -> Vec<ValidationError> {
    let mut v = Validator::new();
    let mut ids = BTreeSet::new();
    let mut devices = BTreeSet::new();
    let mut ports = BTreeSet::new();
    for (i, n) in cfg.instances.iter().enumerate() {
        let b = if cfg.legacy { format!("/instances/{i}") } else { String::new() };
        v.check(ids.insert(n.id.clone()), format!("{b}/id"), "duplicate id");
        let own_name = n.name(req.config.node.id.as_str());
        v.check(valid_tinc_name(&own_name), format!("{b}/name"), "tinc names use letters, digits and '_' only");
        v.check(validate_ifname(&n.device()).is_ok(), format!("{b}/device"), "interface name must be 1-15 bytes");
        v.check(devices.insert(n.device()), format!("{b}/device"), "device used by another instance");
        if n.enabled {
            v.check(ports.insert(n.port), format!("{b}/port"), "port used by another instance");
            match req.config.secrets.get(&n.private_key_ref) {
                None => v.error(format!("{b}/privateKeyRef"), format!("secret {} was not delivered to this node", n.private_key_ref)),
                Some(s) => {
                    let ok = match n.key_format {
                        KeyFormat::Rsa => {
                            s.value.contains("-----BEGIN RSA PRIVATE KEY-----") || s.value.contains("-----BEGIN PRIVATE KEY-----")
                        }
                        KeyFormat::Ed25519 => s.value.contains("-----BEGIN ED25519 PRIVATE KEY-----"),
                    };
                    v.check(ok, format!("{b}/privateKeyRef"), format!("secret is not a {:?} private key in tinc format", n.key_format));
                }
            }
        }
        if let Some(a) = n.address {
            let fam_ok = match n.address_family {
                AddressFamily::Ipv4 => a.addr().is_ipv4(),
                AddressFamily::Ipv6 => a.addr().is_ipv6(),
                AddressFamily::Any => true,
            };
            v.check(fam_ok, format!("{b}/address"), "address does not match the address family");
        }
        for (j, a) in n.public_addresses.iter().enumerate() {
            v.check(valid_address(&tinc_address(a)), format!("{b}/publicAddresses/{j}"), "host or host:port expected");
        }
        if let Some(c) = &n.cipher {
            v.check(CIPHERS.contains(&c.as_str()), format!("{b}/cipher"), format!("cipher must be one of {CIPHERS:?}"));
        }
        if let Some(d) = &n.digest {
            v.check(DIGESTS.contains(&d.as_str()), format!("{b}/digest"), format!("digest must be one of {DIGESTS:?}"));
        }
        let mut names = BTreeSet::from([own_name.clone()]);
        for (j, h) in n.hosts.iter().enumerate() {
            let hb = format!("{b}/hosts/{j}");
            v.check(valid_tinc_name(&h.name), format!("{hb}/name"), "invalid tinc name");
            v.check(names.insert(h.name.clone()), format!("{hb}/name"), format!("name {} collides with another member", h.name));
            let key_ok = match n.key_format {
                KeyFormat::Rsa => h.public_key.contains("-----BEGIN RSA PUBLIC KEY-----"),
                KeyFormat::Ed25519 => {
                    !h.public_key.is_empty() && h.public_key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/')
                }
            };
            v.check(key_ok, format!("{hb}/publicKey"), "public key format does not match keyFormat");
            v.check(!h.public_key.contains("PRIVATE"), format!("{hb}/publicKey"), "a private key must never be distributed to other nodes");
            for (k, a) in h.addresses.iter().enumerate() {
                v.check(valid_address(&tinc_address(a)), format!("{hb}/addresses/{k}"), "host or host:port expected");
            }
        }
        if let Some(ct) = &n.connect_to {
            for (j, name) in ct.iter().enumerate() {
                v.check(n.hosts.iter().any(|h| &h.name == name), format!("{b}/connectTo/{j}"), format!("{name} is not a member"));
            }
        }
    }
    v.finish().err().unwrap_or_default()
}

/// Locally administered, unicast MAC for a switch-mode tap, stable per (netname, member name).
pub fn tap_mac(netname: &str, name: &str) -> String {
    let h = meduza_np_protocol::digest::sha256_hex(format!("{netname}/{name}").as_bytes());
    // first octet: 0x02 = locally administered + unicast
    format!("02:{}:{}:{}:{}:{}", &h[0..2], &h[2..4], &h[4..6], &h[6..8], &h[8..10])
}

fn host_file(name_subnets: &[IpNet], addresses: &[String], port: Option<u16>, public_key: &str, format: KeyFormat) -> String {
    let mut out = String::new();
    for a in addresses {
        out.push_str(&format!("Address = {}\n", tinc_address(a)));
    }
    if let Some(p) = port {
        out.push_str(&format!("Port = {p}\n"));
    }
    for s in name_subnets {
        out.push_str(&format!("Subnet = {s}\n"));
    }
    match format {
        KeyFormat::Rsa => {
            out.push('\n');
            out.push_str(public_key.trim());
            out.push('\n');
        }
        KeyFormat::Ed25519 => out.push_str(&format!("Ed25519PublicKey = {}\n", public_key.trim())),
    }
    out
}

fn own_subnets(n: &Network) -> Vec<IpNet> {
    let mut s: Vec<IpNet> = Vec::new();
    if let Some(a) = n.address {
        // The node's own address as a host route; announced subnets come on top.
        let host = IpNet::new(a.addr(), if a.addr().is_ipv4() { 32 } else { 128 }).expect("valid prefix");
        s.push(host);
    }
    for x in &n.subnets {
        if !s.contains(x) {
            s.push(*x);
        }
    }
    s
}

pub fn plan(cfg: &TincConfig, req: &PluginRequest) -> Result<Vec<GenerationPlan>, PluginError> {
    if !cfg.enabled {
        return Ok(Vec::new());
    }
    let mut plans = Vec::new();
    for n in cfg.instances.iter().filter(|n| n.enabled) {
        let dir = format!("tinc/{}", n.id);
        let abs = req.gen_path(&dir);
        let reach_dir = format!("{}/tinc/{}/reach", req.context.run_dir.trim_end_matches('/'), n.id);
        let private = &req.secret(&n.private_key_ref)?.value;
        let key_file = match n.key_format {
            KeyFormat::Rsa => "rsa_key.priv",
            KeyFormat::Ed25519 => "ed25519_key.priv",
        };
        let dev = n.device();
        let own_name = n.name(req.config.node.id.as_str());
        let connect: Vec<&HostEntry> = match &n.connect_to {
            Some(names) => n.hosts.iter().filter(|h| names.contains(&h.name)).collect(),
            None => n.hosts.iter().filter(|h| !h.addresses.is_empty()).collect(),
        };

        let mut conf = String::new();
        conf.push_str(&format!("Name = {own_name}\n"));
        conf.push_str(&format!(
            "AddressFamily = {}\n",
            match n.address_family {
                AddressFamily::Ipv4 => "ipv4",
                AddressFamily::Ipv6 => "ipv6",
                AddressFamily::Any => "any",
            }
        ));
        conf.push_str(&format!("Mode = {}\n", if n.mode == Mode::Switch { "switch" } else { "router" }));
        conf.push_str(&format!("DeviceType = {}\n", if n.mode == Mode::Switch { "tap" } else { "tun" }));
        conf.push_str(&format!("Interface = {dev}\n"));
        conf.push_str(&format!("Port = {}\n", n.port));
        if n.tcp_only {
            conf.push_str("TCPOnly = yes\n");
        }
        if let Some(c) = &n.cipher {
            conf.push_str(&format!("Cipher = {c}\n"));
        }
        if let Some(d) = &n.digest {
            conf.push_str(&format!("Digest = {d}\n"));
        }
        match n.key_format {
            KeyFormat::Rsa => conf.push_str(&format!("PrivateKeyFile = {abs}/{key_file}\n")),
            KeyFormat::Ed25519 => conf.push_str(&format!("Ed25519PrivateKeyFile = {abs}/{key_file}\n")),
        }
        for h in &connect {
            conf.push_str(&format!("ConnectTo = {}\n", h.name));
        }

        let own_public = n.public_key.clone().unwrap_or_default();
        let mut files = vec![
            PlannedFile { path: format!("{dir}/tinc.conf"), mode: "0644".into(), content: conf.clone(), sensitive: false },
            PlannedFile {
                path: format!("{dir}/{key_file}"),
                mode: "0600".into(),
                content: format!("{}\n", private.trim()),
                sensitive: true,
            },
            PlannedFile {
                path: format!("{dir}/hosts/{own_name}"),
                mode: "0644".into(),
                content: host_file(&own_subnets(n), &n.public_addresses, Some(n.port), &own_public, n.key_format),
                sensitive: false,
            },
        ];
        for h in &n.hosts {
            files.push(PlannedFile {
                path: format!("{dir}/hosts/{}", h.name),
                mode: "0644".into(),
                content: host_file(&h.subnets, &h.addresses, h.port, &h.public_key, n.key_format),
                sensitive: false,
            });
        }
        // The tinc instance owns its device address via its scripts; nothing else configures it (§10.3).
        // Switch mode forwards by MAC, so every member's tap needs a distinct one. Hosts derive virtual-interface MACs
        // from machine-id + name (systemd MACAddressPolicy=persistent): cloned machines get identical taps and the
        // mesh silently drops traffic. The MAC is therefore set explicitly, derived from netname + member name.
        let mac = match n.mode {
            Mode::Switch => {
                let m = tap_mac(&n.netname(), &own_name);
                format!(
                    "if [ \"$(cat /sys/class/net/$INTERFACE/address)\" != \"{m}\" ]; then\n\
                     \tip link set dev \"$INTERFACE\" down\n\
                     \tip link set dev \"$INTERFACE\" address {m} || echo \"tinc-up: cannot set MAC {m} on $INTERFACE\" >&2\n\
                     fi\n"
                )
            }
            Mode::Router => String::new(),
        };
        let (up, down) = match n.address {
            Some(a) => (
                format!("#!/bin/sh\n{mac}ip addr replace {a} dev \"$INTERFACE\"\nip link set dev \"$INTERFACE\" up\n"),
                format!("#!/bin/sh\nip addr del {a} dev \"$INTERFACE\" 2>/dev/null\nip link set dev \"$INTERFACE\" down\nexit 0\n"),
            ),
            None => (format!("#!/bin/sh\n{mac}ip link set dev \"$INTERFACE\" up\n"), "#!/bin/sh\nexit 0\n".to_string()),
        };
        files.push(PlannedFile { path: format!("{dir}/tinc-up"), mode: "0750".into(), content: up, sensitive: false });
        files.push(PlannedFile { path: format!("{dir}/tinc-down"), mode: "0750".into(), content: down, sensitive: false });
        let record = |state: &str| {
            format!(
                "#!/bin/sh\nd='{reach_dir}'\nmkdir -p \"$d\"\nprintf '{state} %s %s %s\\n' \"$(date +%s)\" \"$REMOTEADDRESS\" \"$REMOTEPORT\" > \"$d/.$NODE\" && mv \"$d/.$NODE\" \"$d/$NODE\"\n"
            )
        };
        files.push(PlannedFile { path: format!("{dir}/host-up"), mode: "0750".into(), content: record("up"), sensitive: false });
        files.push(PlannedFile { path: format!("{dir}/host-down"), mode: "0750".into(), content: record("down"), sensitive: false });

        // Restart only for changes tincd cannot pick up with SIGHUP: core settings, own key, own host file, and the
        // device scripts (tinc-up runs only when tincd creates the device: address and MAC changes need a restart).
        let up_down: String =
            files.iter().filter(|f| f.path.ends_with("/tinc-up") || f.path.ends_with("/tinc-down")).map(|f| f.content.as_str()).collect();
        let restart_digest = sha256_hex(format!("{conf}\n{}\n{}\n{up_down}", sha256_hex(private.as_bytes()), files[2].content).as_bytes());
        plans.push(GenerationPlan {
            protocol_version: PLUGIN_PROTOCOL_VERSION,
            request_id: req.request_id.clone(),
            instance: format!("tinc/{}", n.id),
            files,
            processes: vec![ProcessSpec {
                id: format!("tinc/{}", n.id),
                provider_slot: Some(SLOT.into()),
                program: None,
                argv: vec![
                    "-c".into(),
                    format!("${{GENERATION_ROOT}}/{dir}"),
                    "-D".into(),
                    format!("--pidfile=${{RUN_DIR}}/tinc-{}.pid", n.id),
                ],
                env: BTreeMap::new(),
                restart: RestartPolicy::OnFailure,
                startup: Default::default(),
                stop_timeout_seconds: 10,
                reload_signal: Some("SIGHUP".into()),
                restart_digest: Some(restart_digest),
                user: None,
                bound_to: None,
            }],
            resources: Vec::new(),
            depends_on: Vec::new(),
            probes: vec![ProbeSpec::Plugin { interval_seconds: 30, params: Value::Null }],
            checks: Vec::new(),
            reload: Vec::new(),
        });
    }
    Ok(plans)
}

/// One reachability record written by host-up/host-down: `up|down <unix> <address> <port>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reach {
    pub up: bool,
    pub at: u64,
    pub address: Option<String>,
}

pub fn parse_reach(text: &str) -> Option<Reach> {
    let mut f = text.split_whitespace();
    let up = match f.next()? {
        "up" => true,
        "down" => false,
        _ => return None,
    };
    let at = f.next()?.parse().ok()?;
    let address = f.next().filter(|a| !a.is_empty() && *a != "unknown").map(str::to_string);
    Some(Reach { up, at, address })
}

/// Status per instance and per peer from the process state and the reachability records.
pub fn statuses(cfg: &TincConfig, reach: &BTreeMap<String, BTreeMap<String, Reach>>) -> Vec<InstanceStatus> {
    let mut out = Vec::new();
    for n in cfg.instances.iter().filter(|n| n.enabled && cfg.enabled) {
        let records = reach.get(n.id.as_str());
        for h in &n.hosts {
            let r = records.and_then(|m| m.get(&h.name));
            let mut detail = BTreeMap::new();
            detail.insert("peer".into(), Value::String(h.name.clone()));
            let (state, code) = match r {
                Some(r) if r.up => (ServiceState::Up, "reachable"),
                Some(_) => (ServiceState::Down, "unreachable"),
                None => (ServiceState::Unknown, "never-seen"),
            };
            if let Some(r) = r {
                detail.insert("lastChange".into(), Value::String(meduza_np_plugin_sdk::unix_to_rfc3339(r.at)));
                if let Some(a) = &r.address {
                    detail.insert("remoteAddress".into(), Value::String(a.clone()));
                }
            }
            // tinc 1.0 does not expose whether the path is direct; reachability may be relayed.
            detail.insert("direct".into(), Value::String("unknown".into()));
            detail.insert("reasonCode".into(), Value::String(code.into()));
            out.push(InstanceStatus {
                id: format!("tinc/{}/{}", n.id, h.name.to_ascii_lowercase().replace('_', "-")),
                kind: "tinc-peer".into(),
                state,
                observed_at: Some(meduza_np_plugin_sdk::now_rfc3339()),
                reason: None,
                detail,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_mode_taps_get_distinct_stable_macs() {
        let a = tap_mac("mesh", "lab_a");
        assert_eq!(a, tap_mac("mesh", "lab_a"), "stable");
        assert_ne!(a, tap_mac("mesh", "lab_b"), "distinct per member");
        assert_ne!(a, tap_mac("other", "lab_a"), "distinct per network");
        assert!(a.starts_with("02:") && a.len() == 17, "{a}");
    }
    use meduza_np_plugin_sdk::test_request;
    use serde_json::json;

    const RSA_PRIV: &str = "-----BEGIN RSA PRIVATE KEY-----\nMIIBOgIBAAJBALtest\n-----END RSA PRIVATE KEY-----";
    const RSA_PUB_A: &str = "-----BEGIN RSA PUBLIC KEY-----\nMEgCQQC7a\n-----END RSA PUBLIC KEY-----";
    const RSA_PUB_B: &str = "-----BEGIN RSA PUBLIC KEY-----\nMEgCQQC7b\n-----END RSA PUBLIC KEY-----";

    fn req() -> PluginRequest {
        test_request(
            "plan",
            json!({
                "services": {"tinc": {"instances": [{
                    "id": "mesh", "name": "gwA", "device": "tnc0", "port": 655, "address": "10.42.1.1/24",
                    "publicAddresses": ["gw-a.example.test"], "privateKeyRef": "tinc-local", "publicKey": RSA_PUB_A, "tcpOnly": true,
                    "hosts": [{"name": "gwB", "publicKey": RSA_PUB_B, "addresses": ["198.51.100.2:655"], "subnets": ["10.42.1.2/32"]}]
                }]}},
                "secrets": {"tinc-local": {"kind": "tinc-rsa-private", "value": RSA_PRIV}}
            }),
        )
    }

    #[test]
    fn single_mesh_shape_plans_like_the_earlier_list() {
        // D22: `services.tinc` is the mesh itself; the earlier `instances[0]` shape stays readable.
        let legacy = req();
        let mut flat = legacy.clone();
        let mesh = legacy.config.services["tinc"]["instances"][0].clone();
        let mut mesh = mesh.as_object().unwrap().clone();
        mesh.remove("id");
        flat.config.services.insert("tinc".into(), Value::Object(mesh));
        let a = plan(&load(&legacy).unwrap(), &legacy).unwrap();
        let b = plan(&load(&flat).unwrap(), &flat).unwrap();
        assert_eq!(a, b, "same instance tinc/mesh, same files");
        assert!(!load(&flat).unwrap().legacy);
        // problem paths point at services.tinc itself
        flat.config.services.get_mut("tinc").unwrap()["publicAddresses"] = json!(["not a host!"]);
        let errs = validate(&load(&flat).unwrap(), &flat);
        assert!(errs.iter().any(|e| e.path == "/publicAddresses/0"), "{errs:?}");
    }

    #[test]
    fn renders_spec_example() {
        let r = req();
        let cfg = load(&r).unwrap();
        assert!(validate(&cfg, &r).is_empty(), "{:?}", validate(&cfg, &r));
        let plans = plan(&cfg, &r).unwrap();
        let p = &plans[0];
        assert!(p.validate().is_empty(), "{:?}", p.validate().errors());
        let conf = &p.files[0].content;
        for line in [
            "Name = gwA",
            "AddressFamily = ipv4",
            "Mode = switch",
            "DeviceType = tap",
            "Interface = tnc0",
            "Port = 655",
            "TCPOnly = yes",
            "ConnectTo = gwB",
        ] {
            assert!(conf.contains(line), "missing {line}:\n{conf}");
        }
        assert_eq!(p.files[1].mode, "0600");
        assert!(p.files[1].sensitive);
        let peer = p.files.iter().find(|f| f.path == "tinc/mesh/hosts/gwB").unwrap();
        assert!(peer.content.starts_with("Address = 198.51.100.2 655\nSubnet"), "{}", peer.content);
        assert!(peer.content.contains("Subnet = 10.42.1.2/32"));
        assert!(!peer.content.contains("PRIVATE"));
        let own = p.files.iter().find(|f| f.path == "tinc/mesh/hosts/gwA").unwrap();
        assert!(own.content.contains("Subnet = 10.42.1.1/32"));
        let up = p.files.iter().find(|f| f.path == "tinc/mesh/tinc-up").unwrap();
        assert!(up.content.contains("ip addr replace 10.42.1.1/24 dev \"$INTERFACE\""));
        assert_eq!(p.processes[0].reload_signal.as_deref(), Some("SIGHUP"));
    }

    #[test]
    fn adding_a_peer_reloads_instead_of_restarting() {
        let r = req();
        let a = plan(&load(&r).unwrap(), &r).unwrap();
        let mut r2 = r.clone();
        r2.config.services.get_mut("tinc").unwrap()["instances"][0]["hosts"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name": "gwC", "publicKey": RSA_PUB_B, "addresses": []}));
        let b = plan(&load(&r2).unwrap(), &r2).unwrap();
        assert_eq!(a[0].processes[0].restart_digest, b[0].processes[0].restart_digest, "new peer → SIGHUP, not restart");
        let mut r3 = r.clone();
        r3.config.services.get_mut("tinc").unwrap()["instances"][0]["port"] = json!(656);
        let c = plan(&load(&r3).unwrap(), &r3).unwrap();
        assert_ne!(a[0].processes[0].restart_digest, c[0].processes[0].restart_digest, "core change → restart");
    }

    #[test]
    fn rejects_collisions_and_leaked_private_keys() {
        let mut r = req();
        r.config.services.get_mut("tinc").unwrap()["instances"][0]["hosts"] = json!([
            {"name": "gwA", "publicKey": RSA_PUB_B},
            {"name": "gw-b", "publicKey": RSA_PRIV}
        ]);
        let text = format!("{:?}", validate(&load(&r).unwrap(), &r));
        assert!(text.contains("collides"), "{text}");
        assert!(text.contains("invalid tinc name"), "{text}");
        assert!(text.contains("never be distributed"), "{text}");
    }

    #[test]
    fn reachability_status() {
        let r = req();
        let cfg = load(&r).unwrap();
        let mut reach = BTreeMap::new();
        reach.insert("mesh".to_string(), BTreeMap::from([("gwB".to_string(), parse_reach("up 1700000000 198.51.100.2 655").unwrap())]));
        let st = statuses(&cfg, &reach);
        assert_eq!(st[0].state, ServiceState::Up);
        assert_eq!(st[0].detail["remoteAddress"], json!("198.51.100.2"));
        let none = statuses(&cfg, &BTreeMap::new());
        assert_eq!(none[0].state, ServiceState::Unknown, "never seen is not up");
        assert_eq!(normalize_name("gw-a"), "gw_a");
        assert_eq!(tinc_address("[2001:db8::1]:655"), "2001:db8::1 655");
        assert_eq!(tinc_address("host.example:700"), "host.example 700");
    }

    #[test]
    fn config_schema_is_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/config.json");
        meduza_np_protocol::schema::check_file::<Network>(&path).unwrap();
    }
}
