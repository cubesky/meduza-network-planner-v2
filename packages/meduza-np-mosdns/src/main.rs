use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use meduza_np_mosdns as m;
use meduza_np_plugin_sdk::{run, PlanResult, Plugin, PluginError, PluginRequest, ProbeResult, ValidateResult};
use meduza_np_protocol::report::{InstanceStatus, ServiceState};
use serde_json::Value;

struct Mosdns;

impl Plugin for Mosdns {
    fn validate(&self, req: &PluginRequest) -> Result<ValidateResult, PluginError> {
        Ok(ValidateResult { errors: m::load(req)?.map(|c| m::validate(&c, req)).unwrap_or_default(), warnings: Vec::new() })
    }

    fn plan(&self, req: &PluginRequest) -> Result<PlanResult, PluginError> {
        let Some(cfg) = m::load(req)? else { return Ok(PlanResult::default()) };
        let errors = m::validate(&cfg, req);
        if !errors.is_empty() {
            return Err(PluginError::invalid(errors));
        }
        Ok(PlanResult { plans: m::plan(&cfg, req)?, warnings: Vec::new() })
    }

    /// Distinguishes "running with all rules", "partial rule failure", "old rules in use" and "DNS down".
    fn probe(&self, req: &PluginRequest) -> Result<ProbeResult, PluginError> {
        let Some(cfg) = m::load(req)? else { return Ok(ProbeResult::default()) };
        if !cfg.enabled {
            return Ok(ProbeResult::default());
        }
        let ts = meduza_np_plugin_sdk::now_rfc3339();
        let dns = meduza_np_plugin_sdk::dns::probe(SocketAddr::new(cfg.listen, cfg.port), &cfg.probe_name, Duration::from_secs(2));
        let state: Value = std::fs::read_to_string(format!("{}/mosdns-main.state", req.context.run_dir.trim_end_matches('/')))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        let rule_statuses: Vec<String> = state
            .get("rules")
            .and_then(Value::as_object)
            .map(|o| o.values().filter_map(|v| v.get("status").and_then(Value::as_str).map(str::to_string)).collect())
            .unwrap_or_default();
        let degraded = rule_statuses.iter().any(|s| s != "ok");
        let mut detail = BTreeMap::new();
        detail.insert("listen".into(), Value::String(format!("{}:{}", cfg.listen, cfg.port)));
        detail.insert("rules".into(), state.get("rules").cloned().unwrap_or(Value::Null));
        let mut out = vec![InstanceStatus {
            id: "mosdns/main".into(),
            kind: "mosdns".into(),
            state: match (&dns, degraded) {
                (Err(_), _) => ServiceState::Down,
                (Ok(()), true) => ServiceState::Degraded,
                (Ok(()), false) => ServiceState::Up,
            },
            observed_at: Some(ts.clone()),
            reason: dns
                .err()
                .map(|e| format!("DNS not answering: {e}"))
                .or_else(|| degraded.then(|| "some rule files failed; previous or empty rules in use".into())),
            detail,
        }];
        for r in &cfg.rule_files {
            let s = state.pointer(&format!("/rules/{}", r.id));
            let status = s.and_then(|s| s.get("status")).and_then(Value::as_str).unwrap_or("pending");
            let mut d = BTreeMap::new();
            d.insert("category".into(), serde_json::to_value(r.category).unwrap_or_default());
            d.insert("ruleStatus".into(), Value::String(status.into()));
            for k in ["updatedAt", "sha256", "lastError"] {
                if let Some(v) = s.and_then(|s| s.get(k)) {
                    d.insert(k.into(), v.clone());
                }
            }
            out.push(InstanceStatus {
                id: format!("mosdns/main/{}", r.id),
                kind: "mosdns-rules".into(),
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
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("run") {
        m::rules::run(&args[2..]);
    }
    run(&Mosdns, env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;

    #[test]
    fn wildcard_listener_reports_up_after_a_real_dns_answer() {
        let server = UdpSocket::bind("0.0.0.0:0").unwrap();
        server.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let port = server.local_addr().unwrap().port();
        let t = std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            let (n, from) = server.recv_from(&mut buf).unwrap();
            let mut resp = buf[..n].to_vec();
            resp[2] = 0x81;
            resp[3] = 0x80;
            server.send_to(&resp, from).unwrap();
        });
        let mut req =
            meduza_np_plugin_sdk::test_request("probe", serde_json::json!({"services":{"mosdns":{"listen":"0.0.0.0", "port":port}}}));
        // Avoid reading a real node's state file; this fixture has no remote rules.
        req.context.run_dir = std::env::temp_dir()
            .join(format!(
                "meduza-mosdns-probe-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
            ))
            .to_str()
            .unwrap()
            .into();
        let result = Mosdns.probe(&req);
        t.join().unwrap();
        let report = result.unwrap();
        assert_eq!(report.instances[0].state, ServiceState::Up);
        assert_eq!(report.instances[0].reason, None);
        assert_eq!(report.instances[0].detail["listen"], format!("0.0.0.0:{port}"));
    }
}
