//! Avahi / mDNS plugin (AGENT.md §9.11).
//!
//! - `managed` mode: planner supervises `avahi-daemon -f <generated conf>` (interface allow/deny, IPv4/IPv6,
//!   publishing, reflector). Refused when a system avahi-daemon already runs — it is never killed or raced.
//! - `system` mode: the existing system daemon stays in charge; this package only adds the bridge and status.
//! - Cross-interface reflection is off by default and needs an explicit interface allow-list.
//! - The optional bridge (`bridge` mode of this binary) answers `.local` A/AAAA on a loopback port via Avahi's
//!   D-Bus `ResolveHostName`; dnsmasq forwards only `/local/` to it. It is the one extra resident process of
//!   this package (bounded: ~1 MiB RSS, 16 lookups in flight, 512 cache entries).

pub mod bridge;
pub mod dbus;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use meduza_np_plugin_sdk::{PluginError, PluginRequest};
use meduza_np_protocol::plan::{GenerationPlan, PlannedFile, ProbeSpec, ProcessSpec, RestartPolicy};
use meduza_np_protocol::{ValidationError, Validator, PLUGIN_PROTOCOL_VERSION};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SLOT: &str = "avahi.daemon";
pub const INSTANCE: &str = "avahi/main";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(title = "Avahi / mDNS")]
pub struct AvahiConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub mode: Mode,
    /// mDNS host name; unset uses the system host name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_name: Option<String>,
    /// Interfaces mDNS runs on (empty = all except denied).
    #[serde(default)]
    pub allow_interfaces: Vec<String>,
    #[serde(default)]
    pub deny_interfaces: Vec<String>,
    #[serde(default = "default_true")]
    pub use_ipv4: bool,
    #[serde(default)]
    pub use_ipv6: bool,
    #[serde(default)]
    pub publish: Publish,
    #[serde(default)]
    pub reflector: Reflector,
    #[serde(default)]
    pub bridge: Bridge,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// planner runs avahi-daemon with the generated configuration.
    #[default]
    Managed,
    /// Use the already running system avahi-daemon (bridge and status only).
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Publish {
    /// Announce this node's addresses (`<host>.local`).
    #[serde(default = "default_true")]
    pub addresses: bool,
    #[serde(default)]
    pub workstation: bool,
    /// Allow local programs to publish services through Avahi (controlled service discovery).
    #[serde(default)]
    pub services: bool,
}

impl Default for Publish {
    fn default() -> Self {
        Self { addresses: true, workstation: false, services: false }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Reflector {
    /// Reflect mDNS between the allowed interfaces. Off by default: it exposes every service across networks.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub reflect_ipv: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Bridge {
    #[serde(default)]
    pub enabled: bool,
    /// Loopback UDP port dnsmasq forwards `.local` to.
    #[serde(default = "default_bridge_port")]
    pub port: u16,
    #[serde(default = "default_inflight")]
    pub max_inflight: u16,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u32,
    #[serde(default = "default_cache")]
    pub cache_seconds: u32,
}

impl Default for Bridge {
    fn default() -> Self {
        Self {
            enabled: false,
            port: default_bridge_port(),
            max_inflight: default_inflight(),
            timeout_ms: default_timeout(),
            cache_seconds: default_cache(),
        }
    }
}

fn default_bridge_port() -> u16 {
    5354
}
fn default_inflight() -> u16 {
    16
}
fn default_timeout() -> u32 {
    3000
}
fn default_cache() -> u32 {
    30
}

pub fn load(req: &PluginRequest) -> Result<Option<AvahiConfig>, PluginError> {
    req.service::<AvahiConfig>("avahi")
}

/// A system avahi-daemon not started by planner (its pid file points at a live process without our config).
/// A running avahi-daemon planner does not supervise. avahi-daemon rewrites its process title
/// ("avahi-daemon: running [host]"), so the command line soon loses the generation path; ownership is proven by a
/// supervisor record (D32) in `run_dir` naming the pidfile's pid with the same kernel start time and boot (D50).
pub fn foreign_daemon(run_dir: &str) -> Option<String> {
    for pidfile in ["/run/avahi-daemon/pid", "/var/run/avahi-daemon/pid"] {
        let Some(pid) = std::fs::read_to_string(pidfile).ok().and_then(|t| t.trim().parse::<u32>().ok()) else { continue };
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else { continue };
        let text = String::from_utf8_lossy(&cmdline).replace('\0', " ");
        if text.contains("/meduza-np/") {
            continue;
        }
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default();
        if start_ticks(pid).is_some_and(|t| supervised(Path::new(run_dir), pid, t, boot.trim())) {
            continue;
        }
        return Some(format!("pid {pid} ({})", text.trim()));
    }
    None
}

/// `/proc/<pid>/stat` field 22 (start time in clock ticks since boot).
fn start_ticks(pid: u32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm may contain spaces and parentheses; the fields after the last ')' are fixed (f[19] = field 22).
    text[text.rfind(')')? + 1..].split_whitespace().nth(19)?.parse().ok()
}

/// A supervisor record in `<run_dir>/supervisor` names exactly this process (pid, start time, boot).
fn supervised(run_dir: &Path, pid: u32, start_ticks: u64, boot_id: &str) -> bool {
    let Ok(dir) = std::fs::read_dir(run_dir.join("supervisor")) else { return false };
    dir.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "json")).any(|e| {
        let Some(rec) = std::fs::read(e.path()).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { return false };
        rec["pid"].as_u64() == Some(u64::from(pid))
            && rec["startTicks"].as_u64() == Some(start_ticks)
            && !boot_id.is_empty()
            && rec["bootId"].as_str() == Some(boot_id)
    })
}

fn valid_host(h: &str) -> bool {
    !h.is_empty() && h.len() <= 63 && h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') && !h.starts_with('-') && !h.ends_with('-')
}

pub fn validate(cfg: &AvahiConfig, req: &PluginRequest) -> Vec<ValidationError> {
    let mut v = Validator::new();
    if !cfg.enabled {
        return Vec::new();
    }
    if let Some(h) = &cfg.host_name {
        v.check(valid_host(h), "/hostName", "a single DNS label (letters, digits, '-')");
    }
    let allow: BTreeSet<&String> = cfg.allow_interfaces.iter().collect();
    for (i, d) in cfg.allow_interfaces.iter().enumerate() {
        v.check(meduza_np_protocol::ids::validate_ifname(d).is_ok(), format!("/allowInterfaces/{i}"), "invalid interface");
    }
    for (i, d) in cfg.deny_interfaces.iter().enumerate() {
        v.check(meduza_np_protocol::ids::validate_ifname(d).is_ok(), format!("/denyInterfaces/{i}"), "invalid interface");
        v.check(!allow.contains(d), format!("/denyInterfaces/{i}"), "interface is both allowed and denied");
    }
    v.check(cfg.use_ipv4 || cfg.use_ipv6, "/useIpv4", "enable IPv4 and/or IPv6");
    if cfg.reflector.enabled {
        v.check(cfg.mode == Mode::Managed, "/reflector/enabled", "the reflector is part of the daemon configuration (managed mode only)");
        v.check(cfg.allow_interfaces.len() >= 2, "/allowInterfaces", "reflection needs an explicit list of at least two interfaces");
    }
    if cfg.bridge.enabled {
        let b = &cfg.bridge;
        v.check(b.port >= 1024, "/bridge/port", "use an unprivileged loopback port (>= 1024)");
        v.check((1..=256).contains(&b.max_inflight), "/bridge/maxInflight", "1..256");
        v.check((100..=10_000).contains(&b.timeout_ms), "/bridge/timeoutMs", "100..10000 ms");
        v.check(b.cache_seconds <= 3600, "/bridge/cacheSeconds", "at most 3600");
    }
    if cfg!(target_os = "linux") {
        v.check(dbus::system_bus_path().is_some(), "/mode", "the system D-Bus is not running (Avahi and the bridge need it)");
        if cfg.mode == Mode::Managed {
            if let Some(owner) = foreign_daemon(&req.context.run_dir) {
                v.error(
                    "/mode",
                    format!(
                        "a system avahi-daemon is already running ({owner}); use mode \"system\" instead of starting a competing daemon"
                    ),
                );
            }
        }
    }
    v.finish().err().unwrap_or_default()
}

/// System mode without a running system daemon is a runtime state, not an invalid configuration (D51): at boot
/// planner may start before avahi-daemon, and an operator may restart it. Rejecting the revision then dropped the
/// last good configuration and the bridge, and nothing retried it once the daemon was back. The bridge keeps running
/// (SERVFAIL until the daemon answers) and the probe reports the missing dependency.
pub fn warnings(cfg: &AvahiConfig, req: &PluginRequest) -> Vec<String> {
    let mut w = Vec::new();
    if cfg.enabled && cfg.mode == Mode::System && cfg!(target_os = "linux") && foreign_daemon(&req.context.run_dir).is_none() {
        w.push(format!("{NO_SYSTEM_DAEMON}; mDNS works once the system service runs (or use mode \"managed\")"));
    }
    w
}

/// Probe/warning text for system mode without a system avahi-daemon.
pub const NO_SYSTEM_DAEMON: &str = "no system avahi-daemon is running (dependency missing)";

fn yn(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

/// `avahi-daemon.conf` for managed mode. Only keys avahi-daemon knows: it refuses the whole file on an unknown one.
pub fn render_conf(cfg: &AvahiConfig) -> String {
    let mut s = String::from("# Meduza NP avahi-daemon configuration — generated, do not edit\n[server]\n");
    if let Some(h) = &cfg.host_name {
        s.push_str(&format!("host-name={h}\n"));
    }
    s.push_str(&format!("use-ipv4={}\nuse-ipv6={}\n", yn(cfg.use_ipv4), yn(cfg.use_ipv6)));
    if !cfg.allow_interfaces.is_empty() {
        s.push_str(&format!("allow-interfaces={}\n", cfg.allow_interfaces.join(",")));
    }
    if !cfg.deny_interfaces.is_empty() {
        s.push_str(&format!("deny-interfaces={}\n", cfg.deny_interfaces.join(",")));
    }
    s.push_str("enable-dbus=yes\nallow-point-to-point=no\nratelimit-interval-usec=1000000\nratelimit-burst=1000\n");
    s.push_str("\n[wide-area]\nenable-wide-area=no\n");
    let p = &cfg.publish;
    let any = p.addresses || p.workstation || p.services;
    s.push_str(&format!(
        "\n[publish]\ndisable-publishing={}\ndisable-user-service-publishing={}\npublish-addresses={}\npublish-hinfo=no\npublish-workstation={}\npublish-domain=no\npublish-aaaa-on-ipv4=no\npublish-a-on-ipv6=no\n",
        yn(!any),
        yn(!p.services),
        yn(p.addresses),
        yn(p.workstation)
    ));
    s.push_str(&format!(
        "\n[reflector]\nenable-reflector={}\nreflect-ipv={}\n",
        yn(cfg.reflector.enabled),
        yn(cfg.reflector.enabled && cfg.reflector.reflect_ipv)
    ));
    // No rlimit-fsize: avahi-daemon applies the limits before writing its pid file, and 0 kills it with SIGXFSZ.
    s.push_str("\n[rlimits]\nrlimit-core=0\nrlimit-data=8388608\nrlimit-nofile=768\nrlimit-stack=8388608\nrlimit-nproc=3\n");
    s
}

/// avahi-daemon arguments. `chroot` false adds `--no-chroot` (a command-line switch only; the config file has no such
/// key): the chroot needs CAP_SYS_CHROOT, which a planner container created without the avahi component lacks, and
/// avahi-daemon exits at start without it.
pub fn daemon_argv(chroot: bool) -> Vec<String> {
    let mut argv = vec!["-f".to_string(), "${GENERATION_ROOT}/avahi/main/avahi-daemon.conf".to_string()];
    if !chroot {
        argv.push("--no-chroot".into());
    }
    argv
}

pub fn plan(cfg: &AvahiConfig, req: &PluginRequest) -> Result<Vec<GenerationPlan>, PluginError> {
    if !cfg.enabled {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    let mut processes = Vec::new();
    if cfg.mode == Mode::Managed {
        req.provider(SLOT)?;
        files.push(PlannedFile {
            path: "avahi/main/avahi-daemon.conf".into(),
            mode: "0644".into(),
            content: render_conf(cfg),
            sensitive: false,
        });
        processes.push(ProcessSpec {
            id: INSTANCE.into(),
            provider_slot: Some(SLOT.into()),
            program: None,
            // Unknown (no /proc) keeps avahi's default chroot; only a known missing capability turns it off.
            argv: daemon_argv(meduza_np_plugin_sdk::has_effective_capability(meduza_np_plugin_sdk::CAP_SYS_CHROOT) != Some(false)),
            env: Default::default(),
            restart: RestartPolicy::Always,
            startup: Default::default(),
            stop_timeout_seconds: 10,
            reload_signal: None,
            restart_digest: None,
            user: None,
            bound_to: None,
        });
    }
    if cfg.bridge.enabled {
        let b = &cfg.bridge;
        let mut argv = vec![
            "bridge".to_string(),
            "--listen".into(),
            format!("127.0.0.1:{}", b.port),
            "--max-inflight".into(),
            b.max_inflight.to_string(),
            "--timeout-ms".into(),
            b.timeout_ms.to_string(),
            "--cache-seconds".into(),
            b.cache_seconds.to_string(),
        ];
        if !cfg.allow_interfaces.is_empty() {
            argv.extend(["--interfaces".to_string(), cfg.allow_interfaces.join(",")]);
        }
        // D50: a reflecting avahi-daemon never answers ResolveHostName for its own name; the bridge answers it
        // from the local addresses of the families the daemon announces.
        if cfg.mode == Mode::Managed && cfg.reflector.enabled && cfg.publish.addresses {
            let families: Vec<&str> = [(cfg.use_ipv4, "v4"), (cfg.use_ipv6, "v6")].into_iter().filter(|f| f.0).map(|f| f.1).collect();
            argv.extend(["--self-answer".to_string(), families.join(",")]);
        }
        processes.push(ProcessSpec {
            id: "avahi/bridge".into(),
            provider_slot: None,
            program: Some(format!("{}/plugins/avahi", req.context.libexec_dir.trim_end_matches('/'))),
            argv,
            env: Default::default(),
            restart: RestartPolicy::Always,
            startup: Default::default(),
            stop_timeout_seconds: 5,
            reload_signal: None,
            restart_digest: None,
            user: None,
            bound_to: None,
        });
    }
    // System mode without the bridge runs nothing: the plan only carries the status probe.
    let interval = if processes.is_empty() { 60 } else { 30 };
    Ok(vec![GenerationPlan {
        protocol_version: PLUGIN_PROTOCOL_VERSION,
        request_id: req.request_id.clone(),
        instance: INSTANCE.into(),
        files,
        processes,
        resources: Vec::new(),
        depends_on: Vec::new(),
        probes: vec![ProbeSpec::Plugin { interval_seconds: interval, params: serde_json::Value::Null }],
        checks: Vec::new(),
        reload: Vec::new(),
    }])
}

/// Interfaces mDNS is active on: the allow-list (or every non-loopback interface) minus denied, as present now.
pub fn active_interfaces(cfg: &AvahiConfig, sys_net: &Path) -> Vec<String> {
    let present: BTreeSet<String> = std::fs::read_dir(sys_net)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    let base: Vec<String> = if cfg.allow_interfaces.is_empty() {
        present.iter().filter(|n| *n != "lo").cloned().collect()
    } else {
        cfg.allow_interfaces.iter().filter(|n| present.contains(*n)).cloned().collect()
    };
    base.into_iter().filter(|n| !cfg.deny_interfaces.contains(n)).collect()
}

/// SIGTERM/SIGINT set `stop` (unix); elsewhere the process just runs until killed.
pub fn install_term(stop: Arc<AtomicBool>) {
    #[cfg(unix)]
    {
        use std::sync::atomic::Ordering;
        use std::sync::OnceLock;
        static STOP: OnceLock<Arc<AtomicBool>> = OnceLock::new();
        extern "C" fn on_term(_: libc::c_int) {
            if let Some(s) = STOP.get() {
                s.store(true, Ordering::SeqCst);
            }
        }
        let _ = STOP.set(stop);
        // SAFETY: the handler only stores to an atomic.
        unsafe {
            libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
            libc::signal(libc::SIGINT, on_term as *const () as libc::sighandler_t);
        }
    }
    #[cfg(not(unix))]
    let _ = stop;
}

#[cfg(test)]
mod tests {
    use super::*;
    use meduza_np_plugin_sdk::test_request;
    use serde_json::json;

    fn req(a: Value) -> PluginRequest {
        let mut r = test_request("plan", json!({"services": {"avahi": a}}));
        r.providers.insert(
            SLOT.into(),
            meduza_np_plugin_sdk::ResolvedProvider {
                path: "/usr/sbin/avahi-daemon".into(),
                source: "system".into(),
                version: Some("0.8".into()),
            },
        );
        r
    }

    fn host_independent(errs: Vec<ValidationError>) -> Vec<ValidationError> {
        errs.into_iter().filter(|e| e.path != "/mode").collect()
    }

    /// avahi-daemon (0.8, `load_config_file`) exits on any key it does not know, so every rendered key must be one
    /// of these (per section).
    #[test]
    fn rendered_keys_are_known_to_avahi_daemon() {
        let known: &[(&str, &[&str])] = &[
            (
                "server",
                &[
                    "host-name",
                    "domain-name",
                    "browse-domains",
                    "use-ipv4",
                    "use-ipv6",
                    "check-response-ttl",
                    "allow-interfaces",
                    "deny-interfaces",
                    "enable-dbus",
                    "allow-point-to-point",
                    "cache-entries-max",
                    "ratelimit-interval-usec",
                    "ratelimit-burst",
                    "disallow-other-stacks",
                ],
            ),
            ("wide-area", &["enable-wide-area"]),
            (
                "publish",
                &[
                    "publish-addresses",
                    "publish-hinfo",
                    "publish-workstation",
                    "publish-domain",
                    "publish-dns-servers",
                    "publish-resolv-conf-dns-servers",
                    "add-service-cookie",
                    "disable-publishing",
                    "disable-user-service-publishing",
                    "publish-aaaa-on-ipv4",
                    "publish-a-on-ipv6",
                ],
            ),
            ("reflector", &["enable-reflector", "reflect-ipv", "reflect-filters"]),
            ("rlimits", &["rlimit-as", "rlimit-core", "rlimit-data", "rlimit-fsize", "rlimit-nofile", "rlimit-stack", "rlimit-nproc"]),
        ];
        let r = req(json!({"hostName": "n1", "allowInterfaces": ["br-lan"], "denyInterfaces": ["wan"], "useIpv6": true,
            "publish": {"workstation": true, "services": true}, "reflector": {"enabled": true, "reflectIpv": true}}));
        let conf = render_conf(&load(&r).unwrap().unwrap());
        let mut section = "";
        for line in conf.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = s;
                continue;
            }
            let key = line.split('=').next().unwrap();
            let keys = known.iter().find(|(s, _)| *s == section).unwrap_or_else(|| panic!("unknown section [{section}]")).1;
            assert!(keys.contains(&key), "[{section}] {key} is not an avahi-daemon key\n{conf}");
        }
    }

    #[test]
    fn managed_conf_defaults_are_conservative() {
        let r = req(json!({"allowInterfaces": ["br-lan"], "bridge": {"enabled": true}}));
        let cfg = load(&r).unwrap().unwrap();
        assert!(host_independent(validate(&cfg, &r)).is_empty());
        let conf = render_conf(&cfg);
        assert!(!daemon_argv(true).contains(&"--no-chroot".to_string()), "avahi's default chroot stays on");
        assert_eq!(daemon_argv(false).last().map(String::as_str), Some("--no-chroot"));
        assert!(!conf.contains("rlimit-fsize"), "SIGXFSZ on the pid file\n{conf}");
        for line in [
            "allow-interfaces=br-lan",
            "enable-reflector=no",
            "use-ipv6=no",
            "enable-wide-area=no",
            "disable-user-service-publishing=yes",
            "publish-addresses=yes",
        ] {
            assert!(conf.contains(line), "{line}\n{conf}");
        }
        let plans = plan(&cfg, &r).unwrap();
        assert!(plans[0].validate().is_empty(), "{:?}", plans[0].validate().errors());
        assert_eq!(plans[0].processes.len(), 2);
        assert_eq!(plans[0].processes[0].provider_slot.as_deref(), Some(SLOT));
        let bridge = plans[0].processes[1].argv.join(" ");
        assert!(bridge.starts_with("bridge --listen 127.0.0.1:5354"), "{bridge}");
        assert!(bridge.ends_with("--interfaces br-lan"));
    }

    /// D50: only a reflecting managed daemon that publishes its addresses gets the bridge's own-name answer.
    #[test]
    fn reflector_bridge_answers_own_name() {
        let base =
            json!({"allowInterfaces": ["eth1", "eth0"], "useIpv6": true, "bridge": {"enabled": true}, "reflector": {"enabled": true}});
        let argv = |a: Value| {
            let r = req(a);
            plan(&load(&r).unwrap().unwrap(), &r).unwrap()[0].processes.iter().find(|p| p.id == "avahi/bridge").unwrap().argv.join(" ")
        };
        assert!(argv(base.clone()).ends_with("--interfaces eth1,eth0 --self-answer v4,v6"), "{}", argv(base.clone()));
        let mut v4 = base.clone();
        v4["useIpv6"] = json!(false);
        assert!(argv(v4).ends_with("--self-answer v4"));
        let mut quiet = base.clone();
        quiet["publish"] = json!({"addresses": false});
        assert!(!argv(quiet).contains("--self-answer"));
        let mut plain = base;
        plain["reflector"] = json!({"enabled": false});
        assert!(!argv(plain).contains("--self-answer"));
    }

    #[test]
    fn system_mode_runs_only_the_bridge() {
        let r = req(json!({"mode": "system", "bridge": {"enabled": true, "port": 5399}}));
        let cfg = load(&r).unwrap().unwrap();
        let plans = plan(&cfg, &r).unwrap();
        assert!(plans[0].files.is_empty());
        assert_eq!(plans[0].processes.len(), 1);
        assert_eq!(plans[0].processes[0].id, "avahi/bridge");
        // a missing system daemon is a warning (runtime state), never a validation error (D51)
        assert!(validate(&cfg, &r).iter().all(|e| !e.message.contains("avahi-daemon")));
        let r = req(json!({"mode": "system"}));
        assert!(plan(&load(&r).unwrap().unwrap(), &r).unwrap()[0].processes.is_empty());
    }

    #[test]
    fn validation() {
        let r = req(
            json!({"hostName": "bad name", "allowInterfaces": ["br-lan"], "denyInterfaces": ["br-lan"], "reflector": {"enabled": true},
                           "useIpv4": false, "bridge": {"enabled": true, "port": 53, "timeoutMs": 1}}),
        );
        let text = format!("{:?}", validate(&load(&r).unwrap().unwrap(), &r));
        for needle in
            ["single DNS label", "both allowed and denied", "at least two interfaces", "IPv4 and/or IPv6", "unprivileged", "100..10000"]
        {
            assert!(text.contains(needle), "missing {needle}: {text}");
        }
    }

    #[test]
    fn active_interface_view() {
        let dir = std::env::temp_dir().join(format!("mnp-avahi-net-{}", std::process::id()));
        for n in ["lo", "br-lan", "eth1", "wg-a"] {
            std::fs::create_dir_all(dir.join(n)).unwrap();
        }
        let r = req(json!({"denyInterfaces": ["wg-a"]}));
        let cfg = load(&r).unwrap().unwrap();
        assert_eq!(active_interfaces(&cfg, &dir), vec!["br-lan", "eth1"]);
        let r = req(json!({"allowInterfaces": ["br-lan", "missing0"]}));
        assert_eq!(active_interfaces(&load(&r).unwrap().unwrap(), &dir), vec!["br-lan"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D50: the daemon planner supervises is recognised by its supervisor record, not by its (rewritten) title.
    #[test]
    fn supervised_daemon_is_matched_by_record() {
        let run = std::env::temp_dir().join(format!("mnp-avahi-sup-{}", std::process::id()));
        std::fs::create_dir_all(run.join("supervisor")).unwrap();
        assert!(!supervised(&run, 95, 20458856, "boot-1"));
        std::fs::write(
            run.join("supervisor/0098.json"),
            json!({"id": "avahi/main", "pid": 95, "startTicks": 20458856, "bootId": "boot-1", "program": "/usr/sbin/avahi-daemon"})
                .to_string(),
        )
        .unwrap();
        std::fs::write(run.join("supervisor/0098.log"), "not a record").unwrap();
        assert!(supervised(&run, 95, 20458856, "boot-1"));
        // A reused pid (other start time), another boot or an unknown boot never matches.
        assert!(!supervised(&run, 95, 20458857, "boot-1"));
        assert!(!supervised(&run, 95, 20458856, "boot-2"));
        assert!(!supervised(&run, 95, 20458856, ""));
        assert!(!supervised(&run, 96, 20458856, "boot-1"));
        let _ = std::fs::remove_dir_all(&run);
    }

    #[test]
    fn config_schema_is_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/config.json");
        meduza_np_protocol::schema::check_file::<AvahiConfig>(&path).unwrap();
    }
}
