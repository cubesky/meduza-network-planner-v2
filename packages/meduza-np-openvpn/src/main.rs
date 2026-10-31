use std::collections::BTreeMap;

use meduza_np_openvpn as ovpn;
use meduza_np_plugin_sdk::{run, PlanResult, Plugin, PluginError, PluginRequest, ProbeResult, ValidateResult};
use serde_json::{json, Value};

struct OpenVpn;

/// Asks the management socket for `state` (read-only); bounded by a short timeout.
#[cfg(unix)]
fn management_state(path: &str) -> Option<String> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    let mut s = UnixStream::connect(path).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    s.set_write_timeout(Some(Duration::from_secs(2))).ok()?;
    s.write_all(b"state\n").ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while buf.len() < 64 * 1024 {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if String::from_utf8_lossy(&buf).contains("\nEND") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let _ = s.write_all(b"quit\n");
    Some(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(not(unix))]
fn management_state(_path: &str) -> Option<String> {
    None
}

/// A management command reply, or why there is none.
type Reply = Result<String, String>;

/// One management connection for a sequence of commands; each call returns the raw reply (multi-line replies up to
/// `END`, others up to their `SUCCESS:` / `ERROR:` line). Notifications (`>…`) are skipped; replies are bounded.
#[cfg(unix)]
fn management_session(path: &str) -> Result<impl FnMut(&str) -> Reply, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    let s = UnixStream::connect(path).map_err(|e| format!("{path}: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
    let mut w = s.try_clone().map_err(|e| e.to_string())?;
    let mut r = BufReader::new(s);
    Ok(move |cmd: &str| {
        w.write_all(format!("{cmd}\n").as_bytes()).map_err(|e| e.to_string())?;
        let multi = cmd.starts_with("status");
        let mut out = String::new();
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
                return Err("management connection closed".into());
            }
            if line.starts_with('>') {
                continue;
            }
            out.push_str(&line);
            let l = line.trim_end();
            if out.len() > 1024 * 1024 || l.starts_with("ERROR:") || if multi { l == "END" } else { l.starts_with("SUCCESS:") } {
                return Ok(out);
            }
        }
    })
}

#[cfg(not(unix))]
fn management_session(_path: &str) -> Result<fn(&str) -> Reply, String> {
    Err("the management interface needs a unix socket".into())
}

fn process_running(run_dir: &str, id: &str) -> bool {
    // The management socket only exists while OpenVPN runs; planner's supervisor state is reported separately.
    std::path::Path::new(&format!("{run_dir}/openvpn-{id}.mgmt")).exists()
}

impl Plugin for OpenVpn {
    fn validate(&self, req: &PluginRequest) -> Result<ValidateResult, PluginError> {
        let cfg = ovpn::load(req)?;
        Ok(ValidateResult { errors: ovpn::validate(&cfg, req), warnings: Vec::new() })
    }

    fn plan(&self, req: &PluginRequest) -> Result<PlanResult, PluginError> {
        let cfg = ovpn::load(req)?;
        let errors = ovpn::validate(&cfg, req);
        if !errors.is_empty() {
            return Err(PluginError::invalid(errors));
        }
        Ok(PlanResult { plans: ovpn::plan(&cfg, req)?, warnings: Vec::new() })
    }

    fn probe(&self, req: &PluginRequest) -> Result<ProbeResult, PluginError> {
        let cfg = ovpn::load(req)?;
        let run = req.context.run_dir.trim_end_matches('/');
        let mut observed = BTreeMap::new();
        for inst in cfg.instances.iter().filter(|i| i.enabled) {
            let id = inst.id.as_str();
            observed.insert(
                id.to_string(),
                ovpn::Observed {
                    process_running: process_running(run, id),
                    device_present: std::path::Path::new(&format!("/sys/class/net/{}", inst.device())).exists(),
                    state: management_state(&format!("{run}/openvpn-{id}.mgmt")).and_then(|t| ovpn::parse_state(&t)),
                    status_file: std::fs::read_to_string(format!("{run}/openvpn-{id}.status")).ok(),
                },
            );
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        Ok(ProbeResult { instances: ovpn::statuses(&cfg, &observed, now) })
    }

    fn action(&self, req: &PluginRequest) -> Result<Value, PluginError> {
        match req.params.get("action").and_then(Value::as_str) {
            // Renders the effective server configuration (no key material) for diagnostics.
            Some("show-config") => {
                let cfg = ovpn::load(req)?;
                let id = req
                    .instance
                    .as_deref()
                    .and_then(|i| i.strip_prefix("openvpn/"))
                    .ok_or_else(|| PluginError::new("invalid-params", "instance required"))?;
                let inst =
                    cfg.instances.iter().find(|i| i.id.as_str() == id).ok_or_else(|| PluginError::new("not-found", "no such instance"))?;
                Ok(json!({"config": ovpn::render(inst, req)?.config}))
            }
            // Ends live Access sessions of a user (manager Access sessions page, D39); never touches the config.
            Some("disconnect-client") => {
                let cfg = ovpn::load(req)?;
                let id = req
                    .instance
                    .as_deref()
                    .and_then(|i| i.strip_prefix("openvpn/"))
                    .ok_or_else(|| PluginError::new("invalid-params", "instance required"))?;
                let inst = cfg
                    .instances
                    .iter()
                    .find(|i| i.id.as_str() == id && i.enabled)
                    .ok_or_else(|| PluginError::new("not-found", "no such enabled instance"))?;
                if inst.kind != ovpn::Kind::Access && !(inst.role == ovpn::Role::Server && inst.topology == ovpn::Topology::Subnet) {
                    return Err(PluginError::new("invalid-params", "only Access and subnet servers have clients"));
                }
                let username = req.params.get("username").and_then(Value::as_str).filter(|u| !u.is_empty() && u.len() <= 256);
                let client_id = req.params.get("clientId").and_then(Value::as_u64);
                if username.is_none() && client_id.is_none() {
                    return Err(PluginError::new("invalid-params", "username or clientId required"));
                }
                let run = req.context.run_dir.trim_end_matches('/');
                let mgmt =
                    management_session(&format!("{run}/openvpn-{id}.mgmt")).map_err(|e| PluginError::new("management-unavailable", e))?;
                ovpn::disconnect_clients(mgmt, username, client_id)
            }
            other => Err(PluginError::new("unsupported", format!("unknown action {other:?}"))),
        }
    }
}

fn main() {
    run(&OpenVpn, env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}
