//! Exercise the real route backend in a deliberately isolated Linux network namespace.
//! Run only as `unshare -n ./route_recovery_probe --isolated`; never on a production namespace.

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() {
    use meduza_np_planner::netbackend::{LinuxBackend, NetBackend};
    use meduza_np_protocol::plan::{IpFamily, NetworkResource};
    use std::os::unix::fs::MetadataExt;
    let guard = std::env::args().nth(1).as_deref() == Some("--isolated")
        && std::fs::metadata("/proc/self/ns/net").unwrap().ino() != std::fs::metadata("/proc/1/ns/net").unwrap().ino();
    assert!(guard, "refusing to operate outside a separate network namespace; use unshare -n");
    fn ip(args: &[&str]) -> String {
        let out = std::process::Command::new("ip").args(args).output().unwrap();
        assert!(out.status.success(), "ip {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
    ip(&["link", "set", "lo", "up"]);
    ip(&["link", "add", "eth0", "type", "dummy"]);
    ip(&["link", "set", "eth0", "up"]);
    ip(&["address", "add", "10.42.10.2/24", "dev", "eth0"]);
    let backend = LinuxBackend::default();
    let routes = [
        NetworkResource::Route {
            destination: "0.0.0.0/0".into(),
            via: None,
            device: Some("lo".into()),
            table: Some(4626),
            metric: None,
            route_type: Some("local".into()),
        },
        NetworkResource::Route {
            destination: "0.0.0.0/0".into(),
            via: Some("10.42.10.1".into()),
            device: Some("eth0".into()),
            table: Some(4627),
            metric: None,
            route_type: None,
        },
    ];
    for (priority, mark, table) in [(9626, 0x4d4e, 4626), (9627, 0x4d50, 4627)] {
        backend.add(&NetworkResource::Rule { priority, fwmark: mark, mask: None, table, family: IpFamily::Ipv4 }).await.unwrap();
    }
    // A caller-owned third table must survive restoring our own resources.
    ip(&["route", "add", "blackhole", "203.0.113.0/24", "table", "4999"]);
    for route in &routes {
        assert_eq!(backend.exists(route).await, Some(false), "fresh namespace has no FIB table");
        backend.add(route).await.unwrap();
        assert_eq!(backend.exists(route).await, Some(true));
        backend.remove(route).await.unwrap();
        assert_eq!(backend.exists(route).await, Some(false), "deleting the last route removes the table");
        // This is the apply engine's recovery condition after a recreated container/reboot.
        if backend.exists(route).await == Some(false) {
            backend.add(route).await.unwrap();
        }
        assert_eq!(backend.exists(route).await, Some(true));
    }
    assert!(ip(&["-4", "route", "get", "1.1.1.1", "mark", "0x4d4e"]).contains("local 1.1.1.1"));
    assert!(ip(&["-4", "route", "get", "1.1.1.1", "mark", "0x4d50"]).contains("table 4627"));
    assert!(ip(&["route", "show", "table", "4999"]).contains("blackhole 203.0.113.0/24"));
    println!("PASS: fresh and lost policy tables restored; proxy mark routes locally, reply mark routes via main router; unrelated routes preserved");
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("Linux-only isolated route recovery probe");
}
