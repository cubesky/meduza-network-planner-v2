//! dnsmasq front-end DNS (AGENT.md §9.10, §10.5).
//!
//! dnsmasq is the client-facing resolver. It starts with the explicit fallback servers only; the front-end
//! wrapper probes each enabled backend (MosDNS, Mihomo DNS) with real queries and only then lists it in the
//! `servers-file`, removing it as soon as it fails and restoring the fallback when no backend is healthy.
//! dnsmasq re-reads `servers-file` and hosts on SIGHUP, so switching never restarts the front end. DHCP is never
//! enabled, `/etc/resolv.conf` is never touched, and an existing system instance on the port is reported.

pub mod frontend;

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use meduza_np_plugin_sdk::{PluginError, PluginRequest};
use meduza_np_protocol::digest::sha256_hex;
use meduza_np_protocol::ids::StableId;
use meduza_np_protocol::plan::{GenerationPlan, PlannedFile, ProbeSpec, ProcessSpec, RestartPolicy};
use meduza_np_protocol::{ValidationError, Validator, PLUGIN_PROTOCOL_VERSION};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SLOT: &str = "dnsmasq.binary";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(title = "dnsmasq")]
pub struct DnsmasqConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Addresses clients query. Omitted or empty listens on all local IPv4/IPv6 interfaces.
    #[serde(default)]
    pub listen_addresses: Vec<IpAddr>,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Explicit fallback resolvers (`ip` or `ip#port`), used before backends are ready and when none is healthy.
    #[serde(default = "default_fallback_servers")]
    pub fallback_servers: Vec<String>,
    /// Derived from enabled DNS services and installed capabilities; legacy manual entries are ignored.
    #[serde(skip)]
    #[schemars(skip)]
    pub backends: Vec<Backend>,
    #[serde(default)]
    pub strategy: Strategy,
    #[serde(default = "default_ttl")]
    pub local_ttl: u32,
    #[serde(default = "default_cache")]
    pub cache_size: u32,
    /// Forward `.local` A/AAAA queries to the Avahi bridge when meduza-np-avahi provides it.
    #[serde(default)]
    pub mdns_forward: bool,
    #[serde(default = "default_probe_name")]
    pub probe_name: String,
}

fn default_true() -> bool {
    true
}
fn default_port() -> u16 {
    53
}
fn default_fallback_servers() -> Vec<String> {
    vec!["119.29.29.29".into(), "1.0.0.1".into()]
}
fn default_ttl() -> u32 {
    60
}
fn default_cache() -> u32 {
    1000
}
fn default_probe_name() -> String {
    "example.com".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    /// Healthy backends in priority order (`strict-order`); the first answering one wins.
    #[default]
    Ordered,
    /// Query all healthy backends in parallel and take the fastest answer (`all-servers`).
    Parallel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    Mosdns,
    Mihomo,
    #[serde(rename = "mihomo-openwrt")]
    MihomoOpenwrt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Backend {
    pub id: StableId,
    pub kind: BackendKind,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_backend_addr")]
    pub address: IpAddr,
    pub port: u16,
    /// Lower = preferred.
    #[serde(default = "default_priority")]
    pub priority: u16,
}

fn default_backend_addr() -> IpAddr {
    IpAddr::from([127, 0, 0, 1])
}
fn default_priority() -> u16 {
    100
}

impl Backend {
    pub fn addr(&self) -> SocketAddr {
        SocketAddr::new(self.address, self.port)
    }
}

pub fn load(req: &PluginRequest) -> Result<Option<DnsmasqConfig>, PluginError> {
    let Some(mut cfg) = req.service::<DnsmasqConfig>("dnsmasq")? else { return Ok(None) };
    if cfg.fallback_servers.is_empty() {
        cfg.fallback_servers = default_fallback_servers();
    }
    if cfg.enabled {
        cfg.backends = discover_backends(req)?;
    }
    Ok(Some(cfg))
}

/// Candidate backends come from the services themselves, never an independently edited address list.
/// Health probing in the wrapper decides which candidates actually enter dnsmasq's servers-file.
pub fn discover_backends(req: &PluginRequest) -> Result<Vec<Backend>, PluginError> {
    let active = |name: &str| req.config.services.get(name).filter(|s| s.get("enabled") != Some(&Value::Bool(false)));
    let mut backends = Vec::new();
    if let Some(service) = active("mosdns").filter(|_| req.has_capability("mosdns.dns")) {
        let address = service
            .get("listen")
            .and_then(Value::as_str)
            .unwrap_or("127.0.0.1")
            .parse::<IpAddr>()
            .map_err(|_| PluginError::new("invalid-config", "invalid MosDNS listen address"))?;
        let port = service
            .get("port")
            .map_or(Some(1153), |p| p.as_u64().and_then(|p| u16::try_from(p).ok()))
            .filter(|p| *p != 0)
            .ok_or_else(|| PluginError::new("invalid-config", "invalid MosDNS DNS port"))?;
        backends.push(Backend {
            id: StableId::new("mosdns").expect("stable id"),
            kind: BackendKind::Mosdns,
            enabled: true,
            address: local_target(address),
            port,
            priority: 10,
        });
    }
    if let Some(service) = active("mihomo").filter(|_| req.has_capability("mihomo.dns")) {
        if let Some(dns) = service.get("dns").filter(|d| d.get("enabled") == Some(&Value::Bool(true))) {
            let listen = dns
                .get("listen")
                .and_then(Value::as_str)
                .unwrap_or("127.0.0.1:1053")
                .parse::<SocketAddr>()
                .map_err(|_| PluginError::new("invalid-config", "invalid Mihomo DNS listener"))?;
            if listen.port() == 0 {
                return Err(PluginError::new("invalid-config", "invalid Mihomo DNS port"));
            }
            backends.push(Backend {
                id: StableId::new("mihomo").expect("stable id"),
                kind: BackendKind::Mihomo,
                enabled: true,
                address: local_target(listen.ip()),
                port: listen.port(),
                priority: 20,
            });
        }
    }
    if active("mihomo-openwrt").is_some_and(|s| s.get("dnsIntegration") == Some(&Value::Bool(true)))
        && req.has_capability("mihomo-openwrt.dns")
    {
        // OpenClash owns its listener. The wrapper reads dnsPort from its live adapter state on every probe;
        // port 0 is an unresolved candidate, never queried or written to servers-file.
        backends.push(Backend {
            id: StableId::new("mihomo-openwrt").expect("stable id"),
            kind: BackendKind::MihomoOpenwrt,
            enabled: true,
            address: IpAddr::from([127, 0, 0, 1]),
            port: 0,
            priority: 20,
        });
    }
    Ok(backends)
}

pub fn local_target(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(a) if a.is_unspecified() => Ipv4Addr::LOCALHOST.into(),
        IpAddr::V6(a) if a.is_unspecified() => Ipv6Addr::LOCALHOST.into(),
        _ => ip,
    }
}

/// `ip` or `ip#port` → socket address.
pub fn parse_server(s: &str) -> Option<SocketAddr> {
    match s.split_once('#') {
        Some((ip, port)) => Some(SocketAddr::new(ip.parse().ok()?, port.parse().ok()?)),
        None => Some(SocketAddr::new(s.parse().ok()?, 53)),
    }
}

pub fn server_line(addr: SocketAddr) -> String {
    format!("server={}#{}", addr.ip(), addr.port())
}

/// Local address of a `/proc/net/{udp,tcp}[6]` row (`0100007F:0035`; IPv6 as four host-order 32-bit words).
pub fn proc_net_addr(field: &str) -> Option<SocketAddr> {
    let (ip, port) = field.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let ip: IpAddr = match ip.len() {
        8 => Ipv4Addr::from(u32::from_str_radix(ip, 16).ok()?.to_ne_bytes()).into(),
        32 => {
            let mut b = [0u8; 16];
            for (i, chunk) in b.chunks_mut(4).enumerate() {
                chunk.copy_from_slice(&u32::from_str_radix(&ip[i * 8..i * 8 + 8], 16).ok()?.to_ne_bytes());
            }
            Ipv6Addr::from(b).to_canonical()
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// Whether a socket bound to `other` keeps dnsmasq from binding `ours` (bind-interfaces: one socket per address).
/// A wildcard collides with its whole family (`::` with both: dual stack); otherwise only the same address does.
pub fn bind_conflicts(other: IpAddr, ours: &[IpAddr]) -> bool {
    let wild = |a: &IpAddr, b: &IpAddr| a.is_unspecified() && (a.is_ipv6() || a.is_ipv4() == b.is_ipv4());
    ours.iter().any(|a| *a == other || wild(&other, a) || wild(a, &other))
}

/// Who holds `port` on an address dnsmasq would bind for `listen` (UDP, or TCP in LISTEN), other than our own
/// front end, from /proc (Linux only; `None` elsewhere). Owners on other addresses (systemd-resolved on
/// 127.0.0.53, aardvark-dns on a Podman bridge) do not collide and are not reported.
pub fn port_owner(port: u16, own_marker: &str, listen: &[IpAddr]) -> Option<String> {
    let mut inodes: BTreeMap<String, SocketAddr> = BTreeMap::new();
    for (table, listen_state) in
        [("/proc/net/udp", None), ("/proc/net/udp6", None), ("/proc/net/tcp", Some("0A")), ("/proc/net/tcp6", Some("0A"))]
    {
        let Ok(text) = std::fs::read_to_string(table) else { continue };
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() <= 9 || listen_state.is_some_and(|s| f[3] != s) {
                continue;
            }
            if let Some(addr) = proc_net_addr(f[1]) {
                if addr.port() == port && bind_conflicts(addr.ip(), listen) {
                    inodes.insert(f[9].to_string(), addr);
                }
            }
        }
    }
    if inodes.is_empty() {
        return None;
    }
    let procs = std::fs::read_dir("/proc").ok()?;
    let mut own_unreadable = false;
    for entry in procs.flatten().take(4096) {
        let pid = entry.file_name().to_string_lossy().into_owned();
        if !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let cmdline = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
        let own = String::from_utf8_lossy(&cmdline).replace('\0', " ").contains(own_marker);
        let comm = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
        // dnsmasq drops to its own user (non-dumpable): without CAP_SYS_PTRACE planner can list its fds but not
        // read their links, so its sockets are invisible.
        let own_dnsmasq = own && comm.trim() == "dnsmasq";
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            own_unreadable |= own_dnsmasq;
            continue;
        };
        for fd in fds.flatten().take(1024) {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                own_unreadable |= own_dnsmasq;
                continue;
            };
            let t = target.to_string_lossy();
            if let Some(addr) = t.strip_prefix("socket:[").and_then(|x| x.strip_suffix(']')).and_then(|i| inodes.get(i)) {
                if own {
                    return None; // our own front end
                }
                return Some(format!("{} (pid {pid}) on {addr}", comm.trim()));
            }
        }
    }
    if own_unreadable {
        // Nothing visible holds the address, and our own front end runs where planner cannot look: it is ours.
        return None;
    }
    let addrs: BTreeSet<String> = inodes.values().map(SocketAddr::to_string).collect();
    Some(format!("an unidentified process on {}", addrs.into_iter().collect::<Vec<_>>().join(", ")))
}

pub fn validate(cfg: &DnsmasqConfig, req: &PluginRequest) -> Vec<ValidationError> {
    let mut v = Validator::new();
    if !cfg.enabled {
        return Vec::new();
    }
    v.check(cfg.port != 0, "/port", "ports are 1-65535");
    v.check(!cfg.fallback_servers.is_empty(), "/fallbackServers", "explicit fallback servers are required (no /etc/resolv.conf)");
    let listen_addresses = listener_addresses(cfg);
    let listeners: BTreeSet<SocketAddr> = listen_addresses.iter().map(|a| SocketAddr::new(*a, cfg.port)).collect();
    for (i, s) in cfg.fallback_servers.iter().enumerate() {
        match parse_server(s) {
            None => v.error(format!("/fallbackServers/{i}"), "expected ip or ip#port"),
            // A fallback pointing at ourselves would loop.
            Some(a) => v.check(
                !listeners.contains(&a)
                    && !(cfg.listen_addresses.is_empty() && a.port() == cfg.port && (a.ip().is_loopback() || a.ip().is_unspecified())),
                format!("/fallbackServers/{i}"),
                "fallback server points at this front end (loop)",
            ),
        }
    }
    let mut ids = BTreeSet::new();
    let mut addrs = BTreeSet::new();
    for (i, b) in cfg.backends.iter().enumerate() {
        let p = format!("/backends/{i}");
        v.check(ids.insert(b.id.clone()), format!("{p}/id"), "duplicate backend id");
        v.check(addrs.insert(b.addr()), format!("{p}/port"), "two backends share an address");
        v.check(
            !listeners.iter().any(|l| l.port() == b.port && bind_conflicts(l.ip(), &[b.address])),
            format!("{p}/port"),
            "backend address equals the front-end listener (loop)",
        );
        if b.enabled {
            // Only installed DNS-capable backends may be referenced (§9.10).
            // A Mihomo backend is either our own Mihomo or the DNS listener of an OpenClash-managed one.
            let caps: &[&str] = match b.kind {
                BackendKind::Mosdns => &["mosdns.dns"],
                BackendKind::Mihomo => &["mihomo.dns"],
                BackendKind::MihomoOpenwrt => &["mihomo-openwrt.dns"],
            };
            v.check(
                caps.iter().any(|c| req.has_capability(c)),
                format!("{p}/kind"),
                format!("{} is not provided by an installed package on this node", caps.join(" or ")),
            );
        }
    }
    if cfg.mdns_forward {
        v.check(req.has_capability("mdns.bridge"), "/mdnsForward", "requires meduza-np-avahi (mdns.bridge)");
    }
    if cfg!(target_os = "linux") {
        if let Some(owner) = port_owner(cfg.port, "meduza-np", &listen_addresses) {
            v.error(
                "/port",
                format!("port {} is already used by {owner}; choose another listen address or port or integrate with the existing DNS instead (it is never killed)", cfg.port),
            );
        }
    }
    v.finish().err().unwrap_or_default()
}

fn mdns_bridge(req: &PluginRequest) -> Option<SocketAddr> {
    let avahi = req.config.services.get("avahi")?;
    let bridge = avahi.get("bridge")?;
    if bridge.get("enabled").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let port = bridge.get("port").and_then(Value::as_u64).unwrap_or(5354) as u16;
    Some(SocketAddr::new(IpAddr::from([127, 0, 0, 1]), port))
}

pub struct Rendered {
    pub conf: String,
    pub hosts: String,
}

/// Wildcards represent dnsmasq's unrestricted default for probes and local conflict checks. In the native
/// configuration that default is expressed by omitting listen-address, not by binding a literal wildcard IP.
pub fn listener_addresses(cfg: &DnsmasqConfig) -> Vec<IpAddr> {
    if cfg.listen_addresses.is_empty() {
        vec![Ipv4Addr::UNSPECIFIED.into(), Ipv6Addr::UNSPECIFIED.into()]
    } else {
        cfg.listen_addresses.clone()
    }
}

pub fn render(cfg: &DnsmasqConfig, req: &PluginRequest) -> Rendered {
    let run = req.context.run_dir.trim_end_matches('/');
    let gen = req.gen_path("dnsmasq/main");
    let mut c = vec![
        "# Meduza NP front-end DNS — generated, do not edit".to_string(),
        "keep-in-foreground".into(),
        "no-resolv".into(),
        "no-poll".into(),
        "no-hosts".into(),
        "domain-needed".into(),
        "bogus-priv".into(),
        "bind-interfaces".into(),
        format!("port={}", cfg.port),
        format!("cache-size={}", cfg.cache_size),
        format!("local-ttl={}", cfg.local_ttl),
        format!("pid-file={run}/dnsmasq-main.pid"),
        format!("addn-hosts={gen}/hosts"),
        // Upstreams are owned by the front-end wrapper (fallback or probed backends), re-read on SIGHUP.
        format!("servers-file={run}/dnsmasq-main.servers"),
    ];
    for a in &cfg.listen_addresses {
        c.push(format!("listen-address={a}"));
    }
    match cfg.strategy {
        Strategy::Ordered => c.push("strict-order".into()),
        Strategy::Parallel => c.push("all-servers".into()),
    }
    if cfg.mdns_forward {
        if let Some(b) = mdns_bridge(req) {
            c.push(format!("server=/local/{}#{}", b.ip(), b.port()));
        }
    }
    let mut conf = c.join("\n");
    conf.push('\n');
    let mut hosts = String::from("# dns.hosts from the node configuration\n");
    for h in req.config.dns.as_ref().map(|d| d.hosts.as_slice()).unwrap_or_default() {
        for ip in &h.ips {
            hosts.push_str(&format!("{ip} {}\n", h.hostname));
        }
    }
    Rendered { conf, hosts }
}

/// Generation file holding the wrapper spec; re-read by the wrapper on SIGHUP.
pub const SPEC_FILE: &str = "dnsmasq/main/frontend.json";

/// Wrapper spec describing the backends and fallback (JSON in the generation, no secrets involved).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrontendSpec {
    pub fallback: Vec<String>,
    pub backends: Vec<Backend>,
    pub probe_name: String,
    pub listen: Vec<SocketAddr>,
}

pub fn plan(cfg: &DnsmasqConfig, req: &PluginRequest) -> Result<Vec<GenerationPlan>, PluginError> {
    if !cfg.enabled {
        return Ok(Vec::new());
    }
    let r = render(cfg, req);
    let fallback: Vec<String> = cfg.fallback_servers.iter().filter_map(|s| parse_server(s)).map(server_line).collect();
    let spec = FrontendSpec {
        fallback,
        backends: cfg.backends.iter().filter(|b| b.enabled).cloned().collect(),
        probe_name: cfg.probe_name.clone(),
        listen: listener_addresses(cfg).iter().map(|a| SocketAddr::new(*a, cfg.port)).collect(),
    };
    let spec_json = serde_json::to_string(&spec).map_err(|e| PluginError::new("internal", e.to_string()))?;
    // Listener/cache/port changes → restart of the front end. Hosts and the wrapper spec (backend set, fallback,
    // probe name) → SIGHUP: the wrapper re-reads its spec file and dnsmasq its hosts, no client outage.
    let restart_digest = sha256_hex(r.conf.as_bytes());
    let libexec = req.context.libexec_dir.trim_end_matches('/');
    // The wrapper execs the resolved dnsmasq provider (system or provider package).
    let dnsmasq = req.provider(SLOT)?.path.clone();
    Ok(vec![GenerationPlan {
        protocol_version: PLUGIN_PROTOCOL_VERSION,
        request_id: req.request_id.clone(),
        instance: "dnsmasq/main".into(),
        files: vec![
            PlannedFile { path: "dnsmasq/main/dnsmasq.conf".into(), mode: "0644".into(), content: r.conf, sensitive: false },
            PlannedFile { path: "dnsmasq/main/hosts".into(), mode: "0644".into(), content: r.hosts, sensitive: false },
            PlannedFile { path: SPEC_FILE.into(), mode: "0644".into(), content: spec_json, sensitive: false },
        ],
        processes: vec![ProcessSpec {
            id: "dnsmasq/main".into(),
            provider_slot: None,
            program: Some(format!("{libexec}/plugins/dnsmasq")),
            argv: vec![
                "frontend".into(),
                "--dnsmasq".into(),
                dnsmasq,
                "--conf".into(),
                "${GENERATION_ROOT}/dnsmasq/main/dnsmasq.conf".into(),
                "--run-dir".into(),
                "${RUN_DIR}".into(),
                "--spec-file".into(),
                format!("${{GENERATION_ROOT}}/{SPEC_FILE}"),
            ],
            env: Default::default(),
            restart: RestartPolicy::Always,
            startup: Default::default(),
            stop_timeout_seconds: 5,
            reload_signal: Some("SIGHUP".into()),
            restart_digest: Some(restart_digest),
            user: None,
            bound_to: None,
        }],
        resources: Vec::new(),
        depends_on: Vec::new(),
        probes: vec![ProbeSpec::Plugin { interval_seconds: 15, params: Value::Null }],
        checks: Vec::new(),
        reload: Vec::new(),
    }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use meduza_np_plugin_sdk::test_request;
    use serde_json::json;

    fn req(d: Value, caps: &[&str]) -> PluginRequest {
        let mut r = test_request(
            "plan",
            json!({"services": {"dnsmasq": d}, "dns": {"hosts": [{"hostname": "nas.lan", "ips": ["10.42.10.5", "fd00::5"]}]}}),
        );
        r.context.capabilities = caps.iter().map(|c| c.to_string()).collect();
        r.providers.insert(
            SLOT.into(),
            meduza_np_plugin_sdk::ResolvedProvider { path: "/usr/sbin/dnsmasq".into(), source: "system".into(), version: None },
        );
        r
    }

    #[test]
    fn omitted_and_empty_listeners_open_all_local_interfaces() {
        for value in [json!({"port":55353}), json!({"listenAddresses":[],"port":55353})] {
            let request = req(value, &[]);
            let cfg = load(&request).unwrap().unwrap();
            assert!(validate(&cfg, &request).is_empty(), "{:?}", validate(&cfg, &request));
            let rendered = render(&cfg, &request);
            assert!(!rendered.conf.contains("listen-address="), "native dnsmasq default accepts every local interface");
            let plan = plan(&cfg, &request).unwrap();
            let spec: FrontendSpec = serde_json::from_str(&plan[0].files.iter().find(|f| f.path == SPEC_FILE).unwrap().content).unwrap();
            assert_eq!(spec.listen, vec!["0.0.0.0:55353".parse::<SocketAddr>().unwrap(), "[::]:55353".parse().unwrap()]);
            assert!(bind_conflicts("127.0.0.53".parse().unwrap(), &listener_addresses(&cfg)));
        }
        let request = req(json!({"port":55353,"fallbackServers":["127.0.0.1#55353","::1#55353"]}), &[]);
        let cfg = load(&request).unwrap().unwrap();
        assert_eq!(validate(&cfg, &request).iter().filter(|e| e.path.starts_with("/fallbackServers/")).count(), 2);
    }

    #[test]
    fn fallback_first_and_spec_lines() {
        let mut r = req(json!({"listenAddresses": ["10.42.1.1"], "port": 53}), &["mosdns.dns", "mihomo.dns"]);
        r.config.services.insert("mosdns".into(), json!({}));
        r.config.services.insert("mihomo".into(), json!({"dns": {"enabled": true}}));
        let cfg = load(&r).unwrap().unwrap();
        let errs: Vec<_> = validate(&cfg, &r).into_iter().filter(|e| !e.message.contains("already used")).collect();
        assert!(errs.is_empty(), "{errs:?}");
        let out = render(&cfg, &r);
        for line in [
            "port=53",
            "no-resolv",
            "addn-hosts=/run/meduza-np/generations/rev-000001/dnsmasq/main/hosts",
            "listen-address=10.42.1.1",
            "strict-order",
        ] {
            assert!(out.conf.lines().any(|l| l == line), "missing {line}\n{}", out.conf);
        }
        assert!(!out.conf.contains("dhcp-range"), "never DHCP");
        assert!(!out.conf.lines().any(|l| l.starts_with("server=")), "upstreams only come from the servers-file");
        assert_eq!(out.hosts, "# dns.hosts from the node configuration\n10.42.10.5 nas.lan\nfd00::5 nas.lan\n");
        let plans = plan(&cfg, &r).unwrap();
        assert!(plans[0].validate().is_empty(), "{:?}", plans[0].validate().errors());
        assert_eq!(plans[0].processes[0].argv.last().unwrap(), "${GENERATION_ROOT}/dnsmasq/main/frontend.json");
        let file = plans[0].files.iter().find(|f| f.path == SPEC_FILE).unwrap();
        let spec: FrontendSpec = serde_json::from_str(&file.content).unwrap();
        assert_eq!(spec.fallback, vec!["server=119.29.29.29#53", "server=1.0.0.1#53"]);
        assert_eq!(spec.backends.len(), 2);
    }

    #[test]
    fn backend_set_changes_reload_listener_changes_restart() {
        let mut r = req(json!({"listenAddresses": ["10.42.1.1"], "port": 53}), &["mosdns.dns", "mihomo.dns"]);
        r.config.services.insert("mosdns".into(), json!({}));
        let generated = |r: &PluginRequest| {
            let p = plan(&load(r).unwrap().unwrap(), r).unwrap().remove(0);
            (p.processes[0].restart_digest.clone().unwrap(), p.processes[0].argv.clone(), p.files[2].content.clone())
        };
        let (d0, argv0, spec0) = generated(&r);
        r.config.services.insert("mihomo".into(), json!({"dns": {"enabled": true}}));
        r.config.services.get_mut("dnsmasq").unwrap()["fallbackServers"] = json!(["1.0.0.1"]);
        let (d1, argv1, spec1) = generated(&r);
        assert_eq!((d1, argv1), (d0.clone(), argv0), "backend set and fallback are reloaded (SIGHUP), not restarted");
        assert_ne!(spec0, spec1);
        r.config.services.get_mut("mosdns").unwrap()["port"] = json!(1253);
        let moved = generated(&r);
        assert_eq!(moved.0, d0, "moving a backend never restarts the client-facing DNS");
        assert!(moved.2.contains("1253"));
        r.config.services.get_mut("dnsmasq").unwrap()["port"] = json!(5353);
        assert_ne!(generated(&r).0, d0, "front-end listener change restarts");
    }

    #[test]
    fn loops_rejected_and_uninstalled_backends_are_not_required() {
        let mut r = req(
            json!({"listenAddresses": ["127.0.0.1"], "port": 1153, "fallbackServers": ["127.0.0.1#1153"],
            "backends": [{"id": "m", "kind": "mosdns", "port": 1153}], "mdnsForward": true}),
            &["mosdns.dns"],
        );
        r.config.services.insert("mosdns".into(), json!({}));
        let text = format!("{:?}", validate(&load(&r).unwrap().unwrap(), &r));
        for needle in ["points at this front end", "equals the front-end listener", "requires meduza-np-avahi"] {
            assert!(text.contains(needle), "missing {needle}: {text}");
        }
        r.context.capabilities.clear();
        assert!(load(&r).unwrap().unwrap().backends.is_empty(), "an uninstalled backend never prevents standalone fallback DNS");
    }

    #[test]
    fn discovery_follows_services_ports_and_switches_ignoring_legacy_lists() {
        let mut r = req(
            json!({"listenAddresses": ["10.42.10.12"], "backends": [
            {"id":"stale", "kind":"mosdns", "address":"192.0.2.1", "port":9}]}),
            &["mosdns.dns", "mihomo.dns", "mihomo-openwrt.dns"],
        );
        assert!(load(&r).unwrap().unwrap().backends.is_empty(), "legacy entries do not create upstreams");
        r.config.services.get_mut("dnsmasq").unwrap()["fallbackServers"] = json!([]);
        assert_eq!(load(&r).unwrap().unwrap().fallback_servers, ["119.29.29.29", "1.0.0.1"], "empty legacy fallback selects the defaults");
        r.config.services.insert("mosdns".into(), json!({"listen":"0.0.0.0", "port":1253}));
        r.config.services.insert("mihomo".into(), json!({"dns":{"enabled":true,"listen":"[::]:2053"}}));
        let cfg = load(&r).unwrap().unwrap();
        assert_eq!(cfg.backends.iter().map(|b| b.addr().to_string()).collect::<Vec<_>>(), ["127.0.0.1:1253", "[::1]:2053"]);
        r.config.services.get_mut("mihomo").unwrap()["dns"]["enabled"] = json!(false);
        assert_eq!(load(&r).unwrap().unwrap().backends.len(), 1);
        r.config.services.get_mut("mosdns").unwrap()["enabled"] = json!(false);
        assert!(load(&r).unwrap().unwrap().backends.is_empty());
        r.config.services.insert("mihomo-openwrt".into(), json!({"dnsIntegration":true}));
        assert_eq!(load(&r).unwrap().unwrap().backends[0].kind, BackendKind::MihomoOpenwrt);
        r.config.services.get_mut("mihomo-openwrt").unwrap()["dnsIntegration"] = json!(false);
        assert!(load(&r).unwrap().unwrap().backends.is_empty());
    }

    #[test]
    fn port_owners_only_collide_on_our_addresses() {
        // Rows as a little-endian host prints them (the lab: systemd-resolved, aardvark-dns, a dual-stack wildcard).
        let a = |s: &str| proc_net_addr(s).unwrap();
        if cfg!(target_endian = "little") {
            assert_eq!(a("3500007F:0035"), "127.0.0.53:53".parse().unwrap());
            assert_eq!(a("0100590A:0035"), "10.89.0.1:53".parse().unwrap());
            assert_eq!(a("00000000000000000000000000000000:0035"), "[::]:53".parse().unwrap());
            assert_eq!(a("0000000000000000FFFF00000C003E0A:0035"), "10.62.0.12:53".parse().unwrap(), "v4-mapped");
            assert_eq!(a("000080FE00000000FF005450B6AD1DFE:0035"), "[fe80::5054:ff:fe1d:adb6]:53".parse().unwrap());
        }
        let ours: Vec<IpAddr> = vec!["10.62.0.12".parse().unwrap()];
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert!(!bind_conflicts(ip("127.0.0.53"), &ours));
        assert!(!bind_conflicts(ip("10.89.0.1"), &ours));
        assert!(bind_conflicts(ip("10.62.0.12"), &ours));
        assert!(bind_conflicts(ip("0.0.0.0"), &ours));
        assert!(bind_conflicts(ip("::"), &ours), "dual-stack wildcard");
        assert!(!bind_conflicts(ip("0.0.0.0"), &[ip("fd00::1")]));
        assert!(bind_conflicts(ip("127.0.0.53"), &[ip("0.0.0.0")]), "a wildcard listener collides with everything of its family");
        assert!(bind_conflicts(ip("fd00::1"), &[ip("::")]));
    }

    #[test]
    fn config_schema_is_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/config.json");
        meduza_np_protocol::schema::check_file::<DnsmasqConfig>(&path).unwrap();
    }
}
