#![cfg(unix)]
//! Real plugin worker, HTTP subscription/controller and a supervised stand-in core.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        // SAFETY: the test worker owns a separate process group, including its fake core.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.0.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // SAFETY: kill only that same owned group if graceful shutdown timed out.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}

fn wait_for(path: &std::path::Path, expected: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = Value::Null;
    while Instant::now() < deadline {
        last = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
        if last["subscriptions"]["default"]["status"] == expected {
            return last;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("waiting for {expected}: {last}");
}

#[test]
fn worker_inherits_dns_and_keeps_last_good_subscription_when_refresh_would_loop() {
    let root = std::env::temp_dir().join(format!("mnp-real-subscription-{}", std::process::id()));
    let run = root.join("run");
    std::fs::create_dir_all(&run).unwrap();
    let core = root.join("core");
    std::fs::write(&core, b"#!/bin/sh\nexec sleep 60\n").unwrap();
    std::fs::set_permissions(&core, std::fs::Permissions::from_mode(0o755)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let bad = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let bad_server = bad.clone();
    let stop_server = stop.clone();
    let server = std::thread::spawn(move || {
        while !stop_server.load(Ordering::SeqCst) {
            let (mut stream, _) = match listener.accept() {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => panic!("{e}"),
            };
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
            }
            let body = if request.starts_with("GET /sub ") {
                if bad_server.load(Ordering::SeqCst) {
                    "proxies: []\ndns: {nameserver: [127.0.0.1]}\n"
                } else {
                    "proxies: []\ndns: {nameserver: [119.29.29.29], enhanced-mode: fake-ip}\n"
                }
            } else if request.starts_with("GET /version ") {
                r#"{"version":"fixture"}"#
            } else {
                "{}"
            };
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
        }
    });
    let spec = root.join("spec.json");
    std::fs::write(&spec,json!({"controlled":{"dns":{"enable":true,"listen":"127.0.0.1:1053","ipv6":false},"secret":"fixture"},
        "dnsContext":{"front":["0.0.0.0:53"],"localIps":[]},"subscriptions":[{"id":"default","url":format!("http://{address}/sub"),"refreshSeconds":3600}],
        "active":"default","api":address.to_string(),"downloadProxy":null,"stateDir":root,"tproxy":false,"table":"fixture"}).to_string()).unwrap();
    let worker = Command::new(env!("CARGO_BIN_EXE_meduza-np-mihomo"))
        .args(["run", "--mihomo", core.to_str().unwrap(), "--spec", spec.to_str().unwrap(), "--run-dir", run.to_str().unwrap()])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(root.join("worker.log")).unwrap()))
        .spawn()
        .unwrap();
    let worker = Worker(worker);
    let state = run.join("mihomo-main.state");
    wait_for(&state, "ok");
    let final_path = run.join("mihomo-final.yaml");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !final_path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let last = std::fs::read(&final_path).unwrap();
    let normalized: Value = serde_norway::from_slice(&last).unwrap();
    assert_eq!(normalized["dns"]["nameserver"], json!(["119.29.29.29"]));
    assert_eq!(normalized["dns"]["enhanced-mode"], "fake-ip");
    assert_eq!(normalized["dns"]["listen"], "127.0.0.1:1053");
    let cache = root.join("mihomo/sub-default.yaml");
    let good = std::fs::read(&cache).unwrap();
    bad.store(true, Ordering::SeqCst);
    std::fs::write(run.join("mihomo-main.refresh"), b"").unwrap();
    let failed = wait_for(&state, "stale");
    assert!(failed["subscriptions"]["default"]["lastError"].as_str().unwrap().contains("front end"));
    assert_eq!(std::fs::read(cache).unwrap(), good);
    assert_eq!(std::fs::read(final_path).unwrap(), last);
    drop(worker);
    stop.store(true, Ordering::SeqCst);
    server.join().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
