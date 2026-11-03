//! OpenVPN plugin (AGENT.md §9.2, §9.3, §10.1): independent Site2Site and Access instances.
//!
//! - Every instance gets its own directory with `config.ovpn` and 0600 key files from secret references;
//!   no inline secrets in argv, no user-supplied shell hooks.
//! - Clients use `route-nopull` by default: a VPN never silently takes over the default route.
//! - Access servers authenticate users through `openvpn-auth` (via-file) → planner `access.sock` → manager → LDAP.
//! - Status: management socket `state` (client/p2p) and the v3 status file (server client list) — process
//!   alive, device present and session established are reported separately.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr};

use ipnet::{IpNet, Ipv4Net};
use meduza_np_plugin_sdk::text::quote;
use meduza_np_plugin_sdk::{PluginError, PluginRequest};
use meduza_np_protocol::bgp::BgpSession;
use meduza_np_protocol::config::Networks;
use meduza_np_protocol::ids::{derive_ifname, validate_ifname, StableId};
use meduza_np_protocol::plan::{CommandSpec, GenerationPlan, PlannedFile, ProbeSpec, ProcessSpec, ProcessStartupPolicy, RestartPolicy};
use meduza_np_protocol::report::{InstanceStatus, ServiceState};
use meduza_np_protocol::{ValidationError, Validator, PLUGIN_PROTOCOL_VERSION};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const SLOT: &str = "openvpn.binary";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(title = "OpenVPN")]
pub struct OpenVpnConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub instances: Vec<Instance>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    #[default]
    Site2site,
    Access,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    Client,
    Server,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DevType {
    #[default]
    Tun,
    Tap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Proto {
    #[default]
    Udp,
    Tcp,
    Udp6,
    Tcp6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Topology {
    /// Point-to-point with explicit `ifconfig` addresses (one peer).
    #[default]
    P2p,
    /// Server mode with an address pool (`server` directive); clients pull their address.
    Subnet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ClientCertPolicy {
    /// Username/password only (`verify-client-cert none`).
    #[default]
    None,
    Optional,
    Require,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Compression {
    /// No compression (default; avoids VORACLE-class issues). Only `migrate` keeps old peers connecting.
    Off,
    Migrate,
    /// `comp-lzo no`: no compression, but the framing byte peers configured with `comp-lzo no` expect.
    LzoNo,
    /// `comp-lzo yes` + `allow-compression asym`: compressed packets from the peer are accepted, nothing is sent
    /// compressed (peers still configured with `comp-lzo yes`).
    LzoAsym,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Remote {
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proto: Option<Proto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Ifconfig {
    /// Local tunnel address (tun p2p) or address/prefix for tap (e.g. `10.8.0.1/30`).
    pub local: String,
    /// Remote tunnel address (tun p2p).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<Ipv4Addr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct Keepalive {
    pub interval: u32,
    pub timeout: u32,
}

impl Default for Keepalive {
    fn default() -> Self {
        Self { interval: 10, timeout: 60 }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Tls {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crl_ref: Option<String>,
    /// Classic DH parameters; omitted = ECDH only (`dh none`) on servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dh_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_crypt_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_auth_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_direction: Option<u8>,
    /// Require the peer certificate's extended key usage (`remote-cert-tls`). Defaults to the opposite role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_cert_tls: Option<String>,
    /// `verify-x509-name <name> name` for clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_x509_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_version_min: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PushRoutes {
    #[serde(default)]
    pub lan: bool,
    #[serde(default)]
    pub private_lan: bool,
    /// Advertised (external) prefixes of 1:1 mappings.
    #[serde(default)]
    pub mappings: bool,
    #[serde(default)]
    pub extra: Vec<IpNet>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccessSettings {
    /// Client IPv4 pool, e.g. `10.88.0.0/24`.
    pub pool: Ipv4Net,
    #[serde(default)]
    pub client_cert: ClientCertPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_clients: Option<u32>,
    #[serde(default)]
    pub push_routes: PushRoutes,
    #[serde(default)]
    pub push_dns: Vec<IpAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_domain: Option<String>,
    /// Data channel renegotiation; OpenVPN re-runs the auth hook unless an auth token is used.
    #[serde(default = "default_reneg")]
    pub reneg_seconds: u32,
    /// Issue an `auth-gen-token` so renegotiation does not replay the password (OpenVPN ≥ 2.4).
    #[serde(default = "default_true")]
    pub auth_token: bool,
    #[serde(default)]
    pub client_to_client: bool,
}

fn default_reneg() -> u32 {
    3600
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    pub id: StableId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub kind: Kind,
    /// Site2Site only; Access instances are always servers.
    #[serde(default)]
    pub role: Role,
    #[serde(default)]
    pub topology: Topology,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(default)]
    pub dev_type: DevType,
    #[serde(default)]
    pub proto: Proto,
    /// Bind address (servers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<IpAddr>,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Client remotes, tried in order.
    #[serde(default)]
    pub remotes: Vec<Remote>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ifconfig: Option<Ifconfig>,
    /// Server-mode Site2Site address pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_network: Option<Ipv4Net>,
    #[serde(default)]
    pub keepalive: Keepalive,
    #[serde(default = "default_true")]
    pub persist_key: bool,
    #[serde(default = "default_true")]
    pub persist_tun: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 576, max = 9000))]
    pub tun_mtu: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mssfix: Option<u32>,
    #[serde(default = "default_verb")]
    #[schemars(range(min = 0, max = 6))]
    pub verb: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<Compression>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cipher: Option<String>,
    #[serde(default)]
    pub data_ciphers: Vec<String>,
    #[serde(default)]
    pub tls: Tls,
    /// Pre-shared static key mode (p2p without TLS). Mutually exclusive with certificates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_key_ref: Option<String>,
    /// Client: accept server-pushed options (still never routes unless `acceptRoutes`).
    #[serde(default = "default_true")]
    pub pull: bool,
    /// p2p client of a multi-client (`mode server`) peer: also request its pushed options (`pull`, as OpenVPN's
    /// `client` does). Off for p2p peers, which push nothing.
    #[serde(default)]
    pub p2p_pull: bool,
    /// Client: accept pushed routes. Default false → `route-nopull` (§9.2).
    #[serde(default)]
    pub accept_routes: bool,
    /// Prefixes reachable through this tunnel, installed by planner routing (FRR) — informational here.
    #[serde(default)]
    pub routes: Vec<IpNet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<AccessSettings>,
    /// Site2Site: eBGP with the external side over this tunnel; compiled into FRR by the manager, never used by
    /// this plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bgp: Option<BgpSession>,
    /// Public `host[:port]` external clients/sites use to reach this server (exports, Access profiles).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_endpoint: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_port() -> u16 {
    1194
}

fn default_verb() -> u8 {
    3
}

const AUTH_DIGESTS: &[&str] = &["SHA256", "SHA384", "SHA512", "SHA1"];
const CIPHERS: &[&str] = &["AES-256-GCM", "AES-128-GCM", "CHACHA20-POLY1305", "AES-256-CBC", "AES-128-CBC"];

impl Instance {
    pub fn device(&self) -> String {
        let prefix = if self.dev_type == DevType::Tap { "tap-" } else { "tun-" };
        self.device.clone().unwrap_or_else(|| derive_ifname(prefix, self.id.as_str()))
    }
    pub fn is_server(&self) -> bool {
        self.kind == Kind::Access || self.role == Role::Server
    }
}

pub fn load(req: &PluginRequest) -> Result<OpenVpnConfig, PluginError> {
    Ok(req.service::<OpenVpnConfig>("openvpn")?.unwrap_or_default())
}

fn secret_pem_ok(req: &PluginRequest, id: &str, marker: &str) -> Option<bool> {
    req.config.secrets.get(id).map(|s| s.value.contains(marker))
}

fn require_secret(v: &mut Validator, req: &PluginRequest, path: String, id: &Option<String>, marker: &str, what: &str) {
    if let Some(id) = id {
        match secret_pem_ok(req, id, marker) {
            None => v.error(path, format!("secret {id} was not delivered to this node")),
            Some(false) => v.error(path, format!("secret {id} is not a {what}")),
            Some(true) => {}
        }
    }
}

pub fn validate(cfg: &OpenVpnConfig, req: &PluginRequest) -> Vec<ValidationError> {
    let mut v = Validator::new();
    let mut ids = BTreeSet::new();
    let mut devices = BTreeSet::new();
    let mut listeners = BTreeSet::new();
    let pools: Vec<(String, Ipv4Net)> =
        cfg.instances.iter().filter(|i| i.enabled).filter_map(|i| i.access.as_ref().map(|a| (i.id.to_string(), a.pool))).collect();
    for (n, inst) in cfg.instances.iter().enumerate() {
        let b = format!("/instances/{n}");
        v.check(ids.insert(inst.id.clone()), format!("{b}/id"), "duplicate id");
        let dev = inst.device();
        v.check(validate_ifname(&dev).is_ok(), format!("{b}/device"), "interface name must be 1-15 bytes");
        v.check(devices.insert(dev), format!("{b}/device"), "device used by another instance");
        if !inst.enabled {
            continue;
        }
        if inst.is_server() {
            let proto = matches!(inst.proto, Proto::Tcp | Proto::Tcp6);
            v.check(listeners.insert((proto, inst.local, inst.port)), format!("{b}/port"), "another server instance listens on this port");
        } else {
            v.check(!inst.remotes.is_empty(), format!("{b}/remotes"), "clients need at least one remote");
        }
        if let Some(bgp) = &inst.bgp {
            v.check(inst.kind == Kind::Site2site, format!("{b}/bgp"), "BGP runs over Site2Site instances only");
            bgp.validate(&mut v, &format!("{b}/bgp"));
        }
        for (j, r) in inst.remotes.iter().enumerate() {
            let ok = r.host.parse::<IpAddr>().is_ok() || meduza_np_protocol::config::valid_hostname(&r.host);
            v.check(ok, format!("{b}/remotes/{j}/host"), "invalid host");
        }
        if let Some(a) = &inst.auth {
            v.check(AUTH_DIGESTS.contains(&a.as_str()), format!("{b}/auth"), format!("auth must be one of {AUTH_DIGESTS:?}"));
        }
        for (j, c) in inst.cipher.iter().chain(inst.data_ciphers.iter()).enumerate() {
            v.check(CIPHERS.contains(&c.as_str()), format!("{b}/dataCiphers/{j}"), format!("cipher must be one of {CIPHERS:?}"));
        }
        if let Some(k) = inst.tls.key_direction {
            v.check(k <= 1, format!("{b}/tls/keyDirection"), "key-direction is 0 or 1");
        }
        v.check(
            inst.tls.tls_crypt_ref.is_none() || inst.tls.tls_auth_ref.is_none(),
            format!("{b}/tls"),
            "use tls-crypt or tls-auth, not both",
        );
        // Authentication material.
        match (&inst.static_key_ref, inst.kind) {
            (Some(_), Kind::Access) => v.error(format!("{b}/staticKeyRef"), "Access servers need TLS, not a static key"),
            (Some(_), _) if inst.topology == Topology::Subnet => v.error(format!("{b}/staticKeyRef"), "static key mode only supports p2p"),
            (Some(id), _) => {
                require_secret(&mut v, req, format!("{b}/staticKeyRef"), &Some(id.clone()), "OpenVPN Static key", "OpenVPN static key")
            }
            (None, _) => {
                v.check(inst.tls.ca_ref.is_some(), format!("{b}/tls/caRef"), "CA certificate required");
                let needs_cert = inst.is_server() || inst.kind == Kind::Site2site;
                if needs_cert {
                    v.check(inst.tls.cert_ref.is_some(), format!("{b}/tls/certRef"), "certificate required");
                    v.check(inst.tls.key_ref.is_some(), format!("{b}/tls/keyRef"), "private key required");
                }
                require_secret(&mut v, req, format!("{b}/tls/caRef"), &inst.tls.ca_ref, "BEGIN CERTIFICATE", "PEM certificate");
                require_secret(&mut v, req, format!("{b}/tls/certRef"), &inst.tls.cert_ref, "BEGIN CERTIFICATE", "PEM certificate");
                require_secret(&mut v, req, format!("{b}/tls/keyRef"), &inst.tls.key_ref, "PRIVATE KEY", "PEM private key");
                require_secret(&mut v, req, format!("{b}/tls/crlRef"), &inst.tls.crl_ref, "BEGIN X509 CRL", "PEM CRL");
                require_secret(&mut v, req, format!("{b}/tls/dhRef"), &inst.tls.dh_ref, "BEGIN DH PARAMETERS", "DH parameters file");
                require_secret(
                    &mut v,
                    req,
                    format!("{b}/tls/tlsCryptRef"),
                    &inst.tls.tls_crypt_ref,
                    "OpenVPN Static key",
                    "OpenVPN static key",
                );
                require_secret(
                    &mut v,
                    req,
                    format!("{b}/tls/tlsAuthRef"),
                    &inst.tls.tls_auth_ref,
                    "OpenVPN Static key",
                    "OpenVPN static key",
                );
            }
        }
        match (inst.kind, inst.topology, inst.role) {
            (Kind::Access, _, _) => {
                let Some(a) = &inst.access else {
                    v.error(format!("{b}/access"), "Access instances need access settings (pool)");
                    continue;
                };
                v.check(a.pool.prefix_len() <= 29, format!("{b}/access/pool"), "pool must be /29 or larger");
                for (other, p) in &pools {
                    if other != inst.id.as_str() && (p.contains(&a.pool.network()) || a.pool.contains(&p.network())) {
                        v.error(format!("{b}/access/pool"), format!("pool overlaps Access instance {other}"));
                    }
                }
                v.check(a.reneg_seconds == 0 || a.reneg_seconds >= 300, format!("{b}/access/renegSeconds"), "0 (disabled) or at least 300");
            }
            (_, Topology::P2p, _) => match &inst.ifconfig {
                None => v.error(format!("{b}/ifconfig"), "p2p tunnels need ifconfig addresses"),
                Some(ic) if inst.dev_type == DevType::Tun => {
                    v.check(ic.local.parse::<Ipv4Addr>().is_ok(), format!("{b}/ifconfig/local"), "tun p2p local must be an IPv4 address");
                    v.check(ic.remote.is_some(), format!("{b}/ifconfig/remote"), "tun p2p needs the remote address");
                }
                Some(ic) => v.check(ic.local.parse::<Ipv4Net>().is_ok(), format!("{b}/ifconfig/local"), "tap needs address/prefix"),
            },
            (_, Topology::Subnet, Role::Server) => {
                v.check(inst.server_network.is_some(), format!("{b}/serverNetwork"), "server mode needs serverNetwork")
            }
            (_, Topology::Subnet, Role::Client) => {}
        }
    }
    v.finish().err().unwrap_or_default()
}

fn mask(prefix: u8) -> Ipv4Addr {
    let bits = if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix)) };
    Ipv4Addr::from(bits)
}

/// Routes pushed to Access clients (§9.3): approved LAN/private LAN, mapping advertised prefixes and extras;
/// normalized, deduplicated, own pool excluded.
pub fn access_push_routes(a: &AccessSettings, nets: &Networks) -> Vec<IpNet> {
    let mut out: BTreeSet<IpNet> = BTreeSet::new();
    if a.push_routes.lan {
        out.extend(nets.lan.iter().map(|n| n.trunc()));
    }
    if a.push_routes.private_lan {
        out.extend(nets.private_lan.iter().map(|n| n.trunc()));
    }
    if a.push_routes.mappings {
        out.extend(nets.mappings.iter().filter(|m| m.enabled).map(|m| m.advertised.trunc()));
    }
    out.extend(a.push_routes.extra.iter().map(|n| n.trunc()));
    let pool = IpNet::V4(a.pool.trunc());
    out.into_iter().filter(|n| *n != pool && !pool.contains(n)).collect::<Vec<_>>().pipe(aggregate)
}

trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}
impl<T> Pipe for T {}

/// Drops prefixes covered by a broader one in the same list.
fn aggregate(mut v: Vec<IpNet>) -> Vec<IpNet> {
    v.sort_by_key(|n| n.prefix_len());
    let mut kept: Vec<IpNet> = Vec::new();
    for n in v {
        if !kept.iter().any(|k| k.contains(&n)) {
            kept.push(n);
        }
    }
    kept.sort();
    kept
}

fn proto_word(p: Proto, server: bool) -> &'static str {
    match (p, server) {
        (Proto::Udp, _) => "udp",
        (Proto::Udp6, _) => "udp6",
        (Proto::Tcp, true) => "tcp-server",
        (Proto::Tcp, false) => "tcp-client",
        (Proto::Tcp6, true) => "tcp6-server",
        (Proto::Tcp6, false) => "tcp6-client",
    }
}

pub struct Rendered {
    pub config: String,
    pub files: Vec<PlannedFile>,
}

pub fn render(inst: &Instance, req: &PluginRequest) -> Result<Rendered, PluginError> {
    let dir = format!("openvpn/{}", inst.id);
    let abs = req.gen_path(&dir);
    let run = req.context.run_dir.trim_end_matches('/');
    let mut files = Vec::new();
    let mut c: Vec<String> = Vec::new();
    let mut add = |line: String| c.push(line);
    let secret_file = |name: &str, id: &str, files: &mut Vec<PlannedFile>| -> Result<String, PluginError> {
        let value = &req.secret(id)?.value;
        let sensitive = value.contains("PRIVATE KEY") || value.contains("Static key");
        files.push(PlannedFile {
            path: format!("{dir}/{name}"),
            mode: if sensitive { "0600".into() } else { "0644".into() },
            content: format!("{}\n", value.trim()),
            sensitive,
        });
        Ok(format!("{abs}/{name}"))
    };

    let server = inst.is_server();
    add(format!("dev {}", inst.device()));
    add(format!("dev-type {}", if inst.dev_type == DevType::Tap { "tap" } else { "tun" }));
    add(format!("proto {}", proto_word(inst.proto, server)));
    if server {
        if let Some(l) = inst.local {
            add(format!("local {l}"));
        }
        add(format!("port {}", inst.port));
    } else {
        add("nobind".into());
        // A failed DNS lookup must not exhaust startup or require publishing the configuration again.
        add("resolv-retry infinite".into());
        add("connect-retry 1 60".into());
        for r in &inst.remotes {
            let port = r.port.unwrap_or(inst.port);
            match r.proto {
                Some(p) => add(format!("remote {} {port} {}", r.host, proto_word(p, false))),
                None => add(format!("remote {} {port}", r.host)),
            }
        }
    }

    match (inst.kind, inst.topology, inst.role) {
        (Kind::Access, _, _) => {
            let a = inst.access.as_ref().ok_or_else(|| PluginError::new("invalid-config", "access settings missing"))?;
            add("mode server".into());
            add("tls-server".into());
            add("topology subnet".into());
            add(format!("server {} {}", a.pool.network(), mask(a.pool.prefix_len())));
            if let Some(m) = a.max_clients {
                add(format!("max-clients {m}"));
            }
            if a.client_to_client {
                add("client-to-client".into());
            }
            add(format!(
                "verify-client-cert {}",
                match a.client_cert {
                    ClientCertPolicy::None => "none",
                    ClientCertPolicy::Optional => "optional",
                    ClientCertPolicy::Require => "require",
                }
            ));
            if a.client_cert == ClientCertPolicy::None {
                add("username-as-common-name".into());
            }
            add("script-security 2".into());
            add(format!("setenv MEDUZA_NP_ACCESS_SOCKET {}", quote(&format!("{run}/access.sock"))));
            let helper = format!("{}/openvpn-auth", req.context.libexec_dir.trim_end_matches('/'));
            add(format!("auth-user-pass-verify {} via-file", quote(&format!("{helper} --instance {}", inst.id))));
            // The session lifetime the manager granted becomes this client's `session-timeout` (D38).
            add(format!("client-connect {}", quote(&format!("{helper} --instance {} --connect", inst.id))));
            add(format!("reneg-sec {}", a.reneg_seconds));
            if a.auth_token {
                // Renegotiation presents the token instead of the password; the token lives in memory only.
                // `external-auth` still calls the hook then (session_state=Authenticated, no password): the manager
                // re-checks the account, so a user removed from the directory or the policy loses the session at
                // the next renegotiation (D38). The lifetime is only an upper bound; `session-timeout` ends sessions.
                let lifetime = a.reneg_seconds.max(3600) * 8;
                if a.reneg_seconds > 0 {
                    add(format!("auth-gen-token {lifetime} {} external-auth", a.reneg_seconds));
                } else {
                    add(format!("auth-gen-token {lifetime} external-auth"));
                }
            }
            let routes = access_push_routes(a, &req.config.networks);
            for r in &routes {
                match r {
                    IpNet::V4(n) => add(format!("push {}", quote(&format!("route {} {}", n.network(), mask(n.prefix_len()))))),
                    IpNet::V6(n) => add(format!("push {}", quote(&format!("route-ipv6 {n}")))),
                }
            }
            for d in &a.push_dns {
                add(format!("push {}", quote(&format!("dhcp-option DNS {d}"))));
            }
            if let Some(dom) = &a.push_domain {
                add(format!("push {}", quote(&format!("dhcp-option DOMAIN {dom}"))));
            }
            add(format!("status {} 10", quote(&format!("{run}/openvpn-{}.status", inst.id))));
            add("status-version 3".into());
        }
        (_, Topology::P2p, role) => {
            if inst.static_key_ref.is_none() {
                add(if role == Role::Server { "tls-server".into() } else { "tls-client".into() });
                if role == Role::Client && inst.p2p_pull {
                    add("pull".into());
                }
            }
            let ic = inst.ifconfig.as_ref().ok_or_else(|| PluginError::new("invalid-config", "ifconfig missing"))?;
            match (inst.dev_type, ic.remote) {
                (DevType::Tun, Some(r)) => add(format!("ifconfig {} {r}", ic.local)),
                (_, _) => {
                    let net: Ipv4Net =
                        ic.local.parse().map_err(|_| PluginError::new("invalid-config", "tap ifconfig needs address/prefix"))?;
                    add(format!("ifconfig {} {}", net.addr(), mask(net.prefix_len())));
                }
            }
        }
        (_, Topology::Subnet, Role::Server) => {
            let n = inst.server_network.ok_or_else(|| PluginError::new("invalid-config", "serverNetwork missing"))?;
            add("mode server".into());
            add("tls-server".into());
            add("topology subnet".into());
            add(format!("server {} {}", n.network(), mask(n.prefix_len())));
            add(format!("status {} 10", quote(&format!("{run}/openvpn-{}.status", inst.id))));
            add("status-version 3".into());
        }
        (_, Topology::Subnet, Role::Client) => {
            if inst.pull {
                add("client".into());
            } else {
                add("tls-client".into());
            }
        }
    }
    if !server && !inst.accept_routes {
        add("route-nopull".into());
    }
    if !server && !inst.pull && inst.topology == Topology::Subnet {
        add("pull-filter ignore \"route\"".into());
    }
    add(format!("keepalive {} {}", inst.keepalive.interval, inst.keepalive.timeout));
    if inst.persist_key {
        add("persist-key".into());
    }
    if inst.persist_tun {
        add("persist-tun".into());
    }
    if let Some(m) = inst.tun_mtu {
        add(format!("tun-mtu {m}"));
    }
    if let Some(m) = inst.mssfix {
        add(format!("mssfix {m}"));
    }
    match inst.compression {
        Some(Compression::Migrate) => add("compress migrate".into()),
        Some(Compression::LzoNo) => add("comp-lzo no".into()),
        Some(Compression::LzoAsym) => {
            add("comp-lzo yes".into());
            add("allow-compression asym".into());
        }
        _ => add("allow-compression no".into()),
    }
    add(format!("verb {}", inst.verb));
    if let Some(a) = &inst.auth {
        add(format!("auth {a}"));
    }
    if !inst.data_ciphers.is_empty() {
        add(format!("data-ciphers {}", inst.data_ciphers.join(":")));
    }
    if let Some(cph) = &inst.cipher {
        add(format!("{} {cph}", if inst.static_key_ref.is_some() { "cipher" } else { "data-ciphers-fallback" }));
    }
    // Management on a private Unix socket for status queries only.
    add(format!("management {} unix", quote(&format!("{run}/openvpn-{}.mgmt", inst.id))));

    if let Some(id) = &inst.static_key_ref {
        // The user explicitly selected legacy static-key mode. OpenVPN 2.7 needs the opt-in; older supported
        // providers ignore exactly this option, keeping the same configuration usable with 2.4/2.5/2.6.
        add("ignore-unknown-option allow-deprecated-insecure-static-crypto".into());
        add("allow-deprecated-insecure-static-crypto".into());
        let path = secret_file("static.key", id, &mut files)?;
        let dir_word = if inst.role == Role::Server { 0 } else { 1 };
        add(format!("secret {} {dir_word}", quote(&path)));
    } else {
        if let Some(id) = &inst.tls.ca_ref {
            add(format!("ca {}", quote(&secret_file("ca.pem", id, &mut files)?)));
        }
        if let Some(id) = &inst.tls.cert_ref {
            add(format!("cert {}", quote(&secret_file("cert.pem", id, &mut files)?)));
        }
        if let Some(id) = &inst.tls.key_ref {
            add(format!("key {}", quote(&secret_file("key.pem", id, &mut files)?)));
        }
        if let Some(id) = &inst.tls.crl_ref {
            add(format!("crl-verify {}", quote(&secret_file("crl.pem", id, &mut files)?)));
        }
        if server {
            match &inst.tls.dh_ref {
                Some(id) => add(format!("dh {}", quote(&secret_file("dh.pem", id, &mut files)?))),
                None => add("dh none".into()),
            }
        }
        if let Some(id) = &inst.tls.tls_crypt_ref {
            add(format!("tls-crypt {}", quote(&secret_file("tls-crypt.key", id, &mut files)?)));
        }
        if let Some(id) = &inst.tls.tls_auth_ref {
            let kd = inst.tls.key_direction.unwrap_or(if server { 0 } else { 1 });
            add(format!("tls-auth {} {kd}", quote(&secret_file("tls-auth.key", id, &mut files)?)));
        }
        let remote_cert = inst.tls.remote_cert_tls.clone().or_else(|| match (inst.kind, inst.role) {
            // Access clients may authenticate with username/password only; require EKU when certificates are mandatory.
            (Kind::Access, _) => inst.access.as_ref().filter(|a| a.client_cert == ClientCertPolicy::Require).map(|_| "client".into()),
            (_, Role::Client) => Some("server".into()),
            (_, Role::Server) => Some("client".into()),
        });
        if let Some(rc) = remote_cert {
            add(format!("remote-cert-tls {rc}"));
        }
        if let Some(name) = &inst.tls.verify_x509_name {
            add(format!("verify-x509-name {} name", quote(name)));
        }
        add(format!("tls-version-min {}", inst.tls.tls_version_min.as_deref().unwrap_or("1.2")));
    }
    let mut config = c.join("\n");
    config.push('\n');
    for line in config.lines() {
        if line.contains('\r') {
            return Err(PluginError::new("invalid-config", "control characters in configuration"));
        }
    }
    files.insert(0, PlannedFile { path: format!("{dir}/config.ovpn"), mode: "0600".into(), content: config.clone(), sensitive: false });
    Ok(Rendered { config, files })
}

pub fn plan(cfg: &OpenVpnConfig, req: &PluginRequest) -> Result<Vec<GenerationPlan>, PluginError> {
    if !cfg.enabled {
        return Ok(Vec::new());
    }
    let mut plans = Vec::new();
    for inst in cfg.instances.iter().filter(|i| i.enabled) {
        let r = render(inst, req)?;
        let connection = inst.kind == Kind::Site2site;
        let config_path = format!("${{GENERATION_ROOT}}/openvpn/{}/config.ovpn", inst.id);
        plans.push(GenerationPlan {
            protocol_version: PLUGIN_PROTOCOL_VERSION,
            request_id: req.request_id.clone(),
            instance: format!("openvpn/{}", inst.id),
            files: r.files,
            processes: vec![ProcessSpec {
                id: format!("openvpn/{}", inst.id),
                provider_slot: Some(SLOT.into()),
                program: None,
                argv: vec!["--config".into(), config_path.clone()],
                env: BTreeMap::new(),
                restart: if connection { RestartPolicy::Always } else { RestartPolicy::OnFailure },
                startup: if connection { ProcessStartupPolicy::Connection } else { ProcessStartupPolicy::Required },
                stop_timeout_seconds: 10,
                reload_signal: None,
                restart_digest: None,
                user: None,
                bound_to: None,
            }],
            resources: Vec::new(),
            depends_on: Vec::new(),
            probes: vec![ProbeSpec::Plugin { interval_seconds: 30, params: Value::Null }],
            // Static-key crypto self-tests need neither a peer nor a TUN and check local crypto options and
            // key-file accessibility. They do not fully validate the supplied key bytes on every provider.
            // TLS has no general dry-run mode; parse its staged options without opening the network/TUN.
            checks: vec![CommandSpec {
                provider_slot: Some(SLOT.into()),
                program: None,
                argv: if inst.static_key_ref.is_some() {
                    // Config content uses the stable current link, which still names the previous generation
                    // during preflight. Override only the key path for this check to read the staged key.
                    vec![
                        "--config".into(),
                        config_path,
                        "--test-crypto".into(),
                        "--verb".into(),
                        "0".into(),
                        "--secret".into(),
                        format!("${{GENERATION_ROOT}}/openvpn/{}/static.key", inst.id),
                        if inst.role == Role::Server { "0".into() } else { "1".into() },
                    ]
                } else {
                    vec!["--config".into(), config_path, "--show-ciphers".into()]
                },
                timeout_seconds: 10,
                required: true,
            }],
            reload: Vec::new(),
        });
    }
    Ok(plans)
}

/// Parses a management `state` reply line: `1700000000,CONNECTED,SUCCESS,10.8.0.2,198.51.100.1,1194,,`.
pub fn parse_state(text: &str) -> Option<(u64, String, Option<String>, Option<String>)> {
    let line =
        text.lines().find(|l| l.split(',').nth(1).map(|s| s.chars().all(|c| c.is_ascii_uppercase() || c == '_')).unwrap_or(false))?;
    let f: Vec<&str> = line.split(',').collect();
    let at = f.first()?.trim_start_matches('>').trim_start_matches("STATE:").parse().ok()?;
    let local = f.get(3).filter(|s| !s.is_empty()).map(|s| s.to_string());
    let remote = f.get(4).filter(|s| !s.is_empty()).map(|s| s.to_string());
    Some((at, f.get(1)?.to_string(), local, remote))
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClientEntry {
    pub common_name: String,
    pub real_address: String,
    pub virtual_address: String,
    pub rx: u64,
    pub tx: u64,
    pub connected_since: u64,
    pub username: String,
    /// Management client id (`client-kill`); absent in status files of old OpenVPN versions.
    pub client_id: Option<u64>,
}

/// Parses `status-version 3` (tab separated) client list.
pub fn parse_status_v3(text: &str) -> (Vec<ClientEntry>, Option<u64>) {
    let mut clients = Vec::new();
    let mut updated = None;
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f.first().copied() {
            Some("TIME") => updated = f.get(2).and_then(|t| t.parse().ok()),
            Some("CLIENT_LIST") if f.len() >= 9 => clients.push(ClientEntry {
                common_name: f[1].into(),
                real_address: f[2].into(),
                virtual_address: f[3].into(),
                rx: f[5].parse().unwrap_or(0),
                tx: f[6].parse().unwrap_or(0),
                connected_since: f.get(8).and_then(|t| t.parse().ok()).unwrap_or(0),
                username: f.get(9).map(|s| s.to_string()).unwrap_or_default(),
                client_id: f.get(10).and_then(|t| t.parse().ok()),
            }),
            _ => {}
        }
    }
    (clients, updated)
}

/// The first `SUCCESS:` / `ERROR:` line of a management command reply (notifications start with `>`).
pub fn command_reply(text: &str) -> Option<Result<String, String>> {
    text.lines().find_map(|l| {
        let l = l.trim_end();
        l.strip_prefix("SUCCESS:").map(|m| Ok(m.trim().to_string())).or_else(|| l.strip_prefix("ERROR:").map(|m| Err(m.trim().to_string())))
    })
}

/// `disconnect-client` (D39): ends every live session of `username` (or only `client_id`) on a multi-client server.
/// `mgmt(command)` sends one management command and returns its raw reply (`status 3` up to `END`). The client list
/// comes from the live `status 3`, not the status file, so a session that connected after the last file write is
/// found too. Clients get `HALT` (they stop instead of reconnecting with their token). Idempotent: nobody connected
/// is a success with an empty list.
pub fn disconnect_clients(
    mut mgmt: impl FnMut(&str) -> Result<String, String>,
    username: Option<&str>,
    client_id: Option<u64>,
) -> Result<Value, PluginError> {
    let status = mgmt("status 3").map_err(|e| PluginError::new("management-unavailable", e))?;
    if let Some(Err(e)) = command_reply(&status) {
        return Err(PluginError::new("management-error", e));
    }
    let (clients, _) = parse_status_v3(&status);
    let selected: Vec<&ClientEntry> = clients
        .iter()
        .filter(|c| {
            username.is_none_or(|u| c.username.eq_ignore_ascii_case(u) || (c.username.is_empty() && c.common_name.eq_ignore_ascii_case(u)))
        })
        .filter(|c| client_id.is_none_or(|id| c.client_id == Some(id)))
        .collect();
    let mut done = Vec::new();
    let mut failed = Vec::new();
    for c in selected {
        let Some(cid) = c.client_id else {
            failed.push(format!("{}: no client id (OpenVPN too old for client-kill)", c.common_name));
            continue;
        };
        match mgmt(&format!("client-kill {cid} HALT")).map(|r| command_reply(&r)) {
            Ok(Some(Ok(_))) => done.push(json!({
                "clientId": cid,
                "username": if c.username.is_empty() { &c.common_name } else { &c.username },
                "realAddress": c.real_address,
                "virtualAddress": c.virtual_address,
            })),
            Ok(Some(Err(e))) => failed.push(format!("client {cid}: {e}")),
            Ok(None) => failed.push(format!("client {cid}: no reply")),
            Err(e) => failed.push(format!("client {cid}: {e}")),
        }
    }
    if !failed.is_empty() && done.is_empty() {
        return Err(PluginError::new("kill-failed", failed.join("; ")));
    }
    Ok(json!({"disconnected": done, "failed": failed}))
}

pub struct Observed {
    pub process_running: bool,
    pub device_present: bool,
    pub state: Option<(u64, String, Option<String>, Option<String>)>,
    pub status_file: Option<String>,
}

/// Layered status (§9.2): process / device / session are distinct.
pub fn statuses(cfg: &OpenVpnConfig, observed: &BTreeMap<String, Observed>, now: u64) -> Vec<InstanceStatus> {
    let ts = Some(meduza_np_plugin_sdk::unix_to_rfc3339(now));
    let mut out = Vec::new();
    for inst in cfg.instances.iter().filter(|i| i.enabled && cfg.enabled) {
        let Some(o) = observed.get(inst.id.as_str()) else { continue };
        let mut detail = BTreeMap::new();
        detail.insert("processRunning".into(), Value::Bool(o.process_running));
        detail.insert("devicePresent".into(), Value::Bool(o.device_present));
        // (no `kind` detail: details are flattened into the status and would replace `openvpn-site2site`)
        // Servers with a client list (`mode server`: Access, Site2Site subnet); a p2p server has one peer and a session
        // state like a client.
        let multi_client = inst.kind == Kind::Access || (inst.role == Role::Server && inst.topology == Topology::Subnet);
        let (state, reason) = if multi_client {
            let (clients, updated) = o.status_file.as_deref().map(parse_status_v3).unwrap_or_default();
            detail.insert("clients".into(), Value::from(clients.len()));
            if let Some(u) = updated {
                detail.insert("statusUpdated".into(), Value::String(meduza_np_plugin_sdk::unix_to_rfc3339(u)));
            }
            for c in &clients {
                let mut cd = BTreeMap::new();
                cd.insert("commonName".into(), Value::String(c.common_name.clone()));
                cd.insert("username".into(), Value::String(c.username.clone()));
                cd.insert("realAddress".into(), Value::String(c.real_address.clone()));
                cd.insert("virtualAddress".into(), Value::String(c.virtual_address.clone()));
                cd.insert("rxBytes".into(), Value::from(c.rx));
                cd.insert("txBytes".into(), Value::from(c.tx));
                cd.insert("connectedSince".into(), Value::String(meduza_np_plugin_sdk::unix_to_rfc3339(c.connected_since)));
                if let Some(cid) = c.client_id {
                    cd.insert("clientId".into(), Value::from(cid));
                }
                let cid = c
                    .common_name
                    .to_ascii_lowercase()
                    .chars()
                    .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
                    .collect::<String>();
                out.push(InstanceStatus {
                    id: format!("openvpn/{}/{}", inst.id, cid.trim_matches('-').chars().take(63).collect::<String>()),
                    kind: "openvpn-client".into(),
                    state: ServiceState::Up,
                    observed_at: ts.clone(),
                    reason: None,
                    detail: cd,
                });
            }
            match (o.process_running, o.device_present) {
                (false, _) => (ServiceState::Down, Some("process not running".to_string())),
                (true, false) => (ServiceState::Degraded, Some("tunnel device missing".to_string())),
                (true, true) => (ServiceState::Up, None),
            }
        } else {
            let session = o.state.as_ref().map(|s| s.1.clone());
            if let Some((at, st, local, remote)) = &o.state {
                detail.insert("session".into(), Value::String(st.clone()));
                detail.insert("since".into(), Value::String(meduza_np_plugin_sdk::unix_to_rfc3339(*at)));
                if let Some(l) = local {
                    detail.insert("localTunnelAddress".into(), Value::String(l.clone()));
                }
                if let Some(r) = remote {
                    detail.insert("remote".into(), Value::String(r.clone()));
                }
            }
            match (o.process_running, session.as_deref()) {
                (false, _) => (ServiceState::Down, Some("process not running".to_string())),
                (true, Some("CONNECTED")) => (ServiceState::Up, None),
                (true, Some(s)) => (ServiceState::Starting, Some(format!("session {s}"))),
                (true, None) => (ServiceState::Unknown, Some("management interface not reachable".to_string())),
            }
        };
        out.push(InstanceStatus {
            id: format!("openvpn/{}", inst.id),
            kind: format!("openvpn-{}", if inst.kind == Kind::Access { "access" } else { "site2site" }),
            state,
            observed_at: ts.clone(),
            reason,
            detail,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use meduza_np_plugin_sdk::test_request;
    use serde_json::json;

    fn secrets() -> Value {
        json!({
            "ca": {"kind": "pem-certificate", "value": "-----BEGIN CERTIFICATE-----\nCA\n-----END CERTIFICATE-----"},
            "crt": {"kind": "pem-certificate", "value": "-----BEGIN CERTIFICATE-----\nX\n-----END CERTIFICATE-----"},
            "key": {"kind": "pem-private-key", "value": "-----BEGIN PRIVATE KEY-----\nK\n-----END PRIVATE KEY-----"},
            "tc": {"kind": "openvpn-tls-crypt", "value": "-----BEGIN OpenVPN Static key V1-----\nab\n-----END OpenVPN Static key V1-----"}
        })
    }

    fn req(instances: Value) -> PluginRequest {
        test_request(
            "plan",
            json!({
                "networks": {"lan": ["10.42.10.0/24"], "privateLan": ["10.99.10.0/24"], "mappings": [{"id": "m", "advertised": "10.100.1.0/24", "actual": "192.168.1.0/24"}]},
                "services": {"openvpn": {"instances": instances}},
                "secrets": secrets()
            }),
        )
    }

    #[test]
    fn site2site_client_matches_spec_semantics() {
        let r = req(json!([{
            "id": "office", "kind": "site2site", "role": "client", "topology": "subnet", "device": "tun-office",
            "remotes": [{"host": "vpn.example.test", "port": 1194}],
            "tls": {"caRef": "ca", "certRef": "crt", "keyRef": "key", "tlsCryptRef": "tc"}
        }]));
        let cfg = load(&r).unwrap();
        assert!(validate(&cfg, &r).is_empty(), "{:?}", validate(&cfg, &r));
        let out = render(&cfg.instances[0], &r).unwrap();
        for line in [
            "dev tun-office",
            "dev-type tun",
            "proto udp",
            "remote vpn.example.test 1194",
            "resolv-retry infinite",
            "connect-retry 1 60",
            "client",
            "route-nopull",
            "persist-key",
            "persist-tun",
            "remote-cert-tls server",
            "allow-compression no",
        ] {
            assert!(out.config.lines().any(|l| l == line), "missing {line:?} in\n{}", out.config);
        }
        assert!(out.config.contains("ca \"/run/meduza-np/generations/rev-000001/openvpn/office/ca.pem\""));
        let key = out.files.iter().find(|f| f.path.ends_with("key.pem")).unwrap();
        assert_eq!(key.mode, "0600");
        assert!(key.sensitive);
        assert!(!out.config.contains("BEGIN PRIVATE KEY"), "secrets are files, never inline");
        let plans = plan(&cfg, &r).unwrap();
        assert!(plans[0].validate().is_empty(), "{:?}", plans[0].validate().errors());
        assert_eq!(plans[0].processes[0].startup, ProcessStartupPolicy::Connection);
        assert_eq!(plans[0].processes[0].restart, RestartPolicy::Always);
        assert_eq!(plans[0].checks[0].argv, vec!["--config", "${GENERATION_ROOT}/openvpn/office/config.ovpn", "--show-ciphers"]);
        assert!(plans[0].checks[0].required);
    }

    #[test]
    fn site2site_tls_listener_retries_like_an_external_client() {
        let r = req(json!([{
            "id":"partner", "kind":"site2site", "role":"server", "topology":"subnet",
            "serverNetwork":"10.88.0.0/24", "tls":{"caRef":"ca","certRef":"crt","keyRef":"key"}
        }]));
        let cfg = load(&r).unwrap();
        assert!(validate(&cfg, &r).is_empty(), "{:?}", validate(&cfg, &r));
        let plans = plan(&cfg, &r).unwrap();
        assert_eq!(plans[0].processes[0].startup, ProcessStartupPolicy::Connection);
        assert_eq!(plans[0].processes[0].restart, RestartPolicy::Always);
        assert!(plans[0].files[0].content.contains("tls-server\n"));
        assert!(!plans[0].files[0].content.contains("allow-deprecated-insecure-static-crypto"));
        assert_eq!(plans[0].checks[0].argv[2], "--show-ciphers");
        assert!(plans[0].checks[0].required);
    }

    #[test]
    fn access_server_hook_and_push_routes() {
        let r = req(json!([{
            "id": "access-main", "kind": "access", "port": 1195, "device": "tun-access",
            "tls": {"caRef": "ca", "certRef": "crt", "keyRef": "key"},
            "access": {"pool": "10.88.0.0/24", "pushRoutes": {"lan": true, "mappings": true, "extra": ["10.42.10.128/25", "10.88.0.0/25"]}, "pushDns": ["10.42.1.1"]}
        }]));
        let cfg = load(&r).unwrap();
        assert!(validate(&cfg, &r).is_empty(), "{:?}", validate(&cfg, &r));
        let out = render(&cfg.instances[0], &r).unwrap();
        for line in [
            "port 1195",
            "mode server",
            "topology subnet",
            "server 10.88.0.0 255.255.255.0",
            "script-security 2",
            "auth-user-pass-verify \"/usr/libexec/meduza-np/openvpn-auth --instance access-main\" via-file",
            "client-connect \"/usr/libexec/meduza-np/openvpn-auth --instance access-main --connect\"",
            "reneg-sec 3600",
            "auth-gen-token 28800 3600 external-auth",
            "push \"dhcp-option DNS 10.42.1.1\"",
            "push \"route 10.42.10.0 255.255.255.0\"",
            "push \"route 10.100.1.0 255.255.255.0\"",
            "verify-client-cert none",
            "dh none",
        ] {
            assert!(out.config.lines().any(|l| l == line), "missing {line:?} in\n{}", out.config);
        }
        // private LAN not selected; covered extra route aggregated away; own pool excluded.
        assert!(!out.config.contains("10.99.10.0"));
        assert!(!out.config.contains("10.42.10.128"));
        assert!(!out.config.contains("route 10.88.0.0"));
        let plans = plan(&cfg, &r).unwrap();
        assert_eq!(plans[0].processes[0].startup, ProcessStartupPolicy::Required);
        assert_eq!(plans[0].processes[0].restart, RestartPolicy::OnFailure);
    }

    #[test]
    fn legacy_compression_framing_and_p2p_pull() {
        let r = req(json!([
            {"id": "lzo", "kind": "site2site", "role": "server", "topology": "p2p", "staticKeyRef": "tc", "port": 1195,
             "ifconfig": {"local": "10.9.0.1", "remote": "10.9.0.2"}, "compression": "lzo-asym", "cipher":"AES-256-CBC"},
            {"id": "nolzo", "kind": "site2site", "role": "server", "topology": "p2p", "staticKeyRef": "tc", "port": 1196,
             "ifconfig": {"local": "10.9.1.1", "remote": "10.9.1.2"}, "compression": "lzo-no"},
            {"id": "cli", "kind": "site2site", "role": "client", "topology": "p2p", "p2pPull": true, "remotes": [{"host": "vpn.example.test"}],
             "ifconfig": {"local": "10.9.2.2", "remote": "10.9.2.1"}, "tls": {"caRef": "ca", "certRef": "crt", "keyRef": "key"}}
        ]));
        let cfg = load(&r).unwrap();
        let conf = |id: &str| render(cfg.instances.iter().find(|i| i.id.as_str() == id).unwrap(), &r).unwrap().config;
        let lzo = conf("lzo");
        assert!(
            lzo.contains("comp-lzo yes\n") && lzo.contains("allow-compression asym\n") && !lzo.contains("allow-compression no"),
            "{lzo}"
        );
        assert!(conf("nolzo").contains("comp-lzo no\n"));
        assert!(lzo.contains("cipher AES-256-CBC\n") && !lzo.contains("data-ciphers-fallback"));
        for option in ["ignore-unknown-option allow-deprecated-insecure-static-crypto", "allow-deprecated-insecure-static-crypto"] {
            assert!(lzo.lines().any(|line| line == option), "missing static-key compatibility option {option}");
        }
        let cli = conf("cli");
        assert!(cli.contains("tls-client\npull\n"), "{cli}");
        assert!(!cli.contains("allow-deprecated-insecure-static-crypto"), "TLS configurations keep TLS mandatory");
        for plan in plan(&cfg, &r).unwrap() {
            assert!(plan.validate().is_empty(), "{:?}", plan.validate().errors());
            assert_eq!(
                plan.processes[0].startup,
                ProcessStartupPolicy::Connection,
                "{} is an external connection in either role",
                plan.instance
            );
            assert_eq!(plan.processes[0].restart, RestartPolicy::Always);
            assert_eq!(plan.checks[0].argv[2], if plan.instance == "openvpn/cli" { "--show-ciphers" } else { "--test-crypto" });
            if plan.instance != "openvpn/cli" {
                assert_eq!(&plan.checks[0].argv[5..], ["--secret", &format!("${{GENERATION_ROOT}}/{}/static.key", plan.instance), "0"]);
            }
            assert!(plan.checks[0].required);
        }
    }

    #[test]
    fn validation_errors() {
        let r = req(json!([
            {"id": "a", "kind": "access", "tls": {"caRef": "ca", "certRef": "crt", "keyRef": "missing"}, "access": {"pool": "10.88.0.0/24"}},
            {"id": "b", "kind": "access", "port": 1194, "tls": {"caRef": "key", "certRef": "crt", "keyRef": "key"}, "access": {"pool": "10.88.0.0/25"}},
            {"id": "c", "kind": "site2site", "role": "client", "tls": {"caRef": "ca", "certRef": "crt", "keyRef": "key"}},
            {"id": "d", "kind": "site2site", "role": "server", "topology": "p2p", "staticKeyRef": "tc"}
        ]));
        let text = format!("{:?}", validate(&load(&r).unwrap(), &r));
        for needle in [
            "not delivered",
            "another server instance listens",
            "is not a PEM certificate",
            "overlaps Access instance",
            "at least one remote",
            "p2p tunnels need ifconfig",
        ] {
            assert!(text.contains(needle), "missing {needle}: {text}");
        }
    }

    #[test]
    fn p2p_server_reports_its_session() {
        let r = req(json!([
            {"id": "ext", "kind": "site2site", "role": "server", "topology": "p2p", "ifconfig": {"local": "10.99.0.1", "remote": "10.99.0.2"},
             "tls": {"caRef": "ca", "certRef": "crt", "keyRef": "key"}}
        ]));
        let cfg = load(&r).unwrap();
        let mut obs = BTreeMap::new();
        obs.insert(
            "ext".into(),
            Observed {
                process_running: true,
                device_present: true,
                state: parse_state("1700000000,CONNECTED,SUCCESS,10.99.0.1,203.0.113.30,1194,,\nEND\n"),
                status_file: None,
            },
        );
        let st = statuses(&cfg, &obs, 1700000200);
        assert_eq!(st.len(), 1);
        assert_eq!((st[0].kind.as_str(), st[0].state), ("openvpn-site2site", ServiceState::Up));
        assert_eq!(st[0].detail["remote"], json!("203.0.113.30"));
        assert!(!st[0].detail.contains_key("clients"), "one peer, no client list");
        obs.get_mut("ext").unwrap().state = parse_state("1700000000,WAIT,,,,,,\n");
        assert_eq!(statuses(&cfg, &obs, 1700000200)[0].state, ServiceState::Starting, "waiting for the peer");
    }

    #[test]
    fn status_layers() {
        let r = req(json!([
            {"id": "office", "kind": "site2site", "role": "client", "topology": "subnet", "remotes": [{"host": "vpn.example.test"}], "tls": {"caRef": "ca", "certRef": "crt", "keyRef": "key"}},
            {"id": "access-main", "kind": "access", "tls": {"caRef": "ca", "certRef": "crt", "keyRef": "key"}, "access": {"pool": "10.88.0.0/24"}}
        ]));
        let cfg = load(&r).unwrap();
        let mut obs = BTreeMap::new();
        obs.insert(
            "office".into(),
            Observed {
                process_running: true,
                device_present: true,
                state: parse_state("1700000000,CONNECTED,SUCCESS,10.8.0.6,198.51.100.1,1194,,\nEND\n"),
                status_file: None,
            },
        );
        obs.insert("access-main".into(), Observed {
            process_running: true,
            device_present: true,
            state: None,
            status_file: Some("TITLE\tOpenVPN\nTIME\tx\t1700000100\nCLIENT_LIST\talice\t203.0.113.7:51111\t10.88.0.2\t\t100\t200\tx\t1700000050\talice\t1\t0\tAES-256-GCM\n".into()),
        });
        let st = statuses(&cfg, &obs, 1700000200);
        let office = st.iter().find(|s| s.id == "openvpn/office").unwrap();
        assert_eq!(office.state, ServiceState::Up);
        assert_eq!(office.detail["remote"], json!("198.51.100.1"));
        // details are flattened: the serialized status keeps its own kind
        assert_eq!(serde_json::to_value(office).unwrap()["kind"], json!("openvpn-site2site"));
        let client = st.iter().find(|s| s.kind == "openvpn-client").unwrap();
        assert_eq!(client.detail["username"], json!("alice"));
        assert_eq!(client.detail["rxBytes"], json!(100));
        assert_eq!(client.detail["clientId"], json!(1));
        obs.get_mut("office").unwrap().state = parse_state("1700000000,RECONNECTING,ping-restart,,,,,\n");
        let st = statuses(&cfg, &obs, 1700000200);
        assert_eq!(st.iter().find(|s| s.id == "openvpn/office").unwrap().state, ServiceState::Starting, "process alive ≠ session up");
    }

    #[test]
    fn disconnect_kills_every_session_of_the_user() {
        let status = concat!(
            "TITLE\tOpenVPN\n",
            "TIME\tx\t1700000100\n",
            "CLIENT_LIST\talice\t203.0.113.7:51111\t10.88.0.2\t\t100\t200\tx\t1700000050\talice\t4\t0\tAES-256-GCM\n",
            "CLIENT_LIST\tbob\t203.0.113.8:51111\t10.88.0.3\t\t100\t200\tx\t1700000050\tbob\t5\t1\tAES-256-GCM\n",
            "CLIENT_LIST\tAlice\t198.51.100.9:4000\t10.88.0.4\t\t1\t2\tx\t1700000060\tAlice\t9\t2\tAES-256-GCM\n",
            "END\n",
        );
        let mut sent = Vec::new();
        let r = disconnect_clients(
            |cmd| {
                sent.push(cmd.to_string());
                Ok(if cmd == "status 3" { status.to_string() } else { ">INFO:x\nSUCCESS: client-kill command succeeded\n".to_string() })
            },
            Some("alice"),
            None,
        )
        .unwrap();
        assert_eq!(sent, ["status 3", "client-kill 4 HALT", "client-kill 9 HALT"]);
        assert_eq!(r["disconnected"].as_array().unwrap().len(), 2);
        assert_eq!(r["disconnected"][1]["realAddress"], json!("198.51.100.9:4000"));

        // only one client id; nobody connected is fine; kill errors surface
        let mut sent = Vec::new();
        disconnect_clients(
            |cmd| {
                sent.push(cmd.to_string());
                Ok(if cmd == "status 3" { status.to_string() } else { "SUCCESS: ok\n".into() })
            },
            Some("alice"),
            Some(9),
        )
        .unwrap();
        assert_eq!(sent, ["status 3", "client-kill 9 HALT"]);
        let r = disconnect_clients(|_| Ok(status.to_string()), Some("carol"), None).unwrap();
        assert_eq!(r["disconnected"], json!([]));
        let e = disconnect_clients(
            |cmd| Ok(if cmd == "status 3" { status.to_string() } else { "ERROR: client-kill command failed\n".into() }),
            Some("bob"),
            None,
        )
        .unwrap_err();
        assert_eq!(e.code, "kill-failed");
        assert_eq!(disconnect_clients(|_| Err("no socket".into()), Some("bob"), None).unwrap_err().code, "management-unavailable");
    }

    #[test]
    fn config_schema_is_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/config.json");
        meduza_np_protocol::schema::check_file::<OpenVpnConfig>(&path).unwrap();
    }
}
