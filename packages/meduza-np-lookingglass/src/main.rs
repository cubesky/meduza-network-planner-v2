use meduza_np_plugin_sdk::{run, run_status_command_with_env, PlanResult, Plugin, PluginError, PluginRequest, ProbeResult, ValidateResult};
use serde_json::{json, Value};
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    time::Duration,
};
mod local;
mod server;
mod tcping;

struct LookingGlass;

fn settings(req: &PluginRequest) -> Result<(bool, std::net::SocketAddr), PluginError> {
    let cfg = req.config.services.get("lookingglass");
    if cfg.is_some_and(|v| {
        !v.is_object() || v.get("enabled").is_some_and(|v| !v.is_boolean()) || v.get("listen").is_some_and(|v| !v.is_string())
    }) {
        return Err(PluginError::new("invalid-config", "expected enabled boolean and listen IP:port string"));
    }
    let enabled = cfg.and_then(|v| v.get("enabled")).and_then(Value::as_bool).unwrap_or(true);
    let listen = cfg.and_then(|v| v.get("listen")).and_then(Value::as_str).unwrap_or("0.0.0.0:18080");
    let address: std::net::SocketAddr = listen.parse().map_err(|_| PluginError::new("invalid-config", "listen must be IP:port"))?;
    if address.port() == 0 {
        return Err(PluginError::new("invalid-config", "listen port must be nonzero"));
    }
    Ok((enabled, address))
}

fn invalid() -> PluginError {
    PluginError::new(
        "invalid-params",
        "target IP must be RFC1918 private or RFC4193 ULA; tcping/iperf also accept IPv4:port or [IPv6]:port (1..65535); bgp also accepts an ASN; no hostnames or extra arguments",
    )
}

/// Normalize mapped IPv4 before checking the destination, regardless of the requester's address.
fn private_target(ip: IpAddr) -> Option<IpAddr> {
    match ip {
        IpAddr::V4(ip) if ip.is_private() => Some(ip.into()),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(ip) => private_target(ip.into()),
            None if ip.segments()[0] & 0xfe00 == 0xfc00 => Some(ip.into()),
            _ => None,
        },
        _ => None,
    }
}

/// Numeric IP with an optional explicit TCP port. Normalize mapped IPv4 before enforcing the target ACL.
fn endpoint(target: &str, default_port: u16) -> Result<SocketAddr, PluginError> {
    let address = if let Ok(ip) = target.parse::<IpAddr>() {
        SocketAddr::new(ip, default_port)
    } else {
        if target.contains('%') {
            return Err(invalid());
        }
        target.parse::<SocketAddr>().map_err(|_| invalid())?
    };
    if address.port() == 0 {
        return Err(invalid());
    }
    Ok(SocketAddr::new(private_target(address.ip()).ok_or_else(invalid)?, address.port()))
}

/// Never invoke a shell or accept a CLI fragment, hostname, regex or arbitrary FRR command.
fn commands(params: &Value) -> Result<Vec<(String, Vec<String>, u64)>, PluginError> {
    let operation = params.get("operation").and_then(Value::as_str).ok_or_else(invalid)?;
    let target = params.get("target").and_then(Value::as_str).ok_or_else(invalid)?.trim();
    let ip = target.parse::<IpAddr>().ok().and_then(private_target);
    if operation == "bgp" {
        let selector = if let Some(ip) = ip {
            ip.to_string()
        } else {
            let raw = target.strip_prefix("AS").or_else(|| target.strip_prefix("as")).unwrap_or(target);
            if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid());
            }
            let asn = raw.parse::<u32>().ok().filter(|n| *n > 0 && *n < u32::MAX).ok_or_else(invalid)?;
            format!("regexp _{asn}_")
        };
        let families: &[&str] = match ip {
            Some(IpAddr::V4(_)) => &["ipv4"],
            Some(IpAddr::V6(_)) => &["ipv6"],
            None => &["ipv4", "ipv6"],
        };
        return Ok(families
            .iter()
            .map(|f| {
                (
                    "vtysh".into(),
                    vec![
                        "-u".into(),
                        "-N".into(),
                        "meduza".into(),
                        "-d".into(),
                        "bgpd".into(),
                        "-H".into(),
                        "/dev/null".into(),
                        "-c".into(),
                        format!("show bgp {f} unicast {selector}"),
                    ],
                    8,
                )
            })
            .collect());
    }
    let address = match operation {
        "tcping" => Some(endpoint(target, 80)?),
        "iperf" => Some(endpoint(target, 5201)?),
        _ => None,
    };
    let ip = address.map(|a| a.ip()).or(ip).ok_or_else(invalid)?;
    if operation == "tcping" {
        return Ok(vec![("tcping".into(), vec![ip.to_string(), address.unwrap().port().to_string()], 12)]);
    }
    let family = if ip.is_ipv4() { "-4" } else { "-6" };
    let (program, args, seconds): (&str, Vec<&str>, u64) = match operation {
        "ping" => ("ping", vec![family, "-n", "-c", "4", "-W", "2"], 10),
        "mtr" => ("mtr", vec![family, "-n", "-r", "-w", "-c", "3", "-m", "16"], 20),
        "iperf" => ("iperf3", vec![family, "-J", "-t", "5", "-P", "1", "--connect-timeout", "3000"], 12),
        _ => return Err(invalid()),
    };
    let mut args: Vec<String> = args.into_iter().map(String::from).collect();
    if let Some(address) = address {
        args.extend(["-p".into(), address.port().to_string(), "-c".into()]);
    }
    args.push(ip.to_string());
    Ok(vec![(program.into(), args, seconds)])
}

fn binary(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .chain(["/usr/bin", "/usr/sbin", "/bin", "/sbin"].map(PathBuf::from))
        .map(|p| p.join(name))
        .find(|p| p.is_file())
}

fn executable(req: &PluginRequest, name: &str) -> Result<(PathBuf, Option<PathBuf>), PluginError> {
    if name == "iperf3" {
        if let Some(provider) = req.providers.get("iperf3.binary") {
            let path = PathBuf::from(&provider.path);
            if !path.is_file() {
                return Err(PluginError::new("unavailable", "iperf3 provider binary is missing; repair meduza-np-iperf3-pkg"));
            }
            return Ok((path, None));
        }
        if req.context.capabilities.iter().any(|c| c == "iperf3.binary") {
            return Err(PluginError::new("unavailable", "installed iperf3 provider is unavailable; no system fallback"));
        }
    }
    if name == "mtr" {
        if let Some(provider) = req.providers.get("mtr.binary") {
            let path = PathBuf::from(&provider.path);
            if provider.source != "system" {
                let helper = path.parent().unwrap_or(std::path::Path::new("/")).join("mtr-packet");
                if !path.is_file() || !helper.is_file() {
                    return Err(PluginError::new("unavailable", "packaged mtr or mtr-packet is missing; reinstall meduza-np-mtr-pkg"));
                }
                return Ok((path, Some(helper)));
            }
            return Ok((path, None));
        }
        if req.context.capabilities.iter().any(|c| c == "mtr.binary") {
            return Err(PluginError::new("unavailable", "installed MTR provider is unavailable; no system fallback"));
        }
    }
    if name == "vtysh" {
        if let Some(path) = req
            .providers
            .get("frr.daemons")
            .and_then(|p| std::path::Path::new(&p.path).parent())
            .map(|dir| dir.join("vtysh"))
            .filter(|p| p.is_file())
        {
            return Ok((path, None));
        }
    }
    binary(name).map(|p| (p, None)).ok_or_else(|| PluginError::new("unavailable", format!("install system tool {name} on this node")))
}

impl Plugin for LookingGlass {
    fn validate(&self, req: &PluginRequest) -> Result<ValidateResult, PluginError> {
        settings(req)?;
        Ok(ValidateResult::default())
    }
    fn plan(&self, req: &PluginRequest) -> Result<PlanResult, PluginError> {
        let (enabled, address) = settings(req)?;
        let plans = if enabled {
            vec![serde_json::from_value(json!({"protocolVersion":1,"requestId":req.request_id,"instance":"lookingglass/main",
                "processes":[{"id":"lookingglass/api","program":format!("{}/plugins/lookingglass", req.context.libexec_dir),
                    "argv":["serve","--listen",address.to_string()],"env":{"MEDUZA_NP_RUN_DIR":req.context.run_dir},"restart":"on-failure"}],
                "resources":[{"kind":"reservation","namespace":"tcp-port","value":address.port()}],
                "probes":[{"kind":"tcp","host":if address.ip().is_unspecified() { if address.is_ipv4() { "127.0.0.1".into() } else { "::1".into() } } else { address.ip().to_string() },"port":address.port(),"intervalSeconds":30}]
            })).map_err(|e| PluginError::new("invalid-plan", e.to_string()))?]
        } else {
            vec![]
        };
        Ok(PlanResult { plans, warnings: vec![] })
    }
    fn probe(&self, req: &PluginRequest) -> Result<ProbeResult, PluginError> {
        let (enabled, mut address) = settings(req)?;
        let listen = address.to_string();
        if address.ip().is_unspecified() {
            address.set_ip(if address.is_ipv4() { "127.0.0.1".parse().unwrap() } else { "::1".parse().unwrap() });
        }
        let state = if !enabled {
            "disabled"
        } else if std::net::TcpStream::connect_timeout(&address, Duration::from_millis(250)).is_ok() {
            "up"
        } else {
            "down"
        };
        serde_json::from_value(json!({"instances":[{"id":"lookingglass/main","kind":"lookingglass","state":state,"observedAt":meduza_np_plugin_sdk::now_rfc3339(),"listen":listen}]}))
            .map_err(|e| PluginError::new("internal", e.to_string()))
    }
    fn action(&self, req: &PluginRequest) -> Result<Value, PluginError> {
        if req.params.get("action").and_then(Value::as_str) != Some("query") {
            return Err(invalid());
        }
        let mut results = Vec::new();
        for (name, args, seconds) in commands(&req.params)? {
            if name == "tcping" {
                let address = SocketAddr::new(args[0].parse().map_err(|_| invalid())?, args[1].parse().map_err(|_| invalid())?);
                results.push(tcping::probe(address));
                continue;
            }
            let (executable, helper) = executable(req, &name)?;
            let helper = helper.map(|p| p.to_string_lossy().into_owned());
            let env = helper.as_deref().map(|path| vec![("MTR_PACKET", path)]).unwrap_or_default();
            let output = run_status_command_with_env(
                &executable.to_string_lossy(),
                &args.iter().map(String::as_str).collect::<Vec<_>>(),
                &env,
                Duration::from_secs(seconds),
                128 * 1024,
            )
            .map_err(|e| PluginError::new("unavailable", e.to_string()))?;
            if name == "vtysh" {
                let diagnostic = format!("{}\n{}", output.stderr, output.stdout).to_ascii_lowercase();
                if [
                    "processing failure",
                    "can't open configuration file",
                    "failed to connect",
                    "% unknown command",
                    "% ambiguous command",
                    "% command incomplete",
                ]
                .iter()
                .any(|marker| diagnostic.contains(marker))
                    || (output.status == Some(0) && output.stdout.trim().is_empty() && !output.stderr.trim().is_empty())
                {
                    return Err(PluginError::new(
                        "bgp-query-failed",
                        format!(
                            "vtysh failed: {}",
                            format!("{} {}", output.stderr, output.stdout).trim().chars().take(800).collect::<String>()
                        ),
                    ));
                }
            }
            results.push(json!({"command":name,"arguments":args,"exitCode":output.status,"timedOut":output.timed_out,
                "truncated":output.stdout.len() >= 128 * 1024 || output.stderr.len() >= 64 * 1024,"stdout":output.stdout,"stderr":output.stderr}));
        }
        Ok(json!({"results":results}))
    }
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("serve") {
        let args: Vec<_> = std::env::args().skip(2).collect();
        let listen = match args.as_slice() {
            [] => std::env::var("MEDUZA_NP_LOOKINGGLASS_LISTEN").unwrap_or_else(|_| "0.0.0.0:18080".into()),
            [flag, value] if flag == "--listen" => value.clone(),
            _ => {
                eprintln!("usage: meduza-np-lookingglass serve [--listen IP:PORT]");
                std::process::exit(2);
            }
        };
        if let Err(error) = server::run(&listen) {
            eprintln!("meduza-np-lookingglass: {error}");
            std::process::exit(1);
        }
        return;
    }
    run(&LookingGlass, "meduza-np-lookingglass", env!("CARGO_PKG_VERSION"));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn target_acl_applies_to_all_ip_operations_and_normalizes_mapped_ipv4() {
        for operation in ["ping", "mtr", "tcping", "iperf", "bgp"] {
            for target in
                ["10.0.0.1", "10.255.255.254", "172.16.0.1", "172.31.255.254", "192.168.0.1", "192.168.255.254", "fc00::1", "fdff::1"]
            {
                assert!(commands(&json!({"operation":operation,"target":target})).is_ok(), "{operation} {target}");
            }
            for target in [
                "8.8.8.8",
                "127.0.0.1",
                "169.254.1.1",
                "100.64.0.1",
                "172.15.255.254",
                "172.32.0.1",
                "192.167.1.1",
                "192.169.1.1",
                "192.0.2.1",
                "::1",
                "fe80::1",
                "2001:db8::1",
                "fbff::1",
                "fe00::1",
                "::ffff:8.8.8.8",
                "::ffff:127.0.0.1",
                "::192.168.0.1",
            ] {
                assert!(commands(&json!({"operation":operation,"target":target})).is_err(), "{operation} {target}");
            }
            let cmds = commands(&json!({"operation":operation,"target":"::ffff:10.0.0.1"})).unwrap();
            let last = if operation == "tcping" { &cmds[0].1[0] } else { cmds[0].1.last().unwrap() };
            assert!(last.ends_with("10.0.0.1"));
            assert!(!last.contains("ffff"));
        }
    }
    #[test]
    fn plans_separate_api_process_with_configurable_port_and_honours_disable() {
        let mut req: PluginRequest = serde_json::from_value(json!({"protocolVersion":1,"requestId":"plan","operation":"plan",
            "config":{"schemaVersion":1,"revision":"rev-000001","node":{"id":"test"}},
            "context":{"generationRoot":"/run/meduza-np/generations/test","stateDir":"/etc/meduza-np-state","runDir":"/run/meduza-np","libexecDir":"/usr/libexec/meduza-np"}})).unwrap();
        let default = LookingGlass.plan(&req).unwrap();
        assert_eq!(default.plans.len(), 1);
        assert!(default.plans[0].validate().is_empty(), "{:?}", default.plans[0].validate().errors());
        assert_eq!(default.plans[0].processes[0].argv, ["serve", "--listen", "0.0.0.0:18080"]);
        req.config.services.insert("lookingglass".into(), json!({"enabled":true,"listen":"[::]:18081"}));
        let custom = LookingGlass.plan(&req).unwrap();
        assert_eq!(custom.plans[0].processes[0].argv[2], "[::]:18081");
        req.config.services.insert("lookingglass".into(), json!({"enabled":false}));
        assert!(LookingGlass.plan(&req).unwrap().plans.is_empty());
        for listen in ["0.0.0.0:0", "example.com:18080", "invalid"] {
            req.config.services.insert("lookingglass".into(), json!({"listen":listen}));
            assert!(LookingGlass.validate(&req).is_err());
        }
    }
    #[test]
    fn rejects_command_injection_and_ambiguous_targets() {
        for operation in ["ping", "mtr", "tcping", "iperf", "bgp"] {
            for target in [
                "--help",
                "1.1.1.1;id",
                "1.1.1.1\nexit",
                "example.com",
                "0.0.0.0",
                "::",
                "ff02::1",
                "255.255.255.255",
                "AS65000;exit",
                "65000|.*",
            ] {
                assert!(commands(&json!({"operation":operation,"target":target})).is_err(), "{operation} {target}");
            }
        }
    }
    #[test]
    fn tcp_targets_accept_strict_numeric_ports_and_keep_private_address_policy() {
        for operation in ["tcping", "iperf"] {
            for (target, ip, port) in [
                ("10.42.10.1:443", "10.42.10.1", "443"),
                ("172.16.0.1:1", "172.16.0.1", "1"),
                ("[fd42::1]:65535", "fd42::1", "65535"),
                ("[::ffff:192.168.0.1]:5202", "192.168.0.1", "5202"),
            ] {
                let cmds = commands(&json!({"operation":operation,"target":target})).unwrap();
                let (program, args, timeout) = &cmds[0];
                assert!(*timeout <= 12);
                if operation == "tcping" {
                    assert_eq!(program, "tcping");
                    assert_eq!(args, &[ip, port]);
                } else {
                    assert_eq!(program, "iperf3");
                    assert_eq!(&args[args.len() - 4..], &["-p", port, "-c", ip]);
                    assert_eq!(args[0], if ip.contains(':') { "-6" } else { "-4" });
                }
            }
            let default = commands(&json!({"operation":operation,"target":"10.0.0.1"})).unwrap();
            let port = if operation == "tcping" { &default[0].1[1] } else { &default[0].1[default[0].1.len() - 3] };
            assert_eq!(port, if operation == "tcping" { "80" } else { "5201" });
            for target in [
                "8.8.8.8:443",
                "127.0.0.1:80",
                "[2001:db8::1]:443",
                "[::ffff:8.8.8.8]:443",
                "[::1]:80",
                "[fe80::1%1]:443",
                "[fd42::1%1]:443",
                "example.com:443",
                "10.0.0.1:0",
                "10.0.0.1:65536",
                "10.0.0.1:-1",
                "10.0.0.1:+80",
                "10.0.0.1:1.5",
                "10.0.0.1:",
                "10.0.0.1:443 --reverse",
                "10.0.0.1:443;id",
                "http://10.0.0.1:443",
                "[fd42::1]:0",
            ] {
                assert!(commands(&json!({"operation":operation,"target":target})).is_err(), "{operation} {target}");
            }
        }
        for operation in ["ping", "mtr", "bgp"] {
            assert!(commands(&json!({"operation":operation,"target":"10.0.0.1:443"})).is_err(), "ports are only valid for TCP operations");
        }
    }

    #[test]
    fn bounds_probes_and_as_path_queries() {
        for operation in ["ping", "mtr", "iperf"] {
            for target in ["10.42.10.1", "fd42::1"] {
                let cmds = commands(&json!({"operation":operation,"target":target})).unwrap();
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0].1.last().unwrap(), target);
                assert!(cmds[0].2 <= 20);
            }
        }
        let cmds = commands(&json!({"operation":"bgp","target":"AS65000"})).unwrap();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].1.last().unwrap(), "show bgp ipv4 unicast regexp _65000_");
        assert_eq!(commands(&json!({"operation":"bgp","target":"10.0.0.1"})).unwrap().len(), 1);
        for target in ["AS0", "AS4294967295", "4294967296"] {
            assert!(commands(&json!({"operation":"bgp","target":target})).is_err());
        }
    }
}
