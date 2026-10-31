use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use meduza_np_avahi as av;
use meduza_np_plugin_sdk::{run, PlanResult, Plugin, PluginError, PluginRequest, ProbeResult, ValidateResult};
use meduza_np_protocol::report::{InstanceStatus, ServiceState};
use serde_json::{json, Value};

struct Avahi;

#[cfg(unix)]
fn daemon_info() -> Result<(String, i32, String), String> {
    av::dbus::Conn::system(Duration::from_secs(2))?.avahi_info()
}

#[cfg(not(unix))]
fn daemon_info() -> Result<(String, i32, String), String> {
    Err("D-Bus is only available on unix".into())
}

impl Plugin for Avahi {
    fn validate(&self, req: &PluginRequest) -> Result<ValidateResult, PluginError> {
        let Some(cfg) = av::load(req)? else { return Ok(ValidateResult::default()) };
        Ok(ValidateResult { errors: av::validate(&cfg, req), warnings: av::warnings(&cfg, req) })
    }

    fn plan(&self, req: &PluginRequest) -> Result<PlanResult, PluginError> {
        let Some(cfg) = av::load(req)? else { return Ok(PlanResult::default()) };
        let errors = av::validate(&cfg, req);
        if !errors.is_empty() {
            return Err(PluginError::invalid(errors));
        }
        let mut warnings = av::warnings(&cfg, req);
        if cfg.bridge.enabled
            && req.config.services.get("dnsmasq").and_then(|d| d.get("mdnsForward")).and_then(Value::as_bool) != Some(true)
        {
            warnings.push("the .local bridge runs, but dnsmasq does not forward .local to it (dnsmasq mdnsForward is off)".into());
        }
        Ok(PlanResult { plans: av::plan(&cfg, req)?, warnings })
    }

    /// Daemon state via D-Bus (not just "process exists") and the bridge via a real `.local` query.
    fn probe(&self, req: &PluginRequest) -> Result<ProbeResult, PluginError> {
        let Some(cfg) = av::load(req)? else { return Ok(ProbeResult::default()) };
        if !cfg.enabled {
            return Ok(ProbeResult::default());
        }
        let ts = meduza_np_plugin_sdk::now_rfc3339();
        let mut detail = BTreeMap::new();
        detail.insert("mode".into(), serde_json::to_value(cfg.mode).unwrap_or_default());
        detail.insert("activeInterfaces".into(), json!(av::active_interfaces(&cfg, Path::new("/sys/class/net"))));
        detail.insert("reflector".into(), Value::Bool(cfg.reflector.enabled));
        detail.insert("publishing".into(), json!(cfg.publish));
        let (state, reason) = match daemon_info() {
            Ok((version, st, fqdn)) => {
                detail.insert("version".into(), Value::String(version));
                detail.insert("hostName".into(), Value::String(fqdn));
                match st {
                    av::dbus::SERVER_RUNNING => (ServiceState::Up, None),
                    0 | 1 => (ServiceState::Starting, Some("registering host name".to_string())),
                    3 => (ServiceState::Degraded, Some("host name collision".to_string())),
                    other => (ServiceState::Down, Some(format!("server state {other}"))),
                }
            }
            // Nobody owns org.freedesktop.Avahi (calls never bus-activate it, D51).
            Err(e) if cfg.mode == av::Mode::System && (e.contains("NameHasNoOwner") || e.contains("ServiceUnknown")) => {
                (ServiceState::Down, Some(format!("{}: {e}", av::NO_SYSTEM_DAEMON)))
            }
            Err(e) => (ServiceState::Down, Some(e)),
        };
        let mut out =
            vec![InstanceStatus { id: av::INSTANCE.into(), kind: "avahi".into(), state, observed_at: Some(ts.clone()), reason, detail }];
        if cfg.bridge.enabled {
            // NXDOMAIN for an unknown name proves the bridge→D-Bus→Avahi path; SERVFAIL means it is broken.
            let addr = SocketAddr::new(IpAddr::from([127, 0, 0, 1]), cfg.bridge.port);
            let r = meduza_np_plugin_sdk::dns::probe(
                addr,
                "meduza-np-probe.local",
                Duration::from_millis(u64::from(cfg.bridge.timeout_ms) + 1000),
            );
            out.push(InstanceStatus {
                id: "avahi/bridge".into(),
                kind: "mdns-bridge".into(),
                state: if r.is_ok() { ServiceState::Up } else { ServiceState::Down },
                observed_at: Some(ts),
                reason: r.err(),
                detail: BTreeMap::from([("listen".to_string(), Value::String(addr.to_string()))]),
            });
        }
        Ok(ProbeResult { instances: out })
    }

    /// `resolve`: diagnostic lookup of one `.local` name through Avahi (A and AAAA).
    fn action(&self, req: &PluginRequest) -> Result<Value, PluginError> {
        match req.params.get("action").and_then(Value::as_str) {
            Some("resolve") => {
                let name = req.params.get("name").and_then(Value::as_str).unwrap_or_default().trim_end_matches('.').to_ascii_lowercase();
                if !name.ends_with(".local")
                    || name.len() > 253
                    || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
                {
                    return Err(PluginError::new("invalid-request", "expected a name under .local"));
                }
                let t = Duration::from_secs(3);
                let fmt = |r: Result<Option<IpAddr>, String>| match r {
                    Ok(Some(a)) => json!({"address": a.to_string()}),
                    Ok(None) => json!({"address": null}),
                    Err(e) => json!({"error": e}),
                };
                Ok(
                    json!({"name": name, "a": fmt(av::dbus::resolve_once(-1, &name, false, t)), "aaaa": fmt(av::dbus::resolve_once(-1, &name, true, t))}),
                )
            }
            other => Err(PluginError::new("unsupported", format!("unknown action {other:?}"))),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("bridge") {
        av::bridge::run(&args[2..]);
    }
    run(&Avahi, env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}
