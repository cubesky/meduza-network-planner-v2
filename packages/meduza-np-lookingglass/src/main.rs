use meduza_np_plugin_sdk::{run, run_status_command, PlanResult, Plugin, PluginError, PluginRequest, ProbeResult, ValidateResult};
use serde_json::{json, Value};
use std::{net::IpAddr, path::PathBuf, time::Duration};

struct LookingGlass;

fn invalid() -> PluginError {
    PluginError::new("invalid-params", "use ping/mtr/iperf with a unicast IP, or bgp with an ASN/IP; no hostnames or extra arguments")
}

/// Never invoke a shell or accept a CLI fragment, hostname, port, regex or arbitrary FRR command.
fn commands(params: &Value) -> Result<Vec<(String, Vec<String>, u64)>, PluginError> {
    let operation = params.get("operation").and_then(Value::as_str).ok_or_else(invalid)?;
    let target = params.get("target").and_then(Value::as_str).ok_or_else(invalid)?.trim();
    let ip = target
        .parse::<IpAddr>()
        .ok()
        .filter(|ip| !ip.is_unspecified() && !ip.is_multicast() && !matches!(ip, IpAddr::V4(v) if v.is_broadcast()));
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
            .map(|f| ("vtysh".into(), vec!["-N".into(), "meduza".into(), "-c".into(), format!("show bgp {f} unicast {selector}")], 8))
            .collect());
    }
    let ip = ip.ok_or_else(invalid)?;
    let family = if ip.is_ipv4() { "-4" } else { "-6" };
    let (program, args, seconds): (&str, Vec<&str>, u64) = match operation {
        "ping" => ("ping", vec![family, "-n", "-c", "4", "-W", "2"], 10),
        "mtr" => ("mtr", vec![family, "-n", "-r", "-w", "-c", "3", "-m", "16"], 20),
        "iperf" => ("iperf3", vec![family, "-J", "-t", "5", "-P", "1", "--connect-timeout", "3000", "-c"], 12),
        _ => return Err(invalid()),
    };
    let mut args: Vec<String> = args.into_iter().map(String::from).collect();
    args.push(ip.to_string());
    Ok(vec![(program.into(), args, seconds)])
}

fn binary(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .chain(["/usr/bin", "/usr/sbin", "/bin", "/sbin"].map(PathBuf::from))
        .map(|p| p.join(name))
        .find(|p| p.is_file())
}

impl Plugin for LookingGlass {
    fn validate(&self, _: &PluginRequest) -> Result<ValidateResult, PluginError> {
        Ok(ValidateResult::default())
    }
    fn plan(&self, _: &PluginRequest) -> Result<PlanResult, PluginError> {
        Ok(PlanResult { plans: vec![], warnings: vec![] })
    }
    fn probe(&self, _: &PluginRequest) -> Result<ProbeResult, PluginError> {
        Ok(ProbeResult::default())
    }
    fn action(&self, req: &PluginRequest) -> Result<Value, PluginError> {
        if req.params.get("action").and_then(Value::as_str) != Some("query") {
            return Err(invalid());
        }
        let mut results = Vec::new();
        for (name, args, seconds) in commands(&req.params)? {
            let executable =
                binary(&name).ok_or_else(|| PluginError::new("unavailable", format!("install system tool {name} on this node")))?;
            let output = run_status_command(
                &executable.to_string_lossy(),
                &args.iter().map(String::as_str).collect::<Vec<_>>(),
                Duration::from_secs(seconds),
                128 * 1024,
            )
            .map_err(|e| PluginError::new("unavailable", e.to_string()))?;
            results.push(json!({"command":name,"arguments":args,"exitCode":output.status,"timedOut":output.timed_out,
                "truncated":output.stdout.len() >= 128 * 1024 || output.stderr.len() >= 64 * 1024,"stdout":output.stdout,"stderr":output.stderr}));
        }
        Ok(json!({"results":results}))
    }
}

fn main() {
    run(&LookingGlass, "meduza-np-lookingglass", env!("CARGO_PKG_VERSION"));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_command_injection_and_ambiguous_targets() {
        for operation in ["ping", "mtr", "iperf", "bgp"] {
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
    fn bounds_probes_and_as_path_queries() {
        for operation in ["ping", "mtr", "iperf"] {
            for target in ["10.42.10.1", "2001:db8::1"] {
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
