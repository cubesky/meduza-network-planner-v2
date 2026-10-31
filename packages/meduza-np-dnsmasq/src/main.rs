use std::collections::BTreeMap;
use std::net::SocketAddr;

use meduza_np_dnsmasq as dm;
use meduza_np_plugin_sdk::{run, PlanResult, Plugin, PluginError, PluginRequest, ProbeResult, ValidateResult};
use meduza_np_protocol::report::{InstanceStatus, ServiceState};
use serde_json::Value;

struct Dnsmasq;

impl Plugin for Dnsmasq {
    fn validate(&self, req: &PluginRequest) -> Result<ValidateResult, PluginError> {
        let errors = match dm::load(req)? {
            Some(cfg) => dm::validate(&cfg, req),
            None => Vec::new(),
        };
        Ok(ValidateResult { errors, warnings: Vec::new() })
    }

    fn plan(&self, req: &PluginRequest) -> Result<PlanResult, PluginError> {
        let Some(cfg) = dm::load(req)? else { return Ok(PlanResult::default()) };
        let errors = dm::validate(&cfg, req);
        if !errors.is_empty() {
            return Err(PluginError::invalid(errors));
        }
        Ok(PlanResult { plans: dm::plan(&cfg, req)?, warnings: Vec::new() })
    }

    /// Front end, current upstreams, fallback use and each backend's readiness (§9.10 status).
    fn probe(&self, req: &PluginRequest) -> Result<ProbeResult, PluginError> {
        let Some(cfg) = dm::load(req)? else { return Ok(ProbeResult::default()) };
        if !cfg.enabled {
            return Ok(ProbeResult::default());
        }
        let run = req.context.run_dir.trim_end_matches('/');
        let state: Value = std::fs::read_to_string(format!("{run}/dnsmasq-main.state"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        let listen: Vec<SocketAddr> = cfg.listen_addresses.iter().map(|a| SocketAddr::new(*a, cfg.port)).collect();
        let front = dm::frontend::frontend_answers(&listen, &cfg.probe_name);
        let ts = meduza_np_plugin_sdk::now_rfc3339();
        let mut detail = BTreeMap::new();
        detail.insert("fallbackActive".into(), state.get("fallbackActive").cloned().unwrap_or(Value::Null));
        detail.insert("fallbackVerified".into(), state.get("fallbackVerified").cloned().unwrap_or(Value::Null));
        detail.insert("upstreams".into(), state.get("upstreams").cloned().unwrap_or(Value::Null));
        let fallback_active = state.get("fallbackActive").and_then(Value::as_bool).unwrap_or(true);
        let mut out = vec![InstanceStatus {
            id: "dnsmasq/main".into(),
            kind: "dns-frontend".into(),
            state: match (&front, fallback_active && !cfg.backends.is_empty()) {
                (Err(_), _) => ServiceState::Down,
                (Ok(()), true) => ServiceState::Degraded,
                (Ok(()), false) => ServiceState::Up,
            },
            observed_at: Some(ts.clone()),
            reason: front
                .err()
                .map(|e| format!("front end does not answer: {e}"))
                .or_else(|| (fallback_active && !cfg.backends.is_empty()).then(|| "using fallback servers".into())),
            detail,
        }];
        for b in cfg.backends.iter().filter(|b| b.enabled) {
            let s = state.pointer(&format!("/backends/{}", b.id));
            let healthy = s.and_then(|s| s.get("healthy")).and_then(Value::as_bool).unwrap_or(false);
            let mut d = BTreeMap::new();
            d.insert("backend".into(), serde_json::to_value(b.kind).unwrap_or_default());
            d.insert("address".into(), Value::String(b.addr().to_string()));
            d.insert("priority".into(), Value::from(b.priority));
            out.push(InstanceStatus {
                id: format!("dnsmasq/main/{}", b.id),
                kind: "dns-backend".into(),
                state: if s.is_none() {
                    ServiceState::Unknown
                } else if healthy {
                    ServiceState::Up
                } else {
                    ServiceState::Down
                },
                observed_at: Some(ts.clone()),
                reason: s.and_then(|s| s.get("lastError")).and_then(Value::as_str).map(str::to_string),
                detail: d,
            });
        }
        Ok(ProbeResult { instances: out })
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("frontend") {
        dm::frontend::run(&args[2..]);
    }
    run(&Dnsmasq, env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}
