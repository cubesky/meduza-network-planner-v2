//! Exercise the real wrapper: generated candidates, DNS probes, servers-file updates and SIGHUP reloads.
#![cfg(unix)]

use std::net::{SocketAddr, UdpSocket};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use meduza_np_dnsmasq::{FrontendSpec, SPEC_FILE};
use serde_json::{json, Value};

struct Resolver {
    address: SocketAddr,
    answer: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Resolver {
    fn new() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(30))).unwrap();
        let address = socket.local_addr().unwrap();
        let answer = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let (enabled, done) = (answer.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            let mut packet = [0; 512];
            while !done.load(Ordering::SeqCst) {
                if let Ok((len, peer)) = socket.recv_from(&mut packet) {
                    if len >= 12 && enabled.load(Ordering::SeqCst) {
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

struct Running {
    child: Child,
    dir: PathBuf,
}

impl Drop for Running {
    fn drop(&mut self) {
        // SAFETY: the Child owns this running wrapper, which cleans up its own foreground child on SIGTERM.
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let until = Instant::now() + Duration::from_secs(8);
        while Instant::now() < until {
            if self.child.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        for name in ["fake-dnsmasq", "dnsmasq.conf", "frontend.json", "dnsmasq-main.servers", "dnsmasq-main.state", "dnsmasq-main.pid"] {
            let _ = std::fs::remove_file(self.dir.join(name));
        }
        let _ = std::fs::remove_dir(&self.dir);
    }
}

fn wait_for(path: &Path, check: impl Fn(&str) -> bool, timeout: Duration) -> String {
    let until = Instant::now() + timeout;
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if check(&text) {
            return text;
        }
        assert!(Instant::now() < until, "timed out waiting for {}: {text}", path.display());
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn spec(services: Value) -> FrontendSpec {
    let mut req = meduza_np_plugin_sdk::test_request("plan", json!({"services": services}));
    req.context.capabilities = vec!["mosdns.dns".into(), "mihomo.dns".into()];
    req.providers.insert(
        meduza_np_dnsmasq::SLOT.into(),
        meduza_np_plugin_sdk::ResolvedProvider { path: "/test/dnsmasq".into(), source: "test".into(), version: None },
    );
    let cfg = meduza_np_dnsmasq::load(&req).unwrap().unwrap();
    let plans = meduza_np_dnsmasq::plan(&cfg, &req).unwrap();
    serde_json::from_str(&plans[0].files.iter().find(|f| f.path == SPEC_FILE).unwrap().content).unwrap()
}

#[test]
fn automatic_upstreams_reload_without_restarting_the_frontend() {
    let (fallback, mosdns, mihomo) = (Resolver::new(), Resolver::new(), Resolver::new());
    let mut services = json!({
        "dnsmasq":{"listenAddresses":["10.42.10.12"],"fallbackServers":[format!("127.0.0.1#{}",fallback.address.port())]},
        "mosdns":{"port":mosdns.address.port()},
        "mihomo":{"enabled":false,"dns":{"enabled":true,"listen":mihomo.address.to_string()}}
    });
    let dir = std::env::temp_dir().join(format!("meduza-dns-wrapper-{}-{}", std::process::id(), fallback.address.port()));
    std::fs::create_dir(&dir).unwrap();
    let fake = dir.join("fake-dnsmasq");
    // Keep a child alive like foreground dnsmasq; HUP must reload, never create a replacement child.
    std::fs::write(&fake, "#!/bin/sh\ntrap ':' HUP\nwhile :; do sleep 0.05; done\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(dir.join("dnsmasq.conf"), "").unwrap();
    let spec_file = dir.join("frontend.json");
    let initial = spec(services.clone());
    std::fs::write(&spec_file, serde_json::to_vec(&initial).unwrap()).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_meduza-np-dnsmasq"))
        .arg("frontend")
        .arg("--dnsmasq")
        .arg(&fake)
        .arg("--conf")
        .arg(dir.join("dnsmasq.conf"))
        .arg("--run-dir")
        .arg(&dir)
        .arg("--spec-file")
        .arg(&spec_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let running = Running { child, dir };
    let servers_file = running.dir.join("dnsmasq-main.servers");
    let state_file = running.dir.join("dnsmasq-main.state");
    let has = |address| format!("{}\n", meduza_np_dnsmasq::server_line(address));
    wait_for(&servers_file, |text| text == has(mosdns.address), Duration::from_secs(10));
    let pid = wait_for(&state_file, |text| text.contains("frontendPid"), Duration::from_secs(2));
    let pid = serde_json::from_str::<Value>(&pid).unwrap()["frontendPid"].clone();

    services["mosdns"]["enabled"] = json!(false);
    services["mihomo"]["enabled"] = json!(true);
    let updated = spec(services);
    std::fs::write(&spec_file, serde_json::to_vec(&updated).unwrap()).unwrap();
    // SAFETY: the running Child owns this wrapper pid.
    unsafe {
        libc::kill(running.child.id() as i32, libc::SIGHUP);
    }
    wait_for(&servers_file, |text| text == has(fallback.address), Duration::from_secs(2));
    wait_for(&servers_file, |text| text == has(mihomo.address), Duration::from_secs(10));
    mihomo.answer.store(false, Ordering::SeqCst);
    wait_for(&servers_file, |text| text == has(fallback.address), Duration::from_secs(12));
    mihomo.answer.store(true, Ordering::SeqCst);
    wait_for(&servers_file, |text| text == has(mihomo.address), Duration::from_secs(12));
    let latest: Value = serde_json::from_str(&std::fs::read_to_string(&state_file).unwrap()).unwrap();
    assert_eq!(latest["frontendPid"], pid, "adding/removing/recovering upstreams never restarts dnsmasq");
}
