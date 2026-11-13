//! Exercise the real connect helper over its restricted Unix socket, without LDAP or a VPN daemon.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn connect(push: bool, result: Value) -> (bool, String, Value) {
    let dir = std::env::temp_dir().join(format!("mnp-access-hook-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("access.sock");
    let file = dir.join("client.conf");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(peer.try_clone().unwrap()).read_line(&mut line).unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        writeln!(peer, "{}", json!({"result":result})).unwrap();
        request
    });
    let output = Command::new(env!("CARGO_BIN_EXE_openvpn-auth"))
        .args(["--instance", "access", "--connect"])
        .arg(&file)
        .env("MEDUZA_NP_ACCESS_SOCKET", socket)
        .env("common_name", "alice")
        .env("MEDUZA_NP_ACCESS_PUSH_FRR", if push { "1" } else { "0" })
        .output()
        .unwrap();
    let request = server.join().unwrap();
    let content = std::fs::read_to_string(&file).unwrap_or_default();
    std::fs::remove_dir_all(dir).unwrap();
    (output.status.success(), content, request)
}

#[test]
fn authenticated_connect_writes_frr_routes_and_keeps_session_lifetime() {
    let (ok, content, request) = connect(true, json!({"allow":true,"sessionSeconds":3600,"pushRoutes":["route 192.0.2.0 255.255.255.0"]}));
    assert!(ok);
    assert_eq!(content, "session-timeout 3600\npush \"route 192.0.2.0 255.255.255.0\"\n");
    assert_eq!(request["method"], "access.session");
    assert_eq!(request["params"]["pushFrr"], true);
    assert!(request["params"].get("password").is_none());
    let (ok, content, _) = connect(true, json!({"allow":true,"sessionSeconds":0,"pushRoutes":[]}));
    assert!(ok && content.is_empty(), "a complete empty FRR table is usable");
    let (ok, content, request) = connect(false, json!({"allow":true,"sessionSeconds":600}));
    assert!(ok);
    assert_eq!(content, "session-timeout 600\n");
    assert_eq!(request["params"]["pushFrr"], false);
}

#[test]
fn incomplete_failed_and_injected_route_exports_deny_without_writing() {
    for result in [
        json!({"allow":true,"sessionSeconds":3600}),
        json!({"allow":false,"reasonCode":"frr-routes-unavailable"}),
        json!({"allow":true,"pushRoutes":["route 192.0.2.0 255.255.255.0\npush redirect-gateway"]}),
        json!({"allow":true,"pushRoutes":["route example.test 255.255.255.0"]}),
        json!({"allow":true,"pushRoutes":vec!["route 192.0.2.0 255.255.255.0";257]}),
    ] {
        let (ok, content, _) = connect(true, result);
        assert!(!ok);
        assert!(content.is_empty());
    }
}
