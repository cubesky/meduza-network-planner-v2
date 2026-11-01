//! Mihomo plugin (AGENT.md §9.7).
//!
//! - Two explicit modes: `proxy` (HTTP/SOCKS listeners only) and `tproxy` (transparent proxy for selected LAN
//!   sources on PREROUTING; the router's own OUTPUT traffic is never redirected). Only connections that start at
//!   the node's LAN are proxied, never destinations of the routing system, and a side router sends the answers of
//!   the main router's port forwards back to it (D59).
//! - Subscriptions are downloaded by the wrapper (`run` mode) and *normalized*: every field this project
//!   controls (listeners, API, DNS, TUN, interface binding) is replaced, never shallow-merged, and the overridden
//!   keys are recorded so the UI can show the final result.
//! - TPROXY uses an owned `inet meduza_np_mihomo` table whose rules really reference the proxy-server set
//!   (filled by the wrapper with resolved addresses) plus automatic exclusions for mesh/VPN/Access devices and
//!   local destinations; fwmark and routing table numbers are reserved resources checked for conflicts.

mod dns;
pub mod runner;

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use ipnet::IpNet;
use meduza_np_plugin_sdk::{PluginError, PluginRequest};
use meduza_np_protocol::digest::sha256_hex;
use meduza_np_protocol::ids::StableId;
use meduza_np_protocol::plan::{GenerationPlan, IpFamily, NetworkResource, PlannedFile, ProbeSpec, ProcessSpec, RestartPolicy};
use meduza_np_protocol::{ValidationError, Validator, PLUGIN_PROTOCOL_VERSION};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SLOT: &str = "mihomo.binary";
pub const TABLE: &str = "meduza_np_mihomo";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(title = "Mihomo")]
pub struct MihomoConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default = "default_mixed")]
    pub mixed_port: u16,
    /// Optional dedicated SOCKS listener (the previous planner used 7891).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socks_port: Option<u16>,
    /// Listen only on loopback unless explicitly allowed.
    #[serde(default)]
    pub allow_lan: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_address: Option<IpAddr>,
    #[serde(default)]
    pub api: Api,
    #[serde(default)]
    pub dns: MihomoDns,
    #[serde(default)]
    pub subscriptions: Vec<Subscription>,
    /// Active subscription id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_subscription: Option<String>,
    #[serde(default)]
    pub tproxy: Tproxy,
    /// Proxy used to fetch subscriptions (never Mihomo itself before it runs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_proxy: Option<String>,
}

fn default_true() -> bool {
    true
}
fn default_mixed() -> u16 {
    7890
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Proxy,
    Tproxy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Api {
    #[serde(default = "default_api_addr")]
    pub address: IpAddr,
    #[serde(default = "default_api_port")]
    pub port: u16,
    /// Secret id; a per-node random secret, never a project-wide constant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<String>,
}

impl Default for Api {
    fn default() -> Self {
        Self { address: default_api_addr(), port: default_api_port(), secret_ref: None }
    }
}

fn default_api_addr() -> IpAddr {
    IpAddr::from([127, 0, 0, 1])
}
fn default_api_port() -> u16 {
    9090
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MihomoDns {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_dns_listen")]
    pub listen: SocketAddr,
    #[serde(default)]
    pub fake_ip: bool,
    /// Upstreams for Mihomo DNS. Must not include the dnsmasq front end (loop).
    #[serde(default)]
    pub nameservers: Vec<String>,
    /// Plain IP resolvers used to resolve DoH/DoT names and proxy servers (bootstrap path).
    #[serde(default)]
    pub default_nameservers: Vec<IpAddr>,
    #[serde(default)]
    pub fallback: Vec<String>,
    #[serde(default)]
    pub nameserver_policy: BTreeMap<String, DnsResolvers>,
    /// Independent resolution of proxy server hostnames; required by respectRules.
    #[serde(default)]
    pub proxy_server_nameservers: Vec<String>,
    #[serde(default)]
    pub proxy_server_nameserver_policy: BTreeMap<String, DnsResolvers>,
    #[serde(default)]
    pub direct_nameservers: Vec<String>,
    #[serde(default)]
    pub direct_nameserver_follow_policy: bool,
    #[serde(default)]
    pub respect_rules: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_filter: Option<FallbackFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fake_ip_range: Option<IpNet>,
    #[serde(default)]
    pub fake_ip_filter: Vec<String>,
    #[serde(default)]
    pub fake_ip_filter_mode: FakeIpFilterMode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum DnsResolvers {
    One(String),
    Many(Vec<String>),
}

impl DnsResolvers {
    fn servers(&self) -> &[String] {
        match self {
            Self::One(s) => std::slice::from_ref(s),
            Self::Many(s) => s,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FakeIpFilterMode {
    #[default]
    Blacklist,
    Whitelist,
    Rule,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FallbackFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geoip: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geoip_code: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub geosite: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ipcidr: Vec<IpNet>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domain: Vec<String>,
}

impl Default for MihomoDns {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: default_dns_listen(),
            fake_ip: false,
            nameservers: Vec::new(),
            default_nameservers: Vec::new(),
            fallback: Vec::new(),
            nameserver_policy: BTreeMap::new(),
            proxy_server_nameservers: Vec::new(),
            proxy_server_nameserver_policy: BTreeMap::new(),
            direct_nameservers: Vec::new(),
            direct_nameserver_follow_policy: false,
            respect_rules: false,
            fallback_filter: None,
            fake_ip_range: None,
            fake_ip_filter: Vec::new(),
            fake_ip_filter_mode: FakeIpFilterMode::default(),
        }
    }
}

fn default_dns_listen() -> SocketAddr {
    SocketAddr::new(IpAddr::from([127, 0, 0, 1]), 1053)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
    pub id: StableId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Subscription URL secret (URLs usually embed a token).
    pub url_ref: String,
    #[serde(default = "default_refresh")]
    pub refresh_hours: u32,
    /// Refresh interval in seconds; replaces `refreshHours` (the manager sets it from the global Mihomo settings).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 60))]
    pub refresh_seconds: Option<u32>,
}

fn default_refresh() -> u32 {
    12
}

impl Subscription {
    /// Seconds between automatic refreshes.
    pub fn refresh_secs(&self) -> u64 {
        self.refresh_seconds.map(u64::from).unwrap_or(u64::from(self.refresh_hours) * 3600)
    }
}

/// File the `refresh-subscription` action creates; the wrapper re-downloads every subscription when it sees it.
pub fn refresh_trigger(run_dir: &str) -> String {
    format!("{}/mihomo-main.refresh", run_dir.trim_end_matches('/'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Protocols {
    Tcp,
    Udp,
    #[default]
    Both,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Tproxy {
    #[serde(default = "default_tproxy_port")]
    pub port: u16,
    /// Source ranges whose forwarded traffic is proxied (defaults to the node's LAN).
    #[serde(default)]
    pub sources: TproxySources,
    #[serde(default)]
    pub inbound_interfaces: Vec<String>,
    #[serde(default)]
    pub exclude_interfaces: Vec<String>,
    #[serde(default)]
    pub exclude_sources: Vec<IpNet>,
    /// Destinations in RFC 1918 and other private/special ranges are never proxied.
    #[serde(default = "default_true")]
    pub exclude_private_destinations: bool,
    #[serde(default)]
    pub exclude_destinations: Vec<IpNet>,
    /// Which protocols are proxied: tcp, udp or both.
    #[serde(default)]
    pub protocols: Protocols,
    /// Destination ports never proxied (TCP and UDP).
    #[serde(default)]
    pub exclude_ports: Vec<u16>,
    /// Legacy-compatible [(in|out):][(tcp|udp):]port. in = destination, out = source; no direction = both.
    #[serde(default)]
    pub exclude_port_rules: Vec<String>,
    /// Decide once per connection and remember it in the connection's conntrack mark (needs nf_conntrack). Without
    /// conntrack every packet is classified again and established proxied TCP connections are found with a socket
    /// lookup (needs nft_socket).
    #[serde(default = "default_true")]
    pub use_conntrack: bool,
    /// Destinations routed over the mesh, VPN or Access devices (routes learned from BGP/OSPF or another routing
    /// system) are never proxied: they stay with the routing system.
    #[serde(default = "default_true")]
    pub bypass_routed: bool,
    /// This node is a side router: clients use it as their gateway (handed out by the main router's DHCP) while the
    /// main router keeps the WAN and its port forwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side_router: Option<SideRouter>,
    #[serde(default = "default_mark")]
    pub fwmark: u32,
    #[serde(default = "default_table")]
    pub route_table: u32,
    #[serde(default = "default_rule_priority")]
    pub rule_priority: u32,
}

impl Default for Tproxy {
    fn default() -> Self {
        Self {
            port: default_tproxy_port(),
            sources: TproxySources::default(),
            inbound_interfaces: Vec::new(),
            exclude_interfaces: Vec::new(),
            exclude_sources: Vec::new(),
            exclude_private_destinations: true,
            exclude_destinations: Vec::new(),
            protocols: Protocols::Both,
            exclude_ports: Vec::new(),
            exclude_port_rules: Vec::new(),
            use_conntrack: true,
            bypass_routed: true,
            side_router: None,
            fwmark: default_mark(),
            route_table: default_table(),
            rule_priority: default_rule_priority(),
        }
    }
}

/// Side-router mode (D59). A port forward on the main router (WAN → lan_a:9009) reaches lan_a directly, but lan_a
/// answers through its gateway, this node. Such answers are never proxied and leave through `gateway` from a routing
/// table of their own, whatever BGP/OSPF installed in the main table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SideRouter {
    /// The main router's LAN address.
    pub gateway: Ipv4Addr,
    /// Source ports of the main router's forwarded services. TCP answers are recognized on their own (they never
    /// start a connection); UDP answers only by these ports.
    #[serde(default)]
    pub reply_ports: Vec<u16>,
}

fn default_tproxy_port() -> u16 {
    7894
}
fn default_mark() -> u32 {
    0x4d4e // "MN"
}
fn default_table() -> u32 {
    4626
}
fn default_rule_priority() -> u32 {
    9626
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TproxySources {
    #[serde(default = "default_true")]
    pub lan: bool,
    #[serde(default)]
    pub private_lan: bool,
    #[serde(default)]
    pub extra: Vec<IpNet>,
}

impl Default for TproxySources {
    fn default() -> Self {
        Self { lan: true, private_lan: false, extra: Vec::new() }
    }
}

pub const PRIVATE_V4: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "224.0.0.0/4",
    "240.0.0.0/4",
];

pub fn load(req: &PluginRequest) -> Result<Option<MihomoConfig>, PluginError> {
    req.service::<MihomoConfig>("mihomo")
}

/// Devices of other features that must never be intercepted (mesh, VPN, Access).
pub fn auto_excluded_devices(req: &PluginRequest) -> Vec<String> {
    let mut out = BTreeSet::new();
    let svc = &req.config.services;
    for (pkg, prefix) in [("wireguard", "wg-"), ("tinc", "tnc-"), ("openvpn", "tun-")] {
        let service = svc.get(pkg);
        // services.tinc is one mesh object (id `mesh`) since D22; earlier documents list instances
        let single = match service {
            Some(s) if pkg == "tinc" && s.get("instances").is_none() => {
                let mut mesh = s.clone();
                if mesh.get("id").is_none() {
                    mesh["id"] = Value::from("mesh");
                }
                vec![mesh]
            }
            _ => Vec::new(),
        };
        let listed = service.and_then(|s| s.get("instances")).and_then(Value::as_array).cloned().unwrap_or_default();
        for inst in single.iter().chain(&listed) {
            if inst.get("enabled") == Some(&Value::Bool(false)) {
                continue;
            }
            let id = inst.get("id").and_then(Value::as_str).unwrap_or_default();
            let dev = inst.get("device").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| {
                let p = if pkg == "openvpn" && inst.get("devType").and_then(Value::as_str) == Some("tap") { "tap-" } else { prefix };
                meduza_np_protocol::ids::derive_ifname(p, id)
            });
            out.insert(dev);
        }
    }
    out.into_iter().collect()
}

pub fn tproxy_sources(cfg: &MihomoConfig, req: &PluginRequest) -> Vec<IpNet> {
    let nets = &req.config.networks;
    let mut s: BTreeSet<IpNet> = BTreeSet::new();
    if cfg.tproxy.sources.lan {
        s.extend(nets.lan.iter().filter(|n| n.addr().is_ipv4()));
    }
    if cfg.tproxy.sources.private_lan {
        s.extend(nets.private_lan.iter().filter(|n| n.addr().is_ipv4()));
    }
    s.extend(cfg.tproxy.sources.extra.iter().filter(|n| n.addr().is_ipv4()));
    s.into_iter().collect()
}

fn frontend_listeners(req: &PluginRequest) -> Vec<SocketAddr> {
    let Some(d) = req.config.services.get("dnsmasq") else { return Vec::new() };
    if d.get("enabled").and_then(Value::as_bool) == Some(false) {
        return Vec::new();
    }
    let port = d.get("port").and_then(Value::as_u64).unwrap_or(53) as u16;
    d.get("listenAddresses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| a.as_str()?.parse::<IpAddr>().ok())
        .map(|a| SocketAddr::new(a, port))
        .collect()
}

pub fn validate(cfg: &MihomoConfig, req: &PluginRequest) -> Vec<ValidationError> {
    let mut v = Validator::new();
    if !cfg.enabled {
        return Vec::new();
    }
    match &cfg.api.secret_ref {
        None => v.error("/api/secretRef", "an API secret is required (generate a per-node secret)"),
        Some(r) => v.check(
            req.config.secrets.get(r).map(|s| s.value.len() >= 16).unwrap_or(false),
            "/api/secretRef",
            "API secret missing or shorter than 16 characters",
        ),
    }
    let mut ids = BTreeSet::new();
    for (i, s) in cfg.subscriptions.iter().enumerate() {
        v.check(ids.insert(s.id.clone()), format!("/subscriptions/{i}/id"), "duplicate id");
        v.check(req.config.secrets.contains_key(&s.url_ref), format!("/subscriptions/{i}/urlRef"), "subscription URL secret not delivered");
        match s.refresh_seconds {
            Some(secs) => v.check(secs >= 60, format!("/subscriptions/{i}/refreshSeconds"), "at least 60 seconds"),
            None => v.check(s.refresh_hours >= 1, format!("/subscriptions/{i}/refreshHours"), "at least 1 hour"),
        }
    }
    match &cfg.active_subscription {
        Some(a) => v.check(
            cfg.subscriptions.iter().any(|s| s.id.as_str() == a && s.enabled),
            "/activeSubscription",
            "must name an enabled subscription",
        ),
        None => v.error("/activeSubscription", "choose the active subscription"),
    }
    let mut ports = BTreeSet::from([cfg.mixed_port, cfg.api.port]);
    v.check(cfg.mixed_port != 0, "/mixedPort", "ports are 1-65535");
    v.check(cfg.api.port != 0, "/api/port", "ports are 1-65535");
    v.check(ports.len() == 2, "/api/port", "API and mixed port collide");
    if let Some(port) = cfg.socks_port {
        v.check(port != 0 && ports.insert(port), "/socksPort", "SOCKS port must be non-zero and distinct from other listeners");
    }
    if cfg.dns.enabled {
        v.check(ports.insert(cfg.dns.listen.port()), "/dns/listen", "DNS listener collides with another port");
        let front = frontend_listeners(req);
        let mut local_ips: Vec<_> = front.iter().map(SocketAddr::ip).collect();
        local_ips.extend([cfg.api.address, cfg.dns.listen.ip()]);
        local_ips.extend(cfg.bind_address);
        dns::validate(&cfg.dns, &front, &local_ips, &mut v);
    }
    if cfg.mode == Mode::Tproxy {
        v.check(!tproxy_sources(cfg, req).is_empty(), "/tproxy/sources", "no source ranges selected (LAN is empty?)");
        v.check(
            cfg.tproxy.port != 0 && ports.insert(cfg.tproxy.port),
            "/tproxy/port",
            "TPROXY port must be non-zero and distinct from other listeners",
        );
        v.check(
            cfg.tproxy.fwmark != 0
                && cfg.tproxy.fwmark.checked_add(1).is_some()
                && cfg.tproxy.route_table > 255
                && cfg.tproxy.route_table < 0xFFFF_FF00,
            "/tproxy",
            "use a non-zero fwmark and a private routing table number (256..)",
        );
        if cfg!(target_os = "linux") {
            let nft = ["/usr/sbin/nft", "/sbin/nft", "/usr/bin/nft"].iter().any(|p| std::path::Path::new(p).exists());
            v.check(nft, "/mode", "TPROXY mode needs nftables (nft) on this node");
            let tproxy = ["/sys/module/nft_tproxy", "/sys/module/xt_TPROXY"].iter().any(|p| std::path::Path::new(p).exists())
                || std::fs::read_to_string("/proc/modules").map(|m| m.contains("nft_tproxy")).unwrap_or(false);
            v.check(tproxy, "/mode", "the kernel has no TPROXY support loaded (nft_tproxy); install the matching kernel module package");
        }
        for (i, d) in cfg.tproxy.inbound_interfaces.iter().enumerate() {
            v.check(meduza_np_protocol::ids::validate_ifname(d).is_ok(), format!("/tproxy/inboundInterfaces/{i}"), "invalid interface");
        }
        for (i, d) in cfg.tproxy.exclude_interfaces.iter().enumerate() {
            v.check(meduza_np_protocol::ids::validate_ifname(d).is_ok(), format!("/tproxy/excludeInterfaces/{i}"), "invalid interface");
        }
        for (i, rule) in cfg.tproxy.exclude_port_rules.iter().enumerate() {
            v.check(
                parse_port_rule(rule).is_some(),
                format!("/tproxy/excludePortRules/{i}"),
                "expected [(in|out):][(tcp|udp):]port, port 1-65535",
            );
        }
        if cfg!(target_os = "linux") {
            if cfg.tproxy.use_conntrack {
                v.check(
                    kernel_module("nf_conntrack") || std::path::Path::new("/proc/net/nf_conntrack").exists(),
                    "/tproxy/useConntrack",
                    "the kernel has no connection tracking (nf_conntrack); switch useConntrack off or load the module",
                );
            } else {
                v.check(
                    kernel_module("nft_socket"),
                    "/tproxy/useConntrack",
                    "without conntrack, established connections are found with nft_socket, which the kernel has not loaded",
                );
            }
        }
        v.check(!cfg.tproxy.exclude_ports.contains(&0), "/tproxy/excludePorts", "ports are 1-65535");
        if let Some(sr) = &cfg.tproxy.side_router {
            let nets = &req.config.networks;
            v.check(
                nets.lan.iter().chain(&nets.private_lan).any(|n| n.contains(&IpAddr::V4(sr.gateway))),
                "/tproxy/sideRouter/gateway",
                "the main router must be in this node's LAN",
            );
            v.check(!sr.reply_ports.contains(&0), "/tproxy/sideRouter/replyPorts", "ports are 1-65535");
            v.check(
                cfg.tproxy.route_table + 1 < 0xFFFF_FF00 && cfg.tproxy.fwmark.checked_add(2).is_some(),
                "/tproxy",
                "side-router mode uses fwmark+2 and routeTable+1",
            );
        }
    }
    v.finish().err().unwrap_or_default()
}

fn kernel_module(name: &str) -> bool {
    std::path::Path::new(&format!("/sys/module/{name}")).exists()
        || std::fs::read_to_string("/proc/modules").map(|m| m.lines().any(|l| l.split(' ').next() == Some(name))).unwrap_or(false)
}

/// Keys of a subscription this project controls; any value there is replaced.
pub const CONTROLLED_KEYS: &[&str] = &[
    "port",
    "socks-port",
    "redir-port",
    "tproxy-port",
    "mixed-port",
    "allow-lan",
    "bind-address",
    "external-controller",
    "external-controller-tls",
    "external-controller-unix",
    "external-ui",
    "external-ui-url",
    "external-ui-name",
    "secret",
    "dns",
    "tun",
    "iptables",
    "interface-name",
    "routing-mark",
    "listeners",
    "authentication",
];

/// Normalizes a subscription into the final configuration. Returns (yaml, overridden keys).
pub fn normalize(subscription_yaml: &str, controlled: &serde_json::Map<String, Value>) -> Result<(String, Vec<String>), String> {
    let doc: serde_json::Value = serde_norway::from_str(subscription_yaml).map_err(|e| format!("subscription is not valid YAML: {e}"))?;
    let Value::Object(mut map) = doc else { return Err("subscription is not a YAML mapping".into()) };
    if !map.contains_key("proxies") && !map.contains_key("proxy-providers") {
        return Err("subscription contains no proxies or proxy-providers".into());
    }
    let mut overridden = Vec::new();
    for key in CONTROLLED_KEYS {
        if map.remove(*key).is_some() {
            overridden.push((*key).to_string());
        }
    }
    for (k, v) in controlled {
        map.insert(k.clone(), v.clone());
    }
    let yaml = serde_norway::to_string(&Value::Object(map)).map_err(|e| e.to_string())?;
    Ok((yaml, overridden))
}

/// The controlled part of the configuration (identical for every subscription).
pub fn controlled_fields(cfg: &MihomoConfig, secret: &str) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("mixed-port".into(), Value::from(cfg.mixed_port));
    if let Some(port) = cfg.socks_port {
        m.insert("socks-port".into(), Value::from(port));
    }
    m.insert("allow-lan".into(), Value::Bool(cfg.allow_lan));
    m.insert(
        "bind-address".into(),
        Value::String(
            cfg.bind_address.map(|a| a.to_string()).unwrap_or_else(|| if cfg.allow_lan { "*".into() } else { "127.0.0.1".into() }),
        ),
    );
    m.insert("external-controller".into(), Value::String(SocketAddr::new(cfg.api.address, cfg.api.port).to_string()));
    m.insert("secret".into(), Value::String(secret.into()));
    if cfg.mode == Mode::Tproxy {
        m.insert("tproxy-port".into(), Value::from(cfg.tproxy.port));
        m.insert("routing-mark".into(), Value::from(cfg.tproxy.fwmark + 1)); // own outbound traffic is never re-marked
    }
    m.insert("tun".into(), serde_json::json!({"enable": false}));
    m.insert("dns".into(), dns::controlled(&cfg.dns));
    m
}

/// Mark of side-router answers (D59): routed to the main router from a table of their own.
pub fn reply_mark(t: &Tproxy) -> u32 {
    t.fwmark + 2
}

struct PortRule<'a> {
    direction: Option<&'a str>,
    protocol: Option<&'a str>,
    port: u16,
}

fn parse_port_rule(value: &str) -> Option<PortRule<'_>> {
    let mut parts = value.trim().split(':').peekable();
    let direction = match parts.peek().copied() {
        Some("in" | "out") => parts.next(),
        _ => None,
    };
    let protocol = match parts.peek().copied() {
        Some("tcp" | "udp") => parts.next(),
        _ => None,
    };
    let port = parts.next()?.parse::<u16>().ok().filter(|p| *p != 0)?;
    if parts.next().is_some() {
        return None;
    }
    Some(PortRule { direction, protocol, port })
}

/// Owned nftables table for TPROXY. Proxy server addresses are added to `proxy_servers_v4` by the wrapper.
///
/// Only connections that *start* at a LAN source are proxied (D59): a new flow (TCP: a SYN) that survives every
/// exclusion is handed to Mihomo and, with conntrack, its connection is marked so later packets skip the checks.
/// Everything else is routed normally; in side-router mode the answers of the main router's port forwards (TCP
/// packets of connections this node never saw start, UDP from `replyPorts`) are sent to the main router.
pub fn tproxy_table(cfg: &MihomoConfig, req: &PluginRequest) -> String {
    let t = &cfg.tproxy;
    let sources: Vec<String> = tproxy_sources(cfg, req).iter().map(|n| n.to_string()).collect();
    let mut bypass: Vec<String> =
        if t.exclude_private_destinations { PRIVATE_V4.iter().map(|s| s.to_string()).collect() } else { Vec::new() };
    bypass.extend(t.exclude_destinations.iter().filter(|n| n.addr().is_ipv4()).map(|n| n.to_string()));
    let devices = auto_excluded_devices(req);
    let quoted = |list: &[String]| list.iter().map(|i| format!("\"{i}\"")).collect::<Vec<_>>().join(", ");
    let ports = |list: &[u16]| list.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ");
    let mark = format!("{:#x}", t.fwmark);
    let to = format!("tproxy ip to 127.0.0.1:{} meta mark set {mark}", t.port);
    let protos: &[&str] = match t.protocols {
        Protocols::Tcp => &["tcp"],
        Protocols::Udp => &["udp"],
        Protocols::Both => &["tcp", "udp"],
    };
    let l4 = if protos.len() == 2 { "{ tcp, udp }".to_string() } else { protos[0].to_string() };

    let mut s = format!("table inet {TABLE} {{\n");
    s.push_str("\tset proxy_servers_v4 {\n\t\ttype ipv4_addr\n\t\tflags interval\n\t}\n");
    s.push_str(&format!(
        "\tset sources_v4 {{\n\t\ttype ipv4_addr\n\t\tflags interval\n\t\telements = {{ {} }}\n\t}}\n",
        sources.join(", ")
    ));
    if !bypass.is_empty() {
        s.push_str(&format!(
            "\tset bypass_v4 {{\n\t\ttype ipv4_addr\n\t\tflags interval\n\t\telements = {{ {} }}\n\t}}\n",
            bypass.join(", ")
        ));
    }
    s.push_str("\tchain prerouting {\n\t\ttype filter hook prerouting priority mangle; policy accept;\n");
    s.push_str("\t\tmeta nfproto != ipv4 return\n");
    s.push_str(&format!("\t\tmeta mark {:#x} return comment \"Mihomo outbound\"\n", t.fwmark + 1));
    if !t.inbound_interfaces.is_empty() {
        s.push_str(&format!("\t\tiifname != {{ {} }} return\n", quoted(&t.inbound_interfaces)));
    }
    if !devices.is_empty() {
        s.push_str(&format!("\t\tiifname {{ {} }} return comment \"mesh/vpn/access\"\n", quoted(&devices)));
    }
    if !t.exclude_interfaces.is_empty() {
        s.push_str(&format!("\t\tiifname {{ {} }} return\n", quoted(&t.exclude_interfaces)));
    }
    s.push_str("\t\tip saddr != @sources_v4 return comment \"only this node's LAN\"\n");
    if t.exclude_sources.iter().any(|n| n.addr().is_ipv4()) {
        s.push_str(&format!(
            "\t\tip saddr {{ {} }} return\n",
            t.exclude_sources.iter().filter(|n| n.addr().is_ipv4()).map(|n| n.to_string()).collect::<Vec<_>>().join(", ")
        ));
        if t.use_conntrack && t.exclude_sources.iter().any(|n| n.addr().is_ipv4()) {
            s.push_str(&format!(
                "\t\tct original ip saddr {{ {} }} return\n",
                t.exclude_sources.iter().filter(|n| n.addr().is_ipv4()).map(|n| n.to_string()).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    s.push_str("\t\tfib daddr type local return comment \"traffic to this node\"\n");
    s.push_str(&format!("\t\tmeta l4proto {{ tcp, udp }} th dport {} return comment \"TPROXY listener\"\n", t.port));
    if !t.exclude_ports.is_empty() {
        s.push_str(&format!("\t\tmeta l4proto {{ tcp, udp }} th dport {{ {} }} return\n", ports(&t.exclude_ports)));
    }
    for rule in t.exclude_port_rules.iter().filter_map(|r| parse_port_rule(r)) {
        let directions: &[&str] = match rule.direction {
            Some("in") => &["dport"],
            Some("out") => &["sport"],
            _ => &["sport", "dport"],
        };
        for proto in protos.iter().filter(|p| rule.protocol.is_none_or(|rp| rp == **p)) {
            for direction in directions {
                s.push_str(&format!("\t\tmeta l4proto {proto} th {direction} {} return\n", rule.port));
            }
        }
    }
    // packets of connections already handed to Mihomo
    if t.use_conntrack {
        s.push_str(if t.side_router.is_some() { "\t\tct direction reply goto not_proxied\n" } else { "\t\tct direction reply return\n" });
        s.push_str(&format!("\t\tct mark {mark} meta l4proto {l4} {to} accept\n"));
    } else if protos.contains(&"tcp") {
        s.push_str(&format!("\t\tmeta l4proto tcp socket transparent 1 meta mark set {mark} accept\n"));
    }
    if let Some(sr) = t.side_router.as_ref().filter(|sr| !sr.reply_ports.is_empty()) {
        s.push_str(&format!(
            "\t\tmeta l4proto {{ tcp, udp }} th sport {{ {} }} goto not_proxied comment \"answers of forwarded services\"\n",
            ports(&sr.reply_ports)
        ));
    }
    if !bypass.is_empty() {
        s.push_str("\t\tip daddr @bypass_v4 return\n");
    }
    if t.bypass_routed && !devices.is_empty() {
        s.push_str(&format!("\t\tfib daddr oifname {{ {} }} return comment \"routed by the routing system\"\n", quoted(&devices)));
    }
    s.push_str("\t\tip daddr @proxy_servers_v4 return comment \"proxy servers: never loop\"\n");
    // only connections that start here: a TCP packet that is not a SYN belongs to a connection this node did not see
    // start (a port forward's answer, or one older than the rules)
    s.push_str("\t\tmeta l4proto tcp tcp flags & (fin | syn | rst | ack) != syn goto not_proxied\n");
    if t.use_conntrack {
        s.push_str("\t\tct state != new goto not_proxied\n");
    }
    for p in protos {
        let ct = if t.use_conntrack { format!("ct mark set {mark} ") } else { String::new() };
        s.push_str(&format!("\t\tmeta l4proto {p} {ct}{to} accept\n"));
    }
    s.push_str("\t}\n");
    s.push_str("\tchain not_proxied {\n");
    if t.side_router.is_some() {
        s.push_str(&format!("\t\tmeta mark set {:#x} comment \"to the main router\"\n", reply_mark(t)));
    }
    s.push_str("\t}\n");
    if t.side_router.is_some() {
        // answers leave through the interface they came in on: no ICMP redirects telling LAN hosts to skip this node
        s.push_str("\tchain output {\n\t\ttype filter hook output priority filter; policy accept;\n");
        s.push_str("\t\ticmp type redirect drop comment \"side router\"\n\t}\n");
    }
    s.push_str("}\n");
    s
}

pub fn plan(cfg: &MihomoConfig, req: &PluginRequest) -> Result<Vec<GenerationPlan>, PluginError> {
    if !cfg.enabled {
        return Ok(Vec::new());
    }
    let mihomo = req.provider(SLOT)?.path.clone();
    let secret = req.secret(cfg.api.secret_ref.as_deref().unwrap_or_default())?.value.clone();
    let controlled = controlled_fields(cfg, &secret);
    let mut subs = Vec::new();
    for s in cfg.subscriptions.iter().filter(|s| s.enabled) {
        subs.push(serde_json::json!({"id": s.id, "url": req.secret(&s.url_ref)?.value, "refreshSeconds": s.refresh_secs()}));
    }
    // Everything the wrapper needs, including secrets, lives in a 0600 file — not in argv.
    let spec = serde_json::json!({
        "controlled": controlled,
        "subscriptions": subs,
        "active": cfg.active_subscription,
        "api": SocketAddr::new(cfg.api.address, cfg.api.port).to_string(),
        "downloadProxy": cfg.download_proxy,
        "stateDir": req.context.state_dir,
        "tproxy": cfg.mode == Mode::Tproxy,
        "table": TABLE,
    });
    let spec_text = serde_json::to_string_pretty(&spec).map_err(|e| PluginError::new("internal", e.to_string()))?;
    let mut resources = Vec::new();
    if cfg.mode == Mode::Tproxy {
        let t = &cfg.tproxy;
        resources.push(NetworkResource::Sysctl { key: "net.ipv4.ip_forward".into(), value: "1".into() });
        resources.push(NetworkResource::Reservation { namespace: "fwmark".into(), value: t.fwmark });
        resources.push(NetworkResource::Reservation { namespace: "fwmark".into(), value: t.fwmark + 1 });
        resources.push(NetworkResource::Reservation { namespace: "rt_table".into(), value: t.route_table });
        resources.push(NetworkResource::Rule {
            priority: t.rule_priority,
            fwmark: t.fwmark,
            mask: None,
            table: t.route_table,
            family: IpFamily::Ipv4,
        });
        resources.push(NetworkResource::Route {
            destination: "0.0.0.0/0".into(),
            via: None,
            device: Some("lo".into()),
            table: Some(t.route_table),
            metric: None,
            route_type: Some("local".into()),
        });
        if let Some(sr) = &t.side_router {
            resources.push(NetworkResource::Reservation { namespace: "fwmark".into(), value: reply_mark(t) });
            resources.push(NetworkResource::Reservation { namespace: "rt_table".into(), value: t.route_table + 1 });
            resources.push(NetworkResource::Rule {
                priority: t.rule_priority + 1,
                fwmark: reply_mark(t),
                mask: None,
                table: t.route_table + 1,
                family: IpFamily::Ipv4,
            });
            resources.push(NetworkResource::Route {
                destination: "0.0.0.0/0".into(),
                via: Some(sr.gateway.to_string()),
                device: None,
                table: Some(t.route_table + 1),
                metric: None,
                route_type: None,
            });
        }
        resources.push(NetworkResource::NftTable { family: "inet".into(), name: TABLE.into(), content: tproxy_table(cfg, req) });
    }
    let libexec = req.context.libexec_dir.trim_end_matches('/');
    Ok(vec![GenerationPlan {
        protocol_version: PLUGIN_PROTOCOL_VERSION,
        request_id: req.request_id.clone(),
        instance: "mihomo/main".into(),
        files: vec![PlannedFile { path: "mihomo/main/spec.json".into(), mode: "0600".into(), content: spec_text.clone(), sensitive: true }],
        processes: vec![ProcessSpec {
            id: "mihomo/main".into(),
            provider_slot: None,
            program: Some(format!("{libexec}/plugins/mihomo")),
            argv: vec![
                "run".into(),
                "--mihomo".into(),
                mihomo,
                "--spec".into(),
                "${GENERATION_ROOT}/mihomo/main/spec.json".into(),
                "--run-dir".into(),
                "${RUN_DIR}".into(),
            ],
            env: Default::default(),
            restart: RestartPolicy::Always,
            stop_timeout_seconds: 10,
            reload_signal: None,
            restart_digest: Some(sha256_hex(spec_text.as_bytes())),
            user: None,
            bound_to: None,
        }],
        resources,
        depends_on: Vec::new(),
        probes: vec![ProbeSpec::Plugin { interval_seconds: 30, params: Value::Null }],
        checks: Vec::new(),
        reload: Vec::new(),
    }])
}

/// Proxy server hosts named in a normalized configuration (for the bypass set).
pub fn proxy_servers(final_yaml: &str) -> Vec<String> {
    let Ok(Value::Object(map)) = serde_norway::from_str::<Value>(final_yaml) else { return Vec::new() };
    let mut out: BTreeMap<String, ()> = BTreeMap::new();
    for p in map.get("proxies").and_then(Value::as_array).into_iter().flatten() {
        if let Some(s) = p.get("server").and_then(Value::as_str) {
            out.insert(s.to_string(), ());
        }
    }
    out.into_keys().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use meduza_np_plugin_sdk::test_request;
    use serde_json::json;

    fn req(m: Value) -> PluginRequest {
        let mut r = test_request(
            "plan",
            json!({
                "networks": {"lan": ["10.42.10.0/24"], "privateLan": ["10.99.10.0/24"]},
                "services": {"mihomo": m,
                    "wireguard": {"instances": [{"id": "uplink", "privateKeyRef": "x"}]},
                    "tinc": {"privateKeyRef": "t", "address": "10.42.1.1/24"},
                    "openvpn": {"instances": [{"id": "access-main", "kind": "access", "device": "tun-access"}]},
                    "dnsmasq": {"listenAddresses": ["10.42.1.1"], "port": 53, "fallbackServers": ["1.0.0.1"]}},
                "secrets": {"api": {"kind": "text", "value": "0123456789abcdef0123"}, "sub1": {"kind": "text", "value": "https://sub.example.test/x?token=t"}}
            }),
        );
        r.providers.insert(
            SLOT.into(),
            meduza_np_plugin_sdk::ResolvedProvider {
                path: "/var/lib/meduza-np/providers/mihomo/mihomo".into(),
                source: "github-release".into(),
                version: Some("v1.19".into()),
            },
        );
        r
    }

    fn base() -> Value {
        json!({"mode": "tproxy", "api": {"secretRef": "api"}, "subscriptions": [{"id": "main", "urlRef": "sub1"}], "activeSubscription": "main",
               "dns": {"enabled": true, "nameservers": ["https://dns.example/dns-query"], "defaultNameservers": ["223.5.5.5"]}})
    }

    #[test]
    fn tproxy_rules_reference_sets_and_exclusions() {
        let r = req(base());
        let cfg = load(&r).unwrap().unwrap();
        // Host facts (nft binary, tproxy/conntrack modules) vary between test machines; everything else must be clean.
        let errs: Vec<_> = validate(&cfg, &r).into_iter().filter(|e| e.path != "/mode" && e.path != "/tproxy/useConntrack").collect();
        assert!(errs.is_empty(), "{errs:?}");
        let t = tproxy_table(&cfg, &r);
        assert!(t.contains("set proxy_servers_v4"));
        assert!(t.contains("ip daddr @proxy_servers_v4 return"), "the set is really referenced");
        assert!(t.contains("elements = { 10.42.10.0/24 }"), "sources from LAN, private LAN not selected");
        assert!(t.contains("iifname { \"tnc-mesh\", \"tun-access\", \"wg-uplink\" } return"), "mesh/vpn/access excluded automatically");
        assert!(
            t.contains("fib daddr oifname { \"tnc-mesh\", \"tun-access\", \"wg-uplink\" } return"),
            "routes of the routing system stay there"
        );
        assert!(t.contains("fib daddr type local return"));
        assert!(t.contains("hook prerouting"), "PREROUTING only");
        assert!(!t.contains("hook output"), "never the node's own OUTPUT");
        // conntrack (default): decided once per connection
        assert!(t.contains("ct mark 0x4d4e meta l4proto { tcp, udp } tproxy ip to 127.0.0.1:7894 meta mark set 0x4d4e accept"));
        assert!(t.contains("ct state != new goto not_proxied"));
        assert!(t.contains("meta l4proto tcp ct mark set 0x4d4e tproxy ip to 127.0.0.1:7894 meta mark set 0x4d4e accept"));
        assert!(t.contains("tcp flags & (fin | syn | rst | ack) != syn goto not_proxied"), "only connections that start here");
        let plans = plan(&cfg, &r).unwrap();
        assert!(plans[0].validate().is_empty(), "{:?}", plans[0].validate().errors());
        assert!(plans[0].resources.iter().any(|x| matches!(x, NetworkResource::Rule { table: 4626, fwmark: 0x4d4e, .. })));
        assert!(plans[0].resources.iter().any(|x| matches!(x, NetworkResource::Route { route_type: Some(t), .. } if t == "local")));
        assert!(
            plans[0]
                .resources
                .iter()
                .any(|x| matches!(x, NetworkResource::Sysctl { key, value } if key == "net.ipv4.ip_forward" && value == "1")),
            "forwarding is required even without NAT rules"
        );
        let argv = plans[0].processes[0].argv.join(" ");
        assert!(!argv.contains("token=t") && !argv.contains("0123456789abcdef"), "no secrets in argv");
        assert!(plans[0].files[0].sensitive);
    }

    #[test]
    fn tproxy_options_and_side_router() {
        let mut m = base();
        m["tproxy"] = json!({"protocols": "tcp", "excludePorts": [22, 53], "excludePrivateDestinations": false, "useConntrack": false,
            "bypassRouted": false, "sideRouter": {"gateway": "10.42.10.1", "replyPorts": [9009]}});
        let r = req(m.clone());
        let cfg = load(&r).unwrap().unwrap();
        let errs: Vec<_> = validate(&cfg, &r).into_iter().filter(|e| e.path != "/mode" && e.path != "/tproxy/useConntrack").collect();
        assert!(errs.is_empty(), "{errs:?}");
        let t = tproxy_table(&cfg, &r);
        assert!(!t.contains("set bypass_v4"), "RFC 1918 destinations proxied when asked");
        assert!(t.contains("th dport { 22, 53 } return"));
        assert!(!t.contains("meta l4proto udp"), "TCP only");
        assert!(!t.contains("ct mark") && !t.contains("ct state") && !t.contains("ct direction"), "no conntrack expressions");
        assert!(t.contains("meta l4proto tcp socket transparent 1 meta mark set 0x4d4e accept"), "established connections by socket");
        assert!(t.contains("meta l4proto tcp tproxy ip to 127.0.0.1:7894 meta mark set 0x4d4e accept"));
        assert!(!t.contains("fib daddr oifname"), "routed destinations proxied when asked");
        assert!(t.contains("th sport { 9009 } goto not_proxied"));
        assert!(t.contains("chain not_proxied {\n\t\tmeta mark set 0x4d50"), "answers are marked for the main router");
        assert!(t.contains("icmp type redirect drop"));
        let plans = plan(&cfg, &r).unwrap();
        assert!(plans[0].validate().is_empty(), "{:?}", plans[0].validate().errors());
        let res = &plans[0].resources;
        assert!(res.iter().any(|x| matches!(x, NetworkResource::Rule { table: 4627, fwmark: 0x4d50, priority: 9627, .. })));
        assert!(res.iter().any(|x| matches!(x, NetworkResource::Route { via: Some(v), table: Some(4627), .. } if v == "10.42.10.1")));
        assert!(res.iter().any(|x| matches!(x, NetworkResource::Reservation { value: 0x4d50, .. })));
        // the main router must be on the LAN
        m["tproxy"]["sideRouter"]["gateway"] = json!("192.0.2.1");
        let r = req(m);
        assert!(validate(&load(&r).unwrap().unwrap(), &r).iter().any(|e| e.path == "/tproxy/sideRouter/gateway"));
    }

    #[test]
    fn normalization_replaces_controlled_fields() {
        let sub = "port: 1\nmixed-port: 2\nallow-lan: true\nexternal-controller: 0.0.0.0:9999\nsecret: leaked\ntun:\n  enable: true\ndns:\n  enable: true\n  listen: 0.0.0.0:53\nproxies:\n  - name: a\n    type: ss\n    server: srv.example.test\n    port: 443\nrules:\n  - MATCH,a\n";
        let r = req(base());
        let cfg = load(&r).unwrap().unwrap();
        let (yaml, overridden) = normalize(sub, &controlled_fields(&cfg, "s3cret-s3cret-s3")).unwrap();
        for k in ["port", "mixed-port", "allow-lan", "external-controller", "secret", "tun", "dns"] {
            assert!(overridden.contains(&k.to_string()), "{k} reported as overridden");
        }
        let v: Value = serde_norway::from_str(&yaml).unwrap();
        assert_eq!(v["mixed-port"], json!(7890));
        assert_eq!(v["external-controller"], json!("127.0.0.1:9090"));
        assert_eq!(v["tun"]["enable"], json!(false));
        assert_eq!(v["dns"]["listen"], json!("127.0.0.1:1053"));
        assert!(v.get("port").is_none(), "no residual listener from a shallow merge");
        assert_eq!(v["proxies"][0]["server"], json!("srv.example.test"));
        assert_eq!(proxy_servers(&yaml), vec!["srv.example.test"]);
        assert!(normalize("rules: []\n", &controlled_fields(&cfg, "x")).is_err(), "no proxies → rejected");
    }

    #[test]
    fn validation() {
        let mut m = base();
        m["api"] = json!({});
        m["dns"]["nameservers"] = json!(["10.42.1.1"]);
        m["dns"]["listen"] = json!("10.42.1.1:53");
        m["activeSubscription"] = json!("nope");
        let r = req(m);
        let text = format!("{:?}", validate(&load(&r).unwrap().unwrap(), &r));
        for needle in ["API secret is required", "loop", "own listener", "enabled subscription"] {
            assert!(text.contains(needle), "missing {needle}: {text}");
        }
    }

    #[test]
    fn advanced_dns_is_project_owned_and_normalized() {
        let mut m = base();
        m["socksPort"] = json!(7891);
        m["dns"] = json!({
            "enabled": true, "fakeIp": true, "nameservers": ["https://dns.example/dns-query"],
            "defaultNameservers": ["119.29.29.29"], "fallback": ["tls://1.1.1.1:853"],
            "nameserverPolicy": {"+.example.cn": "223.5.5.5", "geosite:private": ["192.168.1.1"]},
            "proxyServerNameservers": ["119.29.29.29"],
            "proxyServerNameserverPolicy": {"+.proxy.example": ["223.5.5.5"]},
            "directNameservers": ["223.5.5.5"], "directNameserverFollowPolicy": true, "respectRules": true,
            "fallbackFilter": {"geoip": true, "geoipCode": "CN", "geosite": ["gfw"], "ipcidr": ["240.0.0.0/4"], "domain": ["+.example.org"]},
            "fakeIpRange": "198.18.0.1/16", "fakeIpFilter": ["*.lan", "+.example.cn"], "fakeIpFilterMode": "blacklist"
        });
        let r = req(m);
        let cfg = load(&r).unwrap().unwrap();
        assert!(validate(&cfg, &r).into_iter().all(|e| e.path == "/mode" || e.path == "/tproxy/useConntrack"));
        let (yaml, _) = normalize("proxies: []\ndns:\n  listen: 0.0.0.0:53\n  nameserver: [127.0.0.1]\n  fake-ip-filter: [subscription.example]\nsocks-port: 9999\n", &controlled_fields(&cfg, "secret")).unwrap();
        let doc: Value = serde_norway::from_str(&yaml).unwrap();
        assert_eq!(doc["socks-port"], 7891);
        let dns = &doc["dns"];
        assert_eq!(dns["listen"], "127.0.0.1:1053");
        assert_eq!(dns["enhanced-mode"], "fake-ip");
        assert_eq!(dns["nameserver-policy"]["+.example.cn"], "223.5.5.5");
        assert_eq!(dns["proxy-server-nameserver-policy"]["+.proxy.example"], json!(["223.5.5.5"]));
        assert_eq!(dns["fallback"], json!(["tls://1.1.1.1:853"]));
        assert_eq!(dns["proxy-server-nameserver"], json!(["119.29.29.29"]));
        assert_eq!(dns["direct-nameserver"], json!(["223.5.5.5"]));
        assert_eq!(dns["respect-rules"], true);
        assert_eq!(dns["direct-nameserver-follow-policy"], true);
        assert_eq!(dns["fallback-filter"]["geoip-code"], "CN");
        assert!(dns["fallback-filter"].get("geoipCode").is_none());
        assert_eq!(dns["fake-ip-range"], "198.18.0.1/16");
        assert_eq!(dns["fake-ip-filter"], json!(["*.lan", "+.example.cn"]));
    }

    #[test]
    fn every_dns_upstream_is_checked_for_loops_and_bootstrap() {
        for field in ["nameservers", "fallback", "proxyServerNameservers", "directNameservers", "defaultNameservers"] {
            let mut m = base();
            m["dns"][field] = json!(["10.42.1.1"]);
            let r = req(m);
            assert!(validate(&load(&r).unwrap().unwrap(), &r).iter().any(|e| e.path == format!("/dns/{field}/0")), "{field}");
        }
        for field in ["nameserverPolicy", "proxyServerNameserverPolicy"] {
            for target in [json!("tcp://127.0.0.1:1053#DIRECT"), json!(["udp://10.42.1.1:53"])] {
                let mut m = base();
                m["dns"][field] = json!({"+.example/zone": target});
                let r = req(m);
                assert!(
                    validate(&load(&r).unwrap().unwrap(), &r).iter().any(|e| e.path == format!("/dns/{field}/+.example~1zone")),
                    "{field}"
                );
            }
        }
        let mut m = base();
        m["dns"]["nameservers"] = json!(["10.42.1.10", "10.42.1.1:5353", "https://10.42.1.1/dns-query"]);
        let r = req(m.clone());
        assert!(
            !validate(&load(&r).unwrap().unwrap(), &r).iter().any(|e| e.path.starts_with("/dns")),
            "no substring false positives; distinct ports/transports"
        );
        m["dns"]["nameservers"] = json!(["119.29.29.29"]);
        m["dns"]["defaultNameservers"] = json!([]);
        m["dns"]["fallback"] = json!(["https://dns.example/dns-query"]);
        m["dns"]["respectRules"] = json!(true);
        let r = req(m);
        let errors = validate(&load(&r).unwrap().unwrap(), &r);
        assert!(errors.iter().any(|e| e.path == "/dns/defaultNameservers"));
        assert!(errors.iter().any(|e| e.path == "/dns/proxyServerNameservers"));
        // DNS is still validated without a front end. A wildcard listen reaches loopback.
        let mut m = base();
        m["dns"]["listen"] = json!("0.0.0.0:1053");
        m["dns"]["nameservers"] = json!(["tcp://localhost:1053"]);
        let mut r = req(m);
        r.config.services.get_mut("dnsmasq").unwrap()["enabled"] = json!(false);
        assert!(validate(&load(&r).unwrap().unwrap(), &r).iter().any(|e| e.path == "/dns/nameservers/0"));
        // Disabled dnsmasq's saved listener is harmless.
        r.config.services.get_mut("mihomo").unwrap()["dns"]["listen"] = json!("10.42.1.1:53");
        r.config.services.get_mut("mihomo").unwrap()["dns"]["nameservers"] = json!(["119.29.29.29"]);
        assert!(!validate(&load(&r).unwrap().unwrap(), &r).iter().any(|e| e.path.starts_with("/dns")));
    }

    #[test]
    fn legacy_port_exclusions_preserve_direction_protocol_and_conntrack_source() {
        let mut m = base();
        m["tproxy"] = json!({"excludePortRules": ["53", "in:tcp:22", "out:udp:123"],
            "excludeInterfaces": ["wan0"], "excludeSources": ["10.42.10.1/32"], "sideRouter": {"gateway": "10.42.10.1"}});
        let r = req(m.clone());
        let cfg = load(&r).unwrap().unwrap();
        assert!(!validate(&cfg, &r).iter().any(|e| e.path.starts_with("/tproxy/exclude")));
        let table = tproxy_table(&cfg, &r);
        for rule in ["tcp th sport 53", "tcp th dport 53", "udp th sport 53", "udp th dport 53", "tcp th dport 22", "udp th sport 123"] {
            assert!(table.contains(rule), "{rule}");
        }
        assert!(!table.contains("tcp th sport 22") && !table.contains("udp th dport 123"));
        assert!(table.contains("iifname { \"wan0\" } return"));
        assert!(table.contains("ct original ip saddr { 10.42.10.1/32 } return"));
        assert!(table.contains("ct direction reply goto not_proxied"));
        assert!(
            table.find("th sport 53").unwrap() < table.find("ct mark 0x4d4e").unwrap(),
            "exclusions apply before cached proxy decisions"
        );
        assert!(table.contains("meta mark 0x4d4f return"));
        assert_eq!(table.matches("tproxy ip to").count(), 3, "only PREROUTING proxies; OUTPUT drops ICMP redirects only");
        for invalid in ["in:udp:0", "65536", "out:icmp:53", "in:out:22", "53;drop"] {
            m["tproxy"]["excludePortRules"] = json!([invalid]);
            let r = req(m.clone());
            assert!(validate(&load(&r).unwrap().unwrap(), &r).iter().any(|e| e.path == "/tproxy/excludePortRules/0"), "{invalid}");
        }
        m["socksPort"] = json!(7890);
        let r = req(m);
        assert!(validate(&load(&r).unwrap().unwrap(), &r).iter().any(|e| e.path == "/socksPort"));
    }

    #[test]
    fn refresh_interval_in_seconds() {
        // The manager sets refreshSeconds from the global setting; the wrapper gets it in its spec.
        let mut m = base();
        m["mode"] = json!("proxy"); // no TPROXY kernel support needed for this check
        m["subscriptions"][0]["refreshSeconds"] = json!(600);
        let r = req(m.clone());
        let cfg = load(&r).unwrap().unwrap();
        assert!(validate(&cfg, &r).is_empty(), "{:?}", validate(&cfg, &r));
        let spec = &plan(&cfg, &r).unwrap()[0].files[0].content;
        assert!(spec.contains("\"refreshSeconds\": 600"), "{spec}");
        m["subscriptions"][0]["refreshSeconds"] = json!(30);
        let r = req(m);
        assert!(validate(&load(&r).unwrap().unwrap(), &r).iter().any(|e| e.path == "/subscriptions/0/refreshSeconds"));
        // Earlier documents: refreshHours.
        let r = req(base());
        assert_eq!(load(&r).unwrap().unwrap().subscriptions[0].refresh_secs(), 12 * 3600);
        assert_eq!(refresh_trigger("/run/meduza-np/"), "/run/meduza-np/mihomo-main.refresh");
    }

    #[test]
    fn config_schema_is_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/config.json");
        meduza_np_protocol::schema::check_file::<MihomoConfig>(&path).unwrap();
    }
}
