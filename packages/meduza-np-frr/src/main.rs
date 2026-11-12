use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use meduza_np_frr as frr;
use meduza_np_plugin_sdk::{run, run_status_command, PlanResult, Plugin, PluginError, PluginRequest, ProbeResult, ValidateResult};
use serde_json::Value;

struct Frr;

fn vtysh_result(req: &PluginRequest, cmd: &str) -> Result<Value, String> {
    let provider = req.providers.get(frr::SLOT).ok_or("FRR provider unavailable")?.path.clone();
    let dir = std::path::Path::new(&provider).parent().ok_or("FRR provider has no directory")?.to_path_buf();
    // vtysh lives in /usr/bin on most systems, next to the daemons on some builds.
    let candidates = [dir.join("vtysh"), "/usr/bin/vtysh".into(), "/usr/sbin/vtysh".into()];
    let vtysh = candidates.iter().find(|p| p.exists()).ok_or("vtysh unavailable")?;
    let out = run_status_command(&vtysh.to_string_lossy(), &["-N", frr::PATHSPACE, "-c", cmd], Duration::from_secs(5), 4 * 1024 * 1024)
        .map_err(|e| e.to_string())?;
    if out.timed_out {
        return Err("query timed out".into());
    }
    if out.status != Some(0) {
        if out.stdout.len() >= 4 * 1024 * 1024 {
            return Err("query exceeded the 4 MiB collection limit".into());
        }
        return Err(format!("vtysh exit {:?}", out.status));
    }
    serde_json::from_str(&out.stdout).map_err(|_| {
        if out.stdout.len() >= 4 * 1024 * 1024 {
            "query exceeded the 4 MiB collection limit".into()
        } else {
            "invalid JSON response".into()
        }
    })
}

fn vtysh(req: &PluginRequest, cmd: &str) -> Option<Value> {
    vtysh_result(req, cmd).ok()
}

impl Plugin for Frr {
    fn validate(&self, req: &PluginRequest) -> Result<ValidateResult, PluginError> {
        let cfg = frr::load(req)?;
        Ok(ValidateResult { errors: frr::validate(&cfg, req), warnings: Vec::new() })
    }

    fn plan(&self, req: &PluginRequest) -> Result<PlanResult, PluginError> {
        let cfg = frr::load(req)?;
        let errors = frr::validate(&cfg, req);
        if !errors.is_empty() {
            return Err(PluginError::invalid(errors));
        }
        let plans = if cfg.enabled { frr::plan(&cfg, req, &frr::daemon_identity(req)?)? } else { Vec::new() };
        Ok(PlanResult { plans, warnings: Vec::new() })
    }

    fn probe(&self, req: &PluginRequest) -> Result<ProbeResult, PluginError> {
        let cfg = frr::load(req)?;
        let ts = meduza_np_plugin_sdk::now_rfc3339();
        let mut instances = Vec::new();
        let summary = vtysh(req, "show bgp summary json").unwrap_or(Value::Null);
        let mut snapshot = frr::paths::Snapshot::default();
        let mut has_tables = false;
        let mut local_routes = std::collections::BTreeSet::new();
        let mut details = frr::PeerDetails::default();
        let mut errors = Vec::new();
        if summary.is_null() {
            errors.push("BGP summary unavailable".into());
        } else {
            for family in ["ipv4", "ipv6"] {
                match vtysh_result(req, &format!("show bgp {family} unicast json")) {
                    Ok(table) if table.get("routes").is_some_and(Value::is_object) => {
                        snapshot.add_table(family, &table);
                        has_tables = true;
                        local_routes.extend(frr::local_routes(&table));
                        for (peer, routes) in frr::bgp_routes_by_peer(&table) {
                            details.routes.entry(peer).or_default().extend(routes);
                        }
                        for (peer, paths) in frr::bgp_as_paths_by_peer(&table) {
                            details.as_paths.entry(peer).or_default().extend(paths);
                        }
                    }
                    Ok(_) => errors.push(format!("{family}: routing table unavailable")),
                    Err(error) => errors.push(format!("{family}: {error}")),
                }
            }
        }
        // The router ID names an unmanaged peer in the topology view; managed peers are known by name.
        details.router_ids = if summary.is_null() || !cfg.external_neighbors.iter().any(|n| n.enabled) {
            BTreeMap::new()
        } else {
            vtysh(req, "show bgp neighbors json").as_ref().map(frr::bgp_router_ids).unwrap_or_default()
        };
        for routes in details.routes.values_mut() {
            routes.sort();
            routes.dedup();
        }
        for paths in details.as_paths.values_mut() {
            paths.sort();
            paths.dedup();
        }
        instances.extend(frr::bgp_statuses(&cfg, &summary, &details, &ts));
        instances.push(snapshot.status(&errors, &ts));
        if has_tables {
            instances.extend(frr::origination_status(&frr::announced_networks(&cfg, req), &local_routes, &ts));
        }
        if !cfg.bfd.profiles.is_empty() || !cfg.bfd.peers.is_empty() {
            if let Some(bfd) = vtysh(req, "show bfd peers json") {
                instances.extend(frr::bfd_statuses(&bfd, &ts));
            }
        }
        if cfg.anycast.enabled {
            let ospf = vtysh(req, "show ip ospf neighbor json").unwrap_or(Value::Null);
            let mut states = BTreeMap::new();
            for a in &cfg.anycast.instances {
                let path = format!("{}/frr-anycast-{}.state", req.context.run_dir.trim_end_matches('/'), a.id);
                states.insert(a.id.to_string(), std::fs::read_to_string(path).unwrap_or_default());
            }
            instances.extend(frr::ospf_statuses(&cfg, &ospf, &states, &ts));
        }
        Ok(ProbeResult { instances })
    }
}

static STOP: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_term(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn ip(args: &[&str]) -> bool {
    std::process::Command::new("ip").args(args).stderr(std::process::Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

/// Announce: addresses on, device up (OSPF advertises the stub). Withdraw: device down, addresses off, so the
/// node itself routes Anycast traffic to another node instead of delivering it to the failed local service.
fn set_announced(device: &str, addresses: &[String], up: bool) {
    if up {
        for a in addresses {
            ip(&["addr", "replace", a, "dev", device]);
        }
        ip(&["link", "set", "dev", device, "up"]);
    } else {
        ip(&["link", "set", "dev", device, "down"]);
        for a in addresses {
            ip(&["addr", "del", a, "dev", device]);
        }
    }
}

/// `frr anycast-guard --device D --state FILE --health JSON [--address CIDR]...` — long-running, supervised by planner.
fn anycast_guard(args: &[String]) -> ! {
    let get = |k: &str| args.windows(2).find(|w| w[0] == k).map(|w| w[1].clone());
    let (Some(device), Some(state), Some(health)) = (get("--device"), get("--state"), get("--health")) else {
        eprintln!("usage: anycast-guard --device DEV --state FILE --health JSON [--address CIDR]...");
        std::process::exit(2)
    };
    let addresses: Vec<String> = args.windows(2).filter(|w| w[0] == "--address").map(|w| w[1].clone()).collect();
    let health: frr::HealthCheck = serde_json::from_str(&health).unwrap_or_else(|e| {
        eprintln!("invalid health check: {e}");
        std::process::exit(2)
    });
    #[cfg(unix)]
    // SAFETY: installs an async-signal-safe handler that only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term as *const () as libc::sighandler_t);
    }
    let write_state = |s: &str| {
        let tmp = format!("{state}.tmp");
        if std::fs::write(&tmp, format!("{s} {}\n", meduza_np_plugin_sdk::now_rfc3339())).is_ok() {
            let _ = std::fs::rename(&tmp, &state);
        }
    };
    // Start withdrawn; announce only after RISE good checks.
    set_announced(&device, &addresses, false);
    write_state("unhealthy starting");
    let mut h = frr::guard::Hysteresis::default();
    while !STOP.load(Ordering::SeqCst) {
        let result = frr::guard::check(&health);
        if let Some(now_healthy) = h.observe(result.is_ok()) {
            set_announced(&device, &addresses, now_healthy);
            match &result {
                Ok(()) => write_state("healthy"),
                Err(e) => write_state(&format!("unhealthy {}", e.replace('\n', " "))),
            }
            eprintln!("anycast {device}: {}", if now_healthy { "announced" } else { "withdrawn" });
        }
        for _ in 0..20 {
            if STOP.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    // Maintenance / stop: withdraw before the service goes away.
    set_announced(&device, &addresses, false);
    write_state("unhealthy stopped");
    std::process::exit(0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("anycast-guard") {
        anycast_guard(&args[2..]);
    }
    run(&Frr, env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}
