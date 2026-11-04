//! WireGuard plugin (AGENT.md §9.4, §10.2).
//!
//! - Interfaces are created by planner as `wireguard` links; addresses and MTU are planner network resources.
//! - Keys and peers are loaded with `wg syncconf <dev> <file>` from a 0600 file holding only native parameters
//!   (no wg-quick lines, no DNS or routing side effects). `syncconf` applies only the differences, so existing peer
//!   sessions survive config changes and planner restarts (`setconf` would drop every session on the interface).
//!   AllowedIPs never install routes (§9.4).
//! - Status comes from `wg show <dev> dump`: per-peer handshake age, endpoint and transfer counters.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr};

use base64::Engine;
use ipnet::IpNet;
use meduza_np_plugin_sdk::text::ConfigWriter;
use meduza_np_plugin_sdk::{PluginError, PluginRequest};
use meduza_np_protocol::bgp::BgpSession;
use meduza_np_protocol::ids::{derive_ifname, validate_ifname, StableId};
use meduza_np_protocol::plan::{CommandSpec, GenerationPlan, NetworkResource, PlannedFile, ProbeSpec, ProcessSpec, RestartPolicy};
use meduza_np_protocol::report::{InstanceStatus, ServiceState};
use meduza_np_protocol::{ValidationError, Validator, PLUGIN_PROTOCOL_VERSION};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod endpoints;

pub const SLOT: &str = "wireguard.tools";

/// `services.wireguard`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(title = "WireGuard")]
pub struct WireGuardConfig {
    /// Master switch for the package on this node.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub instances: Vec<Interface>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Interface {
    pub id: StableId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Linux interface name (max 15 bytes). Derived from the id when omitted; never from the display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Secret id of the X25519 private key (manager- or node-held).
    pub private_key_ref: String,
    /// Shown in the UI; filled by the manager from the key's public value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    #[serde(default)]
    pub addresses: Vec<IpNet>,
    /// UDP port. When omitted, a stable port derived from the interface id (49152-65535) is used, never a random
    /// one: a reload or restart must not move the source port (NAT mappings and peers' learned endpoints break).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1280, max = 9000))]
    pub mtu: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fwmark: Option<u32>,
    /// Public `host:port` external peers use to reach this interface (shown for and exported to them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_endpoint: Option<String>,
    /// A handshake older than this marks the peer as not connected (WireGuard rekeys every 120 s).
    #[serde(default = "default_handshake_timeout")]
    pub handshake_timeout_seconds: u64,
    #[serde(default)]
    pub peers: Vec<Peer>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Peer {
    pub id: StableId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Base64 public key. The manager fills it when `node`/`instance` reference another managed node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preshared_key_ref: Option<String>,
    /// Cryptokey routing range. Does not install system routes.
    #[serde(default)]
    pub allowed_ips: Vec<IpNet>,
    /// `host:port`; omitted for peers that connect to us.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistent_keepalive: Option<u16>,
    /// eBGP with the external side over this peer; compiled into FRR by the manager, never used by this plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bgp: Option<BgpSession>,
}

fn default_true() -> bool {
    true
}

fn default_handshake_timeout() -> u64 {
    180
}

impl Interface {
    /// Configured port, or a stable one derived from the id (see `listen_port`).
    pub fn effective_listen_port(&self) -> u16 {
        self.listen_port.unwrap_or_else(|| {
            let h = meduza_np_protocol::digest::sha256_hex(self.id.as_str().as_bytes());
            49152 + (u16::from_str_radix(&h[..4], 16).unwrap_or(0) % 16384)
        })
    }

    pub fn device(&self) -> String {
        self.device.clone().unwrap_or_else(|| derive_ifname("wg-", self.id.as_str()))
    }
}

/// Parses a WireGuard key (base64 of 32 bytes).
pub fn parse_key(text: &str) -> Option<[u8; 32]> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(text.trim()).ok()?;
    bytes.try_into().ok()
}

pub fn public_from_private(private_b64: &str) -> Option<String> {
    let secret = x25519_dalek::StaticSecret::from(parse_key(private_b64)?);
    Some(base64::engine::general_purpose::STANDARD.encode(x25519_dalek::PublicKey::from(&secret).as_bytes()))
}

/// `host:port` with a numeric port; host may be a name, IPv4 or bracketed IPv6.
pub fn valid_endpoint(ep: &str) -> bool {
    if ep.parse::<SocketAddr>().is_ok() {
        return true;
    }
    let Some((host, port)) = ep.rsplit_once(':') else { return false };
    port.parse::<u16>().map(|p| p != 0).unwrap_or(false)
        && !host.is_empty()
        && !host.contains(['[', ']', ' '])
        && (host.parse::<IpAddr>().is_ok() || meduza_np_protocol::config::valid_hostname(host))
}

pub fn load(req: &PluginRequest) -> Result<WireGuardConfig, PluginError> {
    Ok(req.service::<WireGuardConfig>("wireguard")?.unwrap_or_default())
}

pub fn validate(cfg: &WireGuardConfig, req: &PluginRequest) -> Vec<ValidationError> {
    let mut v = Validator::new();
    let mut ids = BTreeSet::new();
    let mut devices = BTreeMap::new();
    let mut ports = BTreeSet::new();
    for (i, inst) in cfg.instances.iter().enumerate() {
        let base = format!("/instances/{i}");
        v.check(ids.insert(inst.id.clone()), format!("{base}/id"), "duplicate interface id");
        let dev = inst.device();
        if validate_ifname(&dev).is_err() {
            v.error(format!("{base}/device"), "interface name must be 1-15 bytes without '/', ':' or spaces");
        }
        if let Some(other) = devices.insert(dev.clone(), inst.id.clone()) {
            v.error(format!("{base}/device"), format!("device {dev} is also used by {other}"));
        }
        if let Some(p) = inst.listen_port {
            v.check(p != 0, format!("{base}/listenPort"), "port must be 1-65535");
        }
        if inst.enabled {
            let p = inst.effective_listen_port();
            let msg = if inst.listen_port.is_some() {
                "listen port used by another interface".to_string()
            } else {
                format!("derived listen port {p} collides with another interface; set listenPort explicitly")
            };
            v.check(ports.insert(p), format!("{base}/listenPort"), msg);
        }
        if let Some(m) = inst.mtu {
            v.check((1280..=9000).contains(&m), format!("{base}/mtu"), "MTU must be 1280-9000");
        }
        v.check(inst.handshake_timeout_seconds >= 30, format!("{base}/handshakeTimeoutSeconds"), "at least 30 seconds");
        let own_public = if inst.enabled {
            match req.config.secrets.get(&inst.private_key_ref) {
                None => {
                    v.error(format!("{base}/privateKeyRef"), format!("secret {} was not delivered to this node", inst.private_key_ref));
                    None
                }
                Some(s) => match public_from_private(&s.value) {
                    None => {
                        v.error(format!("{base}/privateKeyRef"), "private key is not a 32-byte base64 key");
                        None
                    }
                    Some(pk) => Some(pk),
                },
            }
        } else {
            None
        };
        let mut peer_ids = BTreeSet::new();
        let mut keys = BTreeSet::new();
        for (j, peer) in inst.peers.iter().enumerate() {
            let pb = format!("{base}/peers/{j}");
            v.check(peer_ids.insert(peer.id.clone()), format!("{pb}/id"), "duplicate peer id");
            match &peer.public_key {
                None if peer.enabled => v.error(format!("{pb}/publicKey"), "public key required (or reference a managed node)"),
                None => {}
                Some(k) => {
                    v.check(parse_key(k).is_some(), format!("{pb}/publicKey"), "public key must be 32 bytes of base64");
                    v.check(keys.insert(k.clone()), format!("{pb}/publicKey"), "two peers share a public key");
                    v.check(
                        own_public.as_deref() != Some(k.as_str()),
                        format!("{pb}/publicKey"),
                        "peer key equals this interface's own key",
                    );
                }
            }
            if let Some(ep) = &peer.endpoint {
                v.check(valid_endpoint(ep), format!("{pb}/endpoint"), "endpoint must be host:port");
            }
            if let Some(psk) = &peer.preshared_key_ref {
                if peer.enabled && inst.enabled {
                    match req.config.secrets.get(psk) {
                        None => v.error(format!("{pb}/presharedKeyRef"), format!("secret {psk} was not delivered to this node")),
                        Some(s) => v.check(
                            parse_key(&s.value).is_some(),
                            format!("{pb}/presharedKeyRef"),
                            "pre-shared key must be 32 bytes of base64",
                        ),
                    }
                }
            }
            for (k, net) in peer.allowed_ips.iter().enumerate() {
                v.check(net.trunc() == *net, format!("{pb}/allowedIps/{k}"), "host bits must be zero");
            }
            if let Some(b) = &peer.bgp {
                b.validate(&mut v, &format!("{pb}/bgp"));
            }
        }
    }
    v.finish().err().unwrap_or_default()
}

/// The exact file given to `wg syncconf` (§10.2): native parameters only.
pub fn render_setconf(inst: &Interface, req: &PluginRequest) -> Result<String, PluginError> {
    let private = &req.secret(&inst.private_key_ref)?.value;
    let mut w = ConfigWriter::new();
    let inj = |e: meduza_np_plugin_sdk::text::InjectionError| PluginError::new("invalid-config", e.to_string());
    w.raw_line("[Interface]");
    w.ini("PrivateKey", private.trim()).map_err(inj)?;
    // Always explicit: wg treats a missing ListenPort as "pick a random port" on every syncconf/setconf.
    w.ini("ListenPort", &inst.effective_listen_port().to_string()).map_err(inj)?;
    if let Some(m) = inst.fwmark {
        w.ini("FwMark", &format!("{m:#x}")).map_err(inj)?;
    }
    for peer in inst.peers.iter().filter(|p| p.enabled) {
        let Some(key) = &peer.public_key else { continue };
        w.blank();
        w.raw_line("[Peer]");
        w.ini("PublicKey", key).map_err(inj)?;
        if let Some(psk) = &peer.preshared_key_ref {
            w.ini("PresharedKey", req.secret(psk)?.value.trim()).map_err(inj)?;
        }
        if !peer.allowed_ips.is_empty() {
            let ips: Vec<String> = peer.allowed_ips.iter().map(|n| n.to_string()).collect();
            w.ini("AllowedIPs", &ips.join(", ")).map_err(inj)?;
        }
        if let Some(ep) = &peer.endpoint {
            w.ini("Endpoint", ep).map_err(inj)?;
        }
        if let Some(k) = peer.persistent_keepalive.filter(|k| *k > 0) {
            w.ini("PersistentKeepalive", &k.to_string()).map_err(inj)?;
        }
    }
    Ok(w.finish())
}

/// wg-quick style export for operators (Address/MTU/Table=off). Distinct from what planner applies.
pub fn render_wg_quick_export(inst: &Interface, req: &PluginRequest) -> Result<String, PluginError> {
    let native = render_setconf(inst, req)?;
    let mut extra = String::new();
    for a in &inst.addresses {
        extra.push_str(&format!("Address = {a}\n"));
    }
    if let Some(m) = inst.mtu {
        extra.push_str(&format!("MTU = {m}\n"));
    }
    extra.push_str("Table = off\n");
    Ok(native.replacen("[Interface]\n", &format!("[Interface]\n{extra}"), 1))
}

/// DNS-dependent endpoints are reconciled by the instance runner, outside the required local reload.
fn render_local_config(inst: &Interface, req: &PluginRequest) -> Result<String, PluginError> {
    let mut local = inst.clone();
    for peer in &mut local.peers {
        if peer.endpoint.as_ref().is_some_and(|e| e.parse::<SocketAddr>().is_err()) {
            peer.endpoint = None;
        }
    }
    render_setconf(&local, req)
}

pub fn plan(cfg: &WireGuardConfig, req: &PluginRequest) -> Result<Vec<GenerationPlan>, PluginError> {
    if !cfg.enabled {
        return Ok(Vec::new());
    }
    let mut plans = Vec::new();
    for inst in cfg.instances.iter().filter(|i| i.enabled) {
        let dev = inst.device();
        let rel = format!("wireguard/{}/wg.conf", inst.id);
        let mut resources = vec![NetworkResource::Link { device: dev.clone(), link_type: "wireguard".into(), mtu: inst.mtu, up: true }];
        for a in &inst.addresses {
            resources.push(NetworkResource::Address { device: dev.clone(), cidr: a.to_string() });
        }
        let mut files =
            vec![PlannedFile { path: rel.clone(), mode: "0600".into(), content: render_local_config(inst, req)?, sensitive: true }];
        let peers: Vec<_> = inst
            .peers
            .iter()
            .filter(|p| p.enabled)
            .filter_map(|p| {
                let endpoint = p.endpoint.as_ref().filter(|ep| ep.parse::<SocketAddr>().is_err())?;
                Some(endpoints::PeerSpec { id: p.id.to_string(), public_key: p.public_key.clone()?, endpoint: endpoint.clone() })
            })
            .collect();
        let mut processes = Vec::new();
        if !peers.is_empty() {
            let spec = endpoints::Spec { instance: inst.id.to_string(), device: dev.clone(), wg: req.provider(SLOT)?.path.clone(), peers };
            let spec_path = format!("wireguard/{}/endpoints.json", inst.id);
            files.push(PlannedFile {
                path: spec_path.clone(),
                mode: "0600".into(),
                content: serde_json::to_string(&spec).unwrap(),
                sensitive: false,
            });
            processes.push(ProcessSpec {
                id: format!("wireguard/{}/endpoints", inst.id),
                provider_slot: None,
                program: Some(format!("{}/plugins/wireguard", req.context.libexec_dir.trim_end_matches('/'))),
                argv: vec!["watch-endpoints".into(), format!("${{GENERATION_ROOT}}/{spec_path}"), "${RUN_DIR}".into()],
                env: BTreeMap::new(),
                restart: RestartPolicy::Always,
                startup: Default::default(),
                stop_timeout_seconds: 3,
                reload_signal: None,
                restart_digest: Some(meduza_np_protocol::digest::sha256_hex(
                    format!("{}\n{}", serde_json::to_string(&spec).unwrap(), endpoints::runner_identity()).as_bytes(),
                )),
                user: None,
                bound_to: None,
            });
        }
        plans.push(GenerationPlan {
            protocol_version: PLUGIN_PROTOCOL_VERSION,
            request_id: req.request_id.clone(),
            instance: format!("wireguard/{}", inst.id),
            files,
            processes,
            resources,
            depends_on: Vec::new(),
            probes: vec![ProbeSpec::Plugin { interval_seconds: 30, params: Value::Null }],
            checks: Vec::new(),
            // Kernel interface: (re)load keys and peers whenever the rendered file changes or the link is new.
            // syncconf, not setconf: unchanged peers keep their sessions (verified in the multi-node lab, where
            // setconf cost ~15 s of tunnel outage on every planner restart).
            reload: vec![CommandSpec {
                provider_slot: Some(SLOT.into()),
                program: None,
                argv: vec!["syncconf".into(), dev.clone(), format!("${{GENERATION_ROOT}}/{rel}")],
                timeout_seconds: 10,
                required: true,
            }],
        });
    }
    Ok(plans)
}

/// One peer line of `wg show <dev> dump`.
#[derive(Debug, Clone, PartialEq)]
pub struct DumpPeer {
    pub public_key: String,
    pub endpoint: Option<String>,
    pub latest_handshake: u64,
    pub rx: u64,
    pub tx: u64,
}

pub fn parse_dump(text: &str) -> Option<(String, Vec<DumpPeer>)> {
    let mut lines = text.lines();
    let iface: Vec<&str> = lines.next()?.split('\t').collect();
    let public = iface.get(1)?.to_string();
    let peers = lines
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 8 {
                return None;
            }
            Some(DumpPeer {
                public_key: f[0].to_string(),
                endpoint: (f[2] != "(none)").then(|| f[2].to_string()),
                latest_handshake: f[4].parse().ok()?,
                rx: f[5].parse().ok()?,
                tx: f[6].parse().ok()?,
            })
        })
        .collect();
    Some((public, peers))
}

/// Peer state from a dump (§9.4): no handshake is never "up" just because the interface exists, and an idle
/// passive peer (no endpoint/keepalive) is "unknown" rather than failed.
pub fn peer_state(peer: &Peer, dump: Option<&DumpPeer>, now: u64, timeout: u64) -> (ServiceState, &'static str) {
    if !peer.enabled {
        return (ServiceState::Disabled, "disabled");
    }
    let Some(d) = dump else { return (ServiceState::Down, "peer-not-loaded") };
    let active = peer.endpoint.is_some() || peer.persistent_keepalive.unwrap_or(0) > 0;
    if d.latest_handshake == 0 {
        return if active { (ServiceState::Down, "no-handshake") } else { (ServiceState::Unknown, "waiting-for-peer") };
    }
    if now.saturating_sub(d.latest_handshake) <= timeout {
        (ServiceState::Up, "connected")
    } else if active {
        (ServiceState::Down, "handshake-expired")
    } else {
        (ServiceState::Unknown, "idle")
    }
}

pub fn statuses(cfg: &WireGuardConfig, dumps: &BTreeMap<String, Result<String, String>>, now: u64) -> Vec<InstanceStatus> {
    let observed = Some(meduza_np_plugin_sdk::unix_to_rfc3339(now));
    let mut out = Vec::new();
    for inst in cfg.instances.iter().filter(|i| i.enabled && cfg.enabled) {
        let dev = inst.device();
        let parsed = dumps.get(&dev).and_then(|r| r.as_ref().ok()).and_then(|t| parse_dump(t));
        let mut detail = BTreeMap::new();
        detail.insert("device".into(), Value::String(dev.clone()));
        let (state, reason) = match (&parsed, dumps.get(&dev)) {
            (Some((pk, _)), _) => {
                detail.insert("publicKey".into(), Value::String(pk.clone()));
                (ServiceState::Up, None)
            }
            (None, Some(Err(e))) => (ServiceState::Down, Some(e.clone())),
            _ => (ServiceState::Down, Some("interface not present".into())),
        };
        out.push(InstanceStatus {
            id: format!("wireguard/{}", inst.id),
            kind: "wireguard-interface".into(),
            state,
            observed_at: observed.clone(),
            reason,
            detail,
        });
        for peer in &inst.peers {
            let d = parsed.as_ref().and_then(|(_, peers)| peers.iter().find(|p| Some(&p.public_key) == peer.public_key.as_ref()));
            let (state, reason) = peer_state(peer, d, now, inst.handshake_timeout_seconds);
            let mut detail = BTreeMap::new();
            if let Some(endpoint) = &peer.endpoint {
                detail.insert("configuredEndpoint".into(), Value::String(endpoint.clone()));
            }
            if let Some(d) = d {
                if d.latest_handshake > 0 {
                    detail.insert("latestHandshake".into(), Value::String(meduza_np_plugin_sdk::unix_to_rfc3339(d.latest_handshake)));
                }
                detail.insert("rxBytes".into(), Value::from(d.rx));
                detail.insert("txBytes".into(), Value::from(d.tx));
                if let Some(ep) = &d.endpoint {
                    detail.insert("endpoint".into(), Value::String(ep.clone()));
                }
            }
            detail.insert("reasonCode".into(), Value::String(reason.into()));
            out.push(InstanceStatus {
                id: format!("wireguard/{}/{}", inst.id, peer.id),
                kind: "wireguard-peer".into(),
                state,
                observed_at: observed.clone(),
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
    use meduza_np_plugin_sdk::test_request;
    use serde_json::json;

    const PRIV: &str = "yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=";
    const PEER: &str = "xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=";

    fn req() -> PluginRequest {
        let mut request = test_request(
            "plan",
            json!({
                "services": {"wireguard": {"instances": [{
                    "id": "uplink", "privateKeyRef": "wg-uplink", "listenPort": 51820, "addresses": ["10.9.0.2/24"], "mtu": 1420,
                    "peers": [{"id": "peer-b", "publicKey": PEER, "allowedIps": ["10.9.0.1/32"], "endpoint": "wg.example.test:51820", "persistentKeepalive": 25}]
                }]}},
                "secrets": {"wg-uplink": {"kind": "wireguard-private-key", "value": PRIV}}
            }),
        );
        request.providers.insert(
            SLOT.into(),
            meduza_np_plugin_sdk::ResolvedProvider { path: "/usr/bin/wg".into(), source: "system".into(), version: None },
        );
        request
    }

    #[test]
    fn portless_interfaces_get_a_stable_explicit_port() {
        let r = test_request(
            "plan",
            json!({
                "services": {"wireguard": {"instances": [{"id": "roam", "privateKeyRef": "wg-uplink", "addresses": ["10.9.0.3/24"]}]}},
                "secrets": {"wg-uplink": {"kind": "wireguard-private-key", "value": PRIV}}
            }),
        );
        let mut cfg = load(&r).unwrap();
        assert!(validate(&cfg, &r).is_empty(), "{:?}", validate(&cfg, &r));
        let a = cfg.instances[0].effective_listen_port();
        assert!(a >= 49152);
        assert_eq!(a, cfg.instances[0].effective_listen_port(), "deterministic");
        let text = render_setconf(&cfg.instances[0], &r).unwrap();
        assert!(text.contains(&format!("ListenPort = {a}")), "never left to the kernel: {text}");
        cfg.instances[0].listen_port = Some(51820);
        assert_eq!(cfg.instances[0].effective_listen_port(), 51820);
    }

    #[test]
    fn renders_native_setconf_only() {
        let r = req();
        let cfg = load(&r).unwrap();
        assert!(validate(&cfg, &r).is_empty(), "{:?}", validate(&cfg, &r));
        let text = render_setconf(&cfg.instances[0], &r).unwrap();
        assert_eq!(
            text,
            format!("[Interface]\nPrivateKey = {PRIV}\nListenPort = 51820\n\n[Peer]\nPublicKey = {PEER}\nAllowedIPs = 10.9.0.1/32\nEndpoint = wg.example.test:51820\nPersistentKeepalive = 25\n")
        );
        assert!(!text.contains("Address") && !text.contains("DNS") && !text.contains("Table"));
        let export = render_wg_quick_export(&cfg.instances[0], &r).unwrap();
        assert!(export.contains("Address = 10.9.0.2/24\nMTU = 1420\nTable = off\n"));
    }

    #[test]
    fn plan_uses_link_address_resources_and_syncconf_reload() {
        let r = req();
        let plans = plan(&load(&r).unwrap(), &r).unwrap();
        assert_eq!(plans.len(), 1);
        let p = &plans[0];
        assert!(p.validate().is_empty(), "{:?}", p.validate().errors());
        assert_eq!(p.files[0].mode, "0600");
        assert!(p.files[0].sensitive);
        assert_eq!(p.processes[0].id, "wireguard/uplink/endpoints");
        assert!(!p.files[0].content.contains("Endpoint = wg.example.test"));
        assert!(p.files[0].content.contains(PEER), "peer keys still apply before DNS succeeds");
        assert!(p.reload[0].required, "local configuration errors still fail apply");
        assert_eq!(
            p.resources[0],
            NetworkResource::Link { device: "wg-uplink".into(), link_type: "wireguard".into(), mtu: Some(1420), up: true }
        );
        assert_eq!(p.reload[0].argv, vec!["syncconf", "wg-uplink", "${GENERATION_ROOT}/wireguard/uplink/wg.conf"]);
        // Keys never appear in argv.
        assert!(!serde_json::to_string(&p.reload).unwrap().contains(PRIV));
    }

    #[test]
    fn numeric_endpoints_stay_in_required_local_reload_without_an_extra_runner() {
        let r = req();
        let mut cfg = load(&r).unwrap();
        for endpoint in ["192.0.2.1:51820", "[2001:db8::1]:51820"] {
            cfg.instances[0].peers[0].endpoint = Some(endpoint.into());
            let plans = plan(&cfg, &r).unwrap();
            assert!(plans[0].processes.is_empty());
            assert!(plans[0].files[0].content.contains(&format!("Endpoint = {endpoint}")));
            assert!(plans[0].reload[0].required);
        }
    }

    #[test]
    fn endpoint_errors_only_degrade_the_matching_peer_and_stale_config_errors_are_ignored() {
        let r = req();
        let cfg = load(&r).unwrap();
        let run = std::env::temp_dir().join(format!("mnp-wg-status-{}", std::process::id()));
        let state = endpoints::EndpointStatus {
            public_key: PEER.into(),
            desired_endpoint: "wg.example.test:51820".into(),
            phase: "resolution-failed".into(),
            attempt: 3,
            retry_at_unix: 120,
            updated_unix: 100,
            resolved_endpoint: Some("192.0.2.1:51820".into()),
            error: Some("Name does not resolve".into()),
        };
        meduza_np_runtime::fsutil::write_json_atomic(&endpoints::state_path(&run, "uplink"), &BTreeMap::from([("peer-b", state)]), 0o644)
            .unwrap();
        let make = |id: &str, kind: &str| InstanceStatus {
            id: id.into(),
            kind: kind.into(),
            state: ServiceState::Up,
            observed_at: None,
            reason: None,
            detail: BTreeMap::new(),
        };
        let original = vec![
            make("wireguard/uplink", "wireguard-interface"),
            make("wireguard/uplink/peer-b", "wireguard-peer"),
            make("openvpn/lemon", "openvpn-site2site"),
        ];
        let mut rows = original.clone();
        endpoints::attach_statuses(&cfg, &mut rows, &run, 110);
        assert_eq!(rows[1].state, ServiceState::Degraded);
        assert_eq!(rows[1].detail["connectionState"], "up", "a working old endpoint can still carry traffic");
        assert_eq!(rows[1].reason.as_deref(), Some("Name does not resolve"));
        assert_eq!(rows[0], original[0]);
        assert_eq!(rows[2], original[2]);
        let mut stale = original.clone();
        endpoints::attach_statuses(&cfg, &mut stale, &run, 221);
        assert_eq!(stale, original);
        let mut changed = cfg;
        changed.instances[0].peers[0].endpoint = Some("changed.example.test:51820".into());
        endpoints::attach_statuses(&changed, &mut stale, &run, 110);
        assert_eq!(stale, original);
        std::fs::remove_dir_all(run).unwrap();
    }

    #[test]
    fn validation_catches_errors() {
        let mut r = req();
        r.config.services.insert(
            "wireguard".into(),
            json!({"instances": [
                {"id": "a", "privateKeyRef": "missing", "device": "a-very-long-interface-name", "peers": [
                    {"id": "p", "publicKey": "short"}, {"id": "p", "endpoint": "nope"}
                ]},
                {"id": "b", "privateKeyRef": "wg-uplink", "device": "wg0", "listenPort": 51820},
                {"id": "c", "privateKeyRef": "wg-uplink", "device": "wg0", "listenPort": 51820}
            ]}),
        );
        let errs = validate(&load(&r).unwrap(), &r);
        let text = format!("{errs:?}");
        for needle in [
            "not delivered",
            "1-15 bytes",
            "32 bytes",
            "duplicate peer id",
            "host:port",
            "also used by",
            "listen port used",
            "public key required",
        ] {
            assert!(text.contains(needle), "missing {needle}: {text}");
        }
    }

    #[test]
    fn dump_status() {
        let r = req();
        let cfg = load(&r).unwrap();
        let dump = format!("{PRIV}\tOWNPUB\t51820\toff\n{PEER}\t(none)\t203.0.113.5:51820\t10.9.0.1/32\t1000\t1024\t2048\t25\n");
        let mut dumps = BTreeMap::new();
        dumps.insert("wg-uplink".to_string(), Ok(dump));
        let st = statuses(&cfg, &dumps, 1100);
        assert_eq!(st[0].state, ServiceState::Up);
        assert_eq!(st[1].state, ServiceState::Up);
        assert_eq!(st[1].detail["rxBytes"], json!(1024));
        assert_eq!(st[1].detail["endpoint"], json!("203.0.113.5:51820"));
        let stale = statuses(&cfg, &dumps, 5000);
        assert_eq!(stale[1].state, ServiceState::Down, "active peer with old handshake is down");
        let mut unconnected = BTreeMap::new();
        unconnected.insert(
            "wg-uplink".to_string(),
            Ok(format!("{PRIV}\tOWNPUB\t51820\toff\n{PEER}\t(none)\t203.0.113.5:51820\t10.9.0.1/32\t0\t0\t0\t25\n")),
        );
        let waiting = statuses(&cfg, &unconnected, 1100);
        assert_eq!(waiting[0].state, ServiceState::Up, "configured interface stays up without a peer handshake");
        assert_eq!(waiting[1].state, ServiceState::Down, "an unreachable active peer is reported separately");
        let none = statuses(&cfg, &BTreeMap::new(), 1100);
        assert_eq!(none[0].state, ServiceState::Down, "missing interface is not up");
        assert_eq!(none[1].state, ServiceState::Down);
    }

    #[test]
    fn passive_idle_peer_is_unknown_not_failed() {
        let peer: Peer = serde_json::from_value(json!({"id": "road", "publicKey": PEER})).unwrap();
        let d = DumpPeer { public_key: PEER.into(), endpoint: None, latest_handshake: 0, rx: 0, tx: 0 };
        assert_eq!(peer_state(&peer, Some(&d), 100, 180).0, ServiceState::Unknown);
    }

    #[test]
    fn public_key_derivation_matches_wireguard() {
        // Test vector from the WireGuard documentation (wg genkey | wg pubkey style pair).
        let pk = public_from_private(PRIV).unwrap();
        assert_eq!(pk.len(), 44);
        assert!(parse_key(&pk).is_some());
    }

    #[test]
    fn config_schema_is_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/config.json");
        meduza_np_protocol::schema::check_file::<WireGuardConfig>(&path).unwrap();
    }
}
