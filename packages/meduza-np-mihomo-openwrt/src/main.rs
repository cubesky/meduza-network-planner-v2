use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use meduza_np_mihomo_openwrt::{self as oc, openclash};
use meduza_np_plugin_sdk::{run, PlanResult, Plugin, PluginError, PluginRequest, ProbeResult, ValidateResult};
use meduza_np_protocol::report::{InstanceStatus, ServiceState};
use serde_json::{json, Value};

struct OpenClash;

impl Plugin for OpenClash {
    fn validate(&self, req: &PluginRequest) -> Result<ValidateResult, PluginError> {
        Ok(ValidateResult { errors: oc::load(req)?.map(|c| oc::validate(&c, req)).unwrap_or_default(), warnings: Vec::new() })
    }

    fn plan(&self, req: &PluginRequest) -> Result<PlanResult, PluginError> {
        let Some(cfg) = oc::load(req)? else { return Ok(PlanResult::default()) };
        let errors = oc::validate(&cfg, req);
        if !errors.is_empty() {
            return Err(PluginError::invalid(errors));
        }
        let mut warnings = Vec::new();
        if cfg.enabled
            && req.config.services.contains_key("dnsmasq")
            && openclash::CliUci::show().get("enable_redirect_dns").map(String::as_str) == Some("2")
        {
            warnings
                .push("OpenClash redirects DNS with firewall rules (enable_redirect_dns=2); it may bypass the dnsmasq front end".into());
        }
        Ok(PlanResult { plans: oc::plan(&cfg, req)?, warnings })
    }

    /// OpenClash/core/API state, the managed selection, subscriptions and (optionally) the OpenClash DNS listener.
    fn probe(&self, req: &PluginRequest) -> Result<ProbeResult, PluginError> {
        let Some(cfg) = oc::load(req)? else { return Ok(ProbeResult::default()) };
        if !cfg.enabled {
            return Ok(ProbeResult::default());
        }
        let ts = meduza_np_plugin_sdk::now_rfc3339();
        let state: Value = std::fs::read_to_string(format!("{}/mihomo-openwrt.state", req.context.run_dir.trim_end_matches('/')))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        let compatible = state.pointer("/openclash/compatible").and_then(Value::as_bool).unwrap_or(false);
        let api_ready = state.get("apiReady").and_then(Value::as_bool).unwrap_or(false);
        let selected = state.get("selected").and_then(Value::as_bool).unwrap_or(false);
        let mut detail = BTreeMap::new();
        detail.insert("openclashVersion".into(), state.pointer("/openclash/version").cloned().unwrap_or(Value::Null));
        for k in [
            "coreVersion",
            "coreType",
            "configPath",
            "selected",
            "enMode",
            "groups",
            "connections",
            "takenOver",
            "restartPending",
            "lastRestart",
        ] {
            detail.insert(k.into(), state.get(k).cloned().unwrap_or(Value::Null));
        }
        detail.insert("managedPath".into(), Value::String(oc::managed_path(&cfg)));
        let (st, reason) = if state.is_null() {
            (ServiceState::Starting, Some("adapter has not reported yet".to_string()))
        } else if !compatible {
            (ServiceState::Down, state.pointer("/openclash/reason").and_then(Value::as_str).map(str::to_string))
        } else if let Some(e) = state.get("lastError").and_then(Value::as_str) {
            (ServiceState::Degraded, Some(e.to_string()))
        } else if !api_ready {
            (ServiceState::Down, Some("OpenClash core/controller API not reachable".into()))
        } else if cfg.select && !selected {
            (ServiceState::Degraded, Some("OpenClash is not using the managed configuration".into()))
        } else {
            (ServiceState::Up, None)
        };
        let mut out = vec![InstanceStatus {
            id: oc::INSTANCE.into(),
            kind: "openclash".into(),
            state: st,
            observed_at: Some(ts.clone()),
            reason,
            detail,
        }];
        if cfg.dns_integration {
            // Only the DNS listener OpenClash itself provides; its port/rules stay OpenClash's.
            let port = state.get("dnsPort").and_then(Value::as_u64).unwrap_or(7874) as u16;
            let listen = SocketAddr::new(IpAddr::from([127, 0, 0, 1]), port);
            let r = meduza_np_plugin_sdk::dns::probe(listen, "example.com", Duration::from_secs(2));
            out.push(InstanceStatus {
                id: format!("{}/dns", oc::INSTANCE),
                kind: "openclash-dns".into(),
                state: if r.is_ok() { ServiceState::Up } else { ServiceState::Down },
                observed_at: Some(ts.clone()),
                reason: r.err(),
                detail: BTreeMap::from([("listen".to_string(), Value::String(listen.to_string()))]),
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
                id: format!("{}/{}", oc::INSTANCE, s.id),
                kind: "openclash-subscription".into(),
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

    fn action(&self, req: &PluginRequest) -> Result<Value, PluginError> {
        let cfg = oc::load(req)?;
        match req.params.get("action").and_then(Value::as_str) {
            Some("detect") => {
                let d = openclash::detect(Path::new("/"));
                let uci: BTreeMap<String, String> = openclash::CliUci::show()
                    .into_iter()
                    .filter(|(k, _)| openclash::STATUS_OPTIONS.contains(&k.as_str()) && k != "dashboard_password")
                    .collect();
                Ok(
                    json!({"openclash": d, "options": uci, "tested": {"min": openclash::TESTED_MIN, "maxExclusive": openclash::TESTED_MAX_EXCLUSIVE}}),
                )
            }
            Some("preview-config") => {
                let cfg = cfg.ok_or_else(|| PluginError::new("not-found", "mihomo-openwrt is not configured"))?;
                let text = std::fs::read_to_string(oc::managed_path(&cfg))
                    .map_err(|_| PluginError::new("not-found", "no managed configuration yet (subscription not downloaded)"))?;
                let v: Value = serde_norway::from_str(&text).map_err(|e| PluginError::new("internal", e.to_string()))?;
                Ok(json!({"path": oc::managed_path(&cfg), "config": oc::redact(v)}))
            }
            Some("reload-openclash") => {
                if !cfg.map(|c| c.enabled).unwrap_or(false) {
                    return Err(PluginError::new("conflict", "the adapter is disabled; OpenClash is not managed"));
                }
                let d = openclash::detect(Path::new("/"));
                if let Some(r) = d.reason {
                    return Err(PluginError::new("incompatible", r));
                }
                openclash::init_script("restart").map_err(|e| PluginError::new("failed", e))?;
                Ok(json!({"restarted": true}))
            }
            Some("restore-takeover") => {
                // While enabled the adapter would take over again at once; disabling runs the release itself.
                if cfg.map(|c| c.enabled).unwrap_or(false) {
                    return Err(PluginError::new(
                        "conflict",
                        "disable the instance to hand OpenClash back (the release runs automatically)",
                    ));
                }
                oc::runner::release_takeover(&req.context.state_dir, &mut openclash::CliUci).map_err(|e| PluginError::new("failed", e))
            }
            // Downloads every subscription again now (the running wrapper picks the trigger up within 10 s).
            Some("refresh-subscription") => {
                let path = oc::refresh_trigger(&req.context.run_dir);
                std::fs::write(&path, b"").map_err(|e| PluginError::new("io", format!("{path}: {e}")))?;
                Ok(json!({"scheduled": true}))
            }
            other => Err(PluginError::new("unsupported", format!("unknown action {other:?}"))),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("run") => oc::runner::run(&args[2..]),
        Some("release") => oc::runner::release(&args[2..]),
        _ => run(&OpenClash, env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
    }
}
