use std::collections::BTreeMap;
use std::time::Duration;

use meduza_np_mihomo as mh;
use meduza_np_plugin_sdk::{run, PlanResult, Plugin, PluginError, PluginRequest, ProbeResult, ValidateResult};
use meduza_np_protocol::report::{InstanceStatus, ServiceState};
use serde_json::{json, Value};

struct Mihomo;

impl Plugin for Mihomo {
    fn validate(&self, req: &PluginRequest) -> Result<ValidateResult, PluginError> {
        Ok(ValidateResult { errors: mh::load(req)?.map(|c| mh::validate(&c, req)).unwrap_or_default(), warnings: Vec::new() })
    }

    fn plan(&self, req: &PluginRequest) -> Result<PlanResult, PluginError> {
        let Some(cfg) = mh::load(req)? else { return Ok(PlanResult::default()) };
        let errors = mh::validate(&cfg, req);
        if !errors.is_empty() {
            return Err(PluginError::invalid(errors));
        }
        Ok(PlanResult { plans: mh::plan(&cfg, req)?, warnings: Vec::new() })
    }

    /// Layered readiness (§9.7): API, DNS (real query), TPROXY rules, subscriptions — each reported separately.
    fn probe(&self, req: &PluginRequest) -> Result<ProbeResult, PluginError> {
        let Some(cfg) = mh::load(req)? else { return Ok(ProbeResult::default()) };
        if !cfg.enabled {
            return Ok(ProbeResult::default());
        }
        let ts = meduza_np_plugin_sdk::now_rfc3339();
        let state: Value = std::fs::read_to_string(format!("{}/mihomo-main.state", req.context.run_dir.trim_end_matches('/')))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        let api_ready = state.get("apiReady").and_then(Value::as_bool).unwrap_or(false);
        let mut detail = BTreeMap::new();
        for k in ["version", "groups", "connections", "proxyServerSet"] {
            detail.insert(k.into(), state.get(k).cloned().unwrap_or(Value::Null));
        }
        detail.insert("mode".into(), serde_json::to_value(cfg.mode).unwrap_or_default());
        detail.insert("provider".into(), json!(req.providers.get(mh::SLOT).map(|p| p.source.clone())));
        let mut out = vec![InstanceStatus {
            id: "mihomo/main".into(),
            kind: "mihomo".into(),
            state: if api_ready {
                ServiceState::Up
            } else if state.is_null() {
                ServiceState::Starting
            } else {
                ServiceState::Down
            },
            observed_at: Some(ts.clone()),
            reason: (!api_ready).then(|| "controller API not ready".into()),
            detail,
        }];
        if cfg.dns.enabled {
            // API/proxy readiness does not imply DNS readiness (§9.10).
            let r = meduza_np_plugin_sdk::dns::probe(cfg.dns.listen, "example.com", Duration::from_secs(2));
            out.push(InstanceStatus {
                id: "mihomo/main/dns".into(),
                kind: "mihomo-dns".into(),
                state: if r.is_ok() { ServiceState::Up } else { ServiceState::Down },
                observed_at: Some(ts.clone()),
                reason: r.err(),
                detail: BTreeMap::from([("listen".to_string(), Value::String(cfg.dns.listen.to_string()))]),
            });
        }
        for s in &cfg.subscriptions {
            let st = state.pointer(&format!("/subscriptions/{}", s.id));
            let status = st.and_then(|x| x.get("status")).and_then(Value::as_str).unwrap_or("pending");
            let mut d = BTreeMap::new();
            for k in ["updatedAt", "lastError", "overriddenKeys"] {
                if let Some(v) = st.and_then(|x| x.get(k)) {
                    d.insert(k.into(), v.clone());
                }
            }
            d.insert("active".into(), Value::Bool(cfg.active_subscription.as_deref() == Some(s.id.as_str())));
            out.push(InstanceStatus {
                id: format!("mihomo/main/{}", s.id),
                kind: "mihomo-subscription".into(),
                state: match status {
                    "ok" => ServiceState::Up,
                    "stale" => ServiceState::Degraded,
                    "failed" => ServiceState::Down,
                    _ => ServiceState::Pending,
                },
                observed_at: Some(ts.clone()),
                reason: None,
                detail: d,
            });
        }
        Ok(ProbeResult { instances: out })
    }

    /// `preview-config`: the normalized configuration the node runs, secrets redacted.
    fn action(&self, req: &PluginRequest) -> Result<Value, PluginError> {
        match req.params.get("action").and_then(Value::as_str) {
            Some("preview-config") => {
                let path = format!("{}/mihomo-final.yaml", req.context.run_dir.trim_end_matches('/'));
                let text = std::fs::read_to_string(&path)
                    .map_err(|_| PluginError::new("not-found", "no final configuration yet (subscription not downloaded)"))?;
                let mut v: Value = serde_norway::from_str(&text).map_err(|e| PluginError::new("internal", e.to_string()))?;
                if let Some(o) = v.as_object_mut() {
                    o.insert("secret".into(), Value::String("<redacted>".into()));
                    if let Some(proxies) = o.get_mut("proxies").and_then(Value::as_array_mut) {
                        for p in proxies.iter_mut().filter_map(Value::as_object_mut) {
                            for k in ["password", "uuid", "private-key", "psk", "auth-str"] {
                                if p.contains_key(k) {
                                    p.insert(k.into(), Value::String("<redacted>".into()));
                                }
                            }
                        }
                    }
                }
                Ok(json!({"config": v}))
            }
            // Downloads every subscription again now (the running wrapper picks the trigger up within 10 s).
            Some("refresh-subscription") => {
                let path = mh::refresh_trigger(&req.context.run_dir);
                std::fs::write(&path, b"").map_err(|e| PluginError::new("io", format!("{path}: {e}")))?;
                Ok(json!({"scheduled": true}))
            }
            other => Err(PluginError::new("unsupported", format!("unknown action {other:?}"))),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("run") {
        mh::runner::run(&args[2..]);
    }
    run(&Mihomo, env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}
