//! Front-end wrapper: owns dnsmasq's `servers-file`. Starts dnsmasq on the fallback servers immediately, then
//! adds each backend only after real queries succeed (rise 2) and removes it after failures (fall 2); with no
//! healthy backend the fallback is restored. Changes are applied with SIGHUP (no restart, no client outage).
//! The spec lives in the generation; planner's SIGHUP makes the wrapper re-read it, so a changed backend set or
//! fallback never restarts dnsmasq either.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::{server_line, Backend, BackendKind, FrontendSpec};

pub const RISE: u32 = 2;
pub const FALL: u32 = 2;

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendState {
    pub healthy: bool,
    #[serde(skip)]
    good: u32,
    #[serde(skip)]
    bad: u32,
    pub last_error: Option<String>,
    pub since: Option<String>,
    /// Actual query endpoint; OpenClash may change it without a planner revision.
    pub address: Option<SocketAddr>,
}

impl BackendState {
    fn set_address(&mut self, address: Option<SocketAddr>) {
        if self.address != address {
            *self = Self { address, ..Self::default() };
        }
    }
    /// Returns true if the healthy flag changed.
    pub fn observe(&mut self, result: Result<(), String>) -> bool {
        match result {
            Ok(()) => {
                self.good += 1;
                self.bad = 0;
                self.last_error = None;
                if !self.healthy && self.good >= RISE {
                    self.healthy = true;
                    self.since = Some(meduza_np_plugin_sdk::now_rfc3339());
                    return true;
                }
            }
            Err(e) => {
                self.bad += 1;
                self.good = 0;
                self.last_error = Some(e);
                if self.healthy && self.bad >= FALL {
                    self.healthy = false;
                    self.since = Some(meduza_np_plugin_sdk::now_rfc3339());
                    return true;
                }
            }
        }
        false
    }
}

/// Upstream lines for the current health: healthy backends by priority, else the fallback.
pub fn upstreams(spec: &FrontendSpec, states: &BTreeMap<String, BackendState>) -> (Vec<String>, bool) {
    let mut healthy: Vec<&Backend> =
        spec.backends.iter().filter(|b| states.get(b.id.as_str()).map(|s| s.healthy).unwrap_or(false)).collect();
    healthy.sort_by_key(|b| (b.priority, b.id.clone()));
    if healthy.is_empty() {
        (spec.fallback.clone(), true)
    } else {
        (healthy.iter().map(|b| server_line(states[b.id.as_str()].address.unwrap_or_else(|| b.addr()))).collect(), false)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StateFile<'a> {
    fallback_active: bool,
    upstreams: &'a [String],
    backends: &'a BTreeMap<String, BackendState>,
    frontend_pid: Option<u32>,
    updated_at: String,
}

fn write_atomic(path: &str, content: &str) {
    let tmp = format!("{path}.tmp");
    if std::fs::write(&tmp, content).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(unix)]
mod sig {
    use std::sync::atomic::{AtomicBool, Ordering};
    pub static TERM: AtomicBool = AtomicBool::new(false);
    pub static HUP: AtomicBool = AtomicBool::new(false);
    extern "C" fn on_term(_: libc::c_int) {
        TERM.store(true, Ordering::SeqCst);
    }
    extern "C" fn on_hup(_: libc::c_int) {
        HUP.store(true, Ordering::SeqCst);
    }
    pub fn install() {
        // SAFETY: handlers only store to atomics (async-signal-safe).
        unsafe {
            libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
            libc::signal(libc::SIGINT, on_term as *const () as libc::sighandler_t);
            libc::signal(libc::SIGHUP, on_hup as *const () as libc::sighandler_t);
        }
    }
    pub fn take_hup() -> bool {
        HUP.swap(false, Ordering::SeqCst)
    }
    pub fn term() -> bool {
        TERM.load(Ordering::SeqCst)
    }
    pub fn hup(pid: u32) {
        // SAFETY: signalling our own child.
        unsafe { libc::kill(pid as i32, libc::SIGHUP) };
    }
}

#[cfg(not(unix))]
mod sig {
    pub fn install() {}
    pub fn take_hup() -> bool {
        false
    }
    pub fn term() -> bool {
        false
    }
    pub fn hup(_pid: u32) {}
}

/// Backend health carried over a spec reload: a backend keeps its state while its id and address stay the same
/// (it stays listed, no re-probing); new or moved backends start unprobed and need RISE good probes.
pub fn carry_states(old: &FrontendSpec, states: &BTreeMap<String, BackendState>, new: &FrontendSpec) -> BTreeMap<String, BackendState> {
    new.backends
        .iter()
        .map(|b| {
            let same =
                old.probe_name == new.probe_name && old.backends.iter().any(|o| o.id == b.id && o.kind == b.kind && o.addr() == b.addr());
            let st = if same { states.get(b.id.as_str()).cloned().unwrap_or_default() } else { BackendState::default() };
            (b.id.to_string(), st)
        })
        .collect()
}

fn backend_address(b: &Backend, spec: &FrontendSpec, run_dir: &str) -> Result<SocketAddr, String> {
    let address = if b.kind == BackendKind::MihomoOpenwrt {
        let state = std::fs::read_to_string(format!("{}/mihomo-openwrt.state", run_dir.trim_end_matches('/')))
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
        let port = state
            .as_ref()
            .and_then(|s| s.get("dnsPort"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|p| u16::try_from(p).ok())
            .filter(|p| *p != 0)
            .ok_or("waiting for the OpenClash adapter's DNS listener")?;
        SocketAddr::new(b.address, port)
    } else {
        b.addr()
    };
    if address.port() == 0 {
        return Err("backend has no DNS listener".into());
    }
    if spec.listen.iter().any(|l| l.port() == address.port() && crate::bind_conflicts(l.ip(), &[address.ip()])) {
        return Err("backend DNS listener points back at the front end (loop)".into());
    }
    Ok(address)
}

/// Real DNS responses, rather than an API/process status, decide whether a discovered listener is ready.
pub fn probe_backends(spec: &FrontendSpec, states: &mut BTreeMap<String, BackendState>, run_dir: &str, timeout: Duration) {
    for b in &spec.backends {
        let address = backend_address(b, spec, run_dir);
        let st = states.entry(b.id.to_string()).or_default();
        st.set_address(address.as_ref().ok().copied());
        let result = address.and_then(|a| meduza_np_plugin_sdk::dns::probe(a, &spec.probe_name, timeout));
        if st.observe(result) {
            eprintln!("backend {} is now {}", b.id, if st.healthy { "healthy: added" } else { "unhealthy: removed" });
        }
    }
}

fn read_spec(path: &str) -> Result<FrontendSpec, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("invalid spec {path}: {e}"))
}

pub fn run(args: &[String]) -> ! {
    let get = |k: &str| args.windows(2).find(|w| w[0] == k).map(|w| w[1].clone());
    let (Some(dnsmasq), Some(conf), Some(run_dir), Some(spec_file)) =
        (get("--dnsmasq"), get("--conf"), get("--run-dir"), get("--spec-file"))
    else {
        eprintln!("usage: frontend --dnsmasq BIN --conf FILE --run-dir DIR --spec-file FILE");
        std::process::exit(2)
    };
    let mut spec = read_spec(&spec_file).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2)
    });
    let servers_file = format!("{run_dir}/dnsmasq-main.servers");
    let state_file = format!("{run_dir}/dnsmasq-main.state");
    sig::install();
    // Base DNS first: dnsmasq starts on the fallback servers, before any backend is ready (§9.10).
    let mut current = spec.fallback.clone();
    write_atomic(&servers_file, &(current.join("\n") + "\n"));
    let mut child = match std::process::Command::new(&dnsmasq).args(["-C", &conf, "-k"]).stdin(std::process::Stdio::null()).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot start {dnsmasq}: {e}");
            std::process::exit(1)
        }
    };
    let pid = child.id();
    let mut states: BTreeMap<String, BackendState> = spec.backends.iter().map(|b| (b.id.to_string(), BackendState::default())).collect();
    let mut next_probe = Instant::now();
    let mut fallback_ok: Option<bool> = None;
    loop {
        if sig::term() {
            let _ = child.kill();
            let _ = child.wait();
            // A stopped (disabled) front end leaves nothing in the run dir (dnsmasq keeps its pid file when
            // killed); a restart writes all three again.
            for f in [&servers_file, &state_file, &format!("{run_dir}/dnsmasq-main.pid")] {
                let _ = std::fs::remove_file(f);
            }
            std::process::exit(0);
        }
        if let Ok(Some(status)) = child.try_wait() {
            eprintln!("dnsmasq exited: {status}");
            std::process::exit(status.code().unwrap_or(1));
        }
        if sig::take_hup() {
            // Reload requested by planner: dnsmasq re-reads its hosts, the wrapper its spec (backend set,
            // fallback, probe name). A broken spec keeps the running one.
            sig::hup(pid);
            match read_spec(&spec_file) {
                Ok(new) if new != spec => {
                    eprintln!("spec reloaded: backends {:?}", new.backends.iter().map(|b| b.id.as_str()).collect::<Vec<_>>());
                    states = carry_states(&spec, &states, &new);
                    if new.fallback != spec.fallback {
                        fallback_ok = None;
                    }
                    spec = new;
                    // A disabled/removed/moved backend leaves immediately, before any potentially slow probes.
                    let (lines, _) = upstreams(&spec, &states);
                    if lines != current {
                        write_atomic(&servers_file, &(lines.join("\n") + "\n"));
                        sig::hup(pid);
                        current = lines;
                    }
                    next_probe = Instant::now();
                }
                Ok(_) => {}
                Err(e) => eprintln!("warning: {e}; keeping the running spec"),
            }
        }
        if Instant::now() >= next_probe {
            next_probe = Instant::now() + Duration::from_secs(3);
            if fallback_ok.is_none() {
                // One bounded verification of the fallback path; failure is reported, never blocks.
                let first = spec.fallback.first().and_then(|l| l.strip_prefix("server=")).and_then(crate::parse_server);
                fallback_ok = Some(
                    first.map(|a| meduza_np_plugin_sdk::dns::probe(a, &spec.probe_name, Duration::from_secs(2)).is_ok()).unwrap_or(false),
                );
                if fallback_ok == Some(false) {
                    eprintln!("warning: fallback DNS did not answer the verification query (degraded)");
                }
            }
            probe_backends(&spec, &mut states, &run_dir, Duration::from_millis(1500));
            let (lines, fallback_active) = upstreams(&spec, &states);
            if lines != current {
                write_atomic(&servers_file, &(lines.join("\n") + "\n"));
                sig::hup(pid);
                current = lines;
            }
            let st = StateFile {
                fallback_active,
                upstreams: &current,
                backends: &states,
                frontend_pid: Some(pid),
                updated_at: meduza_np_plugin_sdk::now_rfc3339(),
            };
            let mut json = serde_json::to_value(&st).unwrap_or_default();
            json["fallbackVerified"] = serde_json::Value::from(fallback_ok.unwrap_or(false));
            write_atomic(&state_file, &json.to_string());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Probe result for the front end itself (client-facing listener).
pub fn frontend_answers(listen: &[SocketAddr], name: &str) -> Result<(), String> {
    let first = listen.first().ok_or("no listener")?;
    let target = SocketAddr::new(crate::local_target(first.ip()), first.port());
    meduza_np_plugin_sdk::dns::probe(target, name, Duration::from_secs(2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    struct Resolver {
        address: SocketAddr,
        answer: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Resolver {
        fn new() -> Self {
            let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            socket.set_read_timeout(Some(Duration::from_millis(30))).unwrap();
            let address = socket.local_addr().unwrap();
            let answer = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let (enabled, done) = (answer.clone(), stop.clone());
            let thread = std::thread::spawn(move || {
                let mut packet = [0u8; 512];
                while !done.load(Ordering::SeqCst) {
                    if let Ok((len, peer)) = socket.recv_from(&mut packet) {
                        if enabled.load(Ordering::SeqCst) && len >= 12 {
                            packet[2] = 0x81;
                            packet[3] = 0x80;
                            let _ = socket.send_to(&packet[..len], peer);
                        }
                    }
                }
            });
            Self { address, answer, stop, thread: Some(thread) }
        }
    }

    impl Drop for Resolver {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            self.thread.take().unwrap().join().unwrap();
        }
    }

    #[test]
    fn discovered_backends_follow_real_dns_failure_recovery_and_disable() {
        let (mosdns, mihomo) = (Resolver::new(), Resolver::new());
        let request = meduza_np_plugin_sdk::test_request(
            "plan",
            json!({"services":{
                "dnsmasq":{"listenAddresses":["10.42.10.12"]},
                "mosdns":{"port":mosdns.address.port()},
                "mihomo":{"dns":{"enabled":true,"listen":mihomo.address.to_string()}}
            }}),
        );
        let mut request = request;
        request.context.capabilities = vec!["mosdns.dns".into(), "mihomo.dns".into()];
        let cfg = crate::load(&request).unwrap().unwrap();
        let spec = FrontendSpec {
            fallback: vec!["server=119.29.29.29#53".into()],
            backends: cfg.backends,
            probe_name: "example.test".into(),
            listen: vec!["10.42.10.12:53".parse().unwrap()],
        };
        let mut states = BTreeMap::new();
        let poll = |states: &mut BTreeMap<String, BackendState>| probe_backends(&spec, states, "", Duration::from_millis(60));
        poll(&mut states);
        assert!(upstreams(&spec, &states).1, "fallback until actual DNS queries succeed");
        mosdns.answer.store(true, Ordering::SeqCst);
        mihomo.answer.store(true, Ordering::SeqCst);
        poll(&mut states);
        poll(&mut states);
        assert_eq!(upstreams(&spec, &states).0, [server_line(mosdns.address), server_line(mihomo.address)]);
        mosdns.answer.store(false, Ordering::SeqCst);
        poll(&mut states);
        poll(&mut states);
        assert_eq!(upstreams(&spec, &states).0, [server_line(mihomo.address)], "failed MosDNS removed, healthy Mihomo retained");
        mihomo.answer.store(false, Ordering::SeqCst);
        poll(&mut states);
        poll(&mut states);
        assert!(upstreams(&spec, &states).1, "all failed: fallback restored");
        mosdns.answer.store(true, Ordering::SeqCst);
        poll(&mut states);
        poll(&mut states);
        assert_eq!(upstreams(&spec, &states).0, [server_line(mosdns.address)], "recovered backend automatically added again");
        request.config.services.get_mut("mosdns").unwrap()["enabled"] = json!(false);
        let new = FrontendSpec { backends: crate::load(&request).unwrap().unwrap().backends, ..spec.clone() };
        let carried = carry_states(&spec, &states, &new);
        assert!(upstreams(&new, &carried).1, "disable removes healthy backend immediately without a failed probe");
    }

    #[test]
    fn openclash_listener_is_discovered_live_and_port_changes_require_new_probes() {
        let (first, second) = (Resolver::new(), Resolver::new());
        first.answer.store(true, Ordering::SeqCst);
        second.answer.store(true, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("meduza-dns-discovery-{}-{}", std::process::id(), first.address.port()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("mihomo-openwrt.state");
        let mut spec: FrontendSpec = serde_json::from_value(json!({
            "fallback":["server=119.29.29.29#53"], "probeName":"example.test", "listen":["10.42.10.12:53"],
            "backends":[{"id":"mihomo-openwrt","kind":"mihomo-openwrt","port":0}]
        }))
        .unwrap();
        let mut states = BTreeMap::new();
        let poll = |spec: &FrontendSpec, states: &mut BTreeMap<String, BackendState>| {
            probe_backends(spec, states, dir.to_str().unwrap(), Duration::from_millis(60))
        };
        poll(&spec, &mut states);
        assert!(upstreams(&spec, &states).1, "no assumed OpenClash port before it reports");
        std::fs::write(&path, json!({"dnsPort":first.address.port()}).to_string()).unwrap();
        poll(&spec, &mut states);
        poll(&spec, &mut states);
        assert_eq!(upstreams(&spec, &states).0, [server_line(first.address)]);
        std::fs::write(&path, json!({"dnsPort":second.address.port()}).to_string()).unwrap();
        poll(&spec, &mut states);
        assert!(upstreams(&spec, &states).1, "changed endpoint cannot reuse old readiness");
        poll(&spec, &mut states);
        assert_eq!(upstreams(&spec, &states).0, [server_line(second.address)]);
        spec.listen = vec![second.address];
        poll(&spec, &mut states);
        assert!(upstreams(&spec, &states).1, "a live listener change must not create a front-end loop");
        assert!(states["mihomo-openwrt"].last_error.as_ref().unwrap().contains("loop"));
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn hysteresis_and_selection() {
        let spec: FrontendSpec = serde_json::from_value(json!({
            "fallback": ["server=119.29.29.29#53"],
            "backends": [{"id": "mihomo", "kind": "mihomo", "port": 1053, "priority": 20}, {"id": "mosdns", "kind": "mosdns", "port": 1153, "priority": 10}],
            "probeName": "example.com", "listen": ["10.42.1.1:53"]
        }))
        .unwrap();
        let mut states: BTreeMap<String, BackendState> = BTreeMap::new();
        states.insert("mosdns".into(), BackendState::default());
        states.insert("mihomo".into(), BackendState::default());
        assert_eq!(upstreams(&spec, &states), (vec!["server=119.29.29.29#53".to_string()], true), "fallback until probed");
        let m = states.get_mut("mihomo").unwrap();
        assert!(!m.observe(Ok(())));
        assert!(m.observe(Ok(())), "healthy after RISE");
        assert_eq!(upstreams(&spec, &states).0, vec!["server=127.0.0.1#1053"], "only the healthy backend, fallback removed");
        let s = states.get_mut("mosdns").unwrap();
        s.observe(Ok(()));
        s.observe(Ok(()));
        assert_eq!(upstreams(&spec, &states).0, vec!["server=127.0.0.1#1153", "server=127.0.0.1#1053"], "priority order");
        let m = states.get_mut("mihomo").unwrap();
        m.observe(Err("timeout".into()));
        assert!(m.observe(Err("timeout".into())), "removed after FALL");
        assert_eq!(upstreams(&spec, &states).0, vec!["server=127.0.0.1#1153"]);
        let s = states.get_mut("mosdns").unwrap();
        s.observe(Err("x".into()));
        s.observe(Err("x".into()));
        assert_eq!(upstreams(&spec, &states), (vec!["server=119.29.29.29#53".to_string()], true), "fallback restored");
    }

    #[test]
    fn reload_keeps_unchanged_backends() {
        let spec = |backends: serde_json::Value| -> FrontendSpec {
            serde_json::from_value(
                json!({"fallback": ["server=1.0.0.1#53"], "backends": backends, "probeName": "example.com", "listen": []}),
            )
            .unwrap()
        };
        let old = spec(
            json!([{"id": "mosdns", "kind": "mosdns", "port": 1153, "priority": 10}, {"id": "mihomo", "kind": "mihomo", "port": 1053}]),
        );
        let mut states: BTreeMap<String, BackendState> = BTreeMap::new();
        for id in ["mosdns", "mihomo"] {
            let st = states.entry(id.into()).or_default();
            st.observe(Ok(()));
            st.observe(Ok(()));
        }
        // mihomo moved to another port, mosdns only changed priority, a third backend appears.
        let new = spec(json!([{"id": "mosdns", "kind": "mosdns", "port": 1153, "priority": 30},
            {"id": "mihomo", "kind": "mihomo", "port": 1054}, {"id": "other", "kind": "mosdns", "port": 1253}]));
        let carried = carry_states(&old, &states, &new);
        assert_eq!(carried.keys().collect::<Vec<_>>(), vec!["mihomo", "mosdns", "other"]);
        assert!(carried["mosdns"].healthy, "unchanged address keeps its health (stays listed)");
        assert!(!carried["mihomo"].healthy, "moved backend is probed again");
        assert!(!carried["other"].healthy, "new backend waits for RISE");
        assert_eq!(upstreams(&new, &carried).0, vec!["server=127.0.0.1#1153"]);
        let removed = carry_states(&new, &carried, &spec(json!([])));
        assert!(removed.is_empty());
        assert_eq!(upstreams(&spec(json!([])), &removed), (vec!["server=1.0.0.1#53".to_string()], true), "no backends → fallback");
    }
}
