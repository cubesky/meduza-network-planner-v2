//! Test plugin (not shipped), registered in tests under the `meduza-np-avahi` package name:
//! `services.avahi.instances[] = {id, crash?, device?, version?}` → one supervised daemon per instance using the
//! `avahi.daemon` provider slot, one config file and an optional link resource.

use meduza_np_plugin_sdk::{run, PlanResult, Plugin, PluginError, PluginRequest, ValidateResult};
use meduza_np_protocol::plan::{GenerationPlan, NetworkResource, PlannedFile, ProcessSpec, ProcessStartupPolicy, RestartPolicy};
use serde_json::Value;

struct Fake;

fn instances(req: &PluginRequest) -> Vec<Value> {
    req.config.services.get("avahi").and_then(|v| v.get("instances")).and_then(Value::as_array).cloned().unwrap_or_default()
}

impl Plugin for Fake {
    fn validate(&self, req: &PluginRequest) -> Result<ValidateResult, PluginError> {
        let mut r = ValidateResult::default();
        for (i, inst) in instances(req).iter().enumerate() {
            if inst.get("id").and_then(Value::as_str).is_none() {
                r.errors.push(meduza_np_protocol::ValidationError::new(format!("/instances/{i}/id"), "id required"));
            }
        }
        Ok(r)
    }

    fn plan(&self, req: &PluginRequest) -> Result<PlanResult, PluginError> {
        let log = req.params.get("log").and_then(Value::as_str).map(str::to_string).or_else(|| std::env::var("MNP_FAKE_TOOL_LOG").ok());
        let mut plans = Vec::new();
        for inst in instances(req) {
            let id = inst["id"].as_str().unwrap_or_default().to_string();
            let crash = inst.get("crash").and_then(Value::as_bool).unwrap_or(false);
            let connection = inst.get("connection").and_then(Value::as_bool).unwrap_or(false);
            let version = inst.get("version").and_then(Value::as_u64).unwrap_or(1);
            let mut argv =
                vec![if crash { "crash".to_string() } else { "daemon".to_string() }, format!("${{GENERATION_ROOT}}/avahi/{id}/app.conf")];
            if let Some(path) = inst.get("readyFile").and_then(Value::as_str) {
                argv[0] = "wait-for-peer".into();
                argv.push(format!("--ready-file={path}"));
            }
            if let Some(l) = inst.get("log").and_then(Value::as_str).map(str::to_string).or(log.clone()) {
                argv.push(format!("--log={l}"));
            }
            let mut resources = Vec::new();
            if let Some(dev) = inst.get("device").and_then(Value::as_str) {
                resources.push(NetworkResource::Link { device: dev.into(), link_type: "dummy".into(), mtu: None, up: true });
            }
            plans.push(GenerationPlan {
                protocol_version: 1,
                request_id: req.request_id.clone(),
                instance: format!("avahi/{id}"),
                files: vec![PlannedFile {
                    path: format!("avahi/{id}/app.conf"),
                    mode: "0640".into(),
                    content: format!("version={version}\n"),
                    sensitive: false,
                }],
                processes: vec![ProcessSpec {
                    id: format!("avahi/{id}"),
                    provider_slot: Some("avahi.daemon".into()),
                    program: None,
                    argv,
                    env: Default::default(),
                    restart: if connection { RestartPolicy::Always } else { RestartPolicy::OnFailure },
                    startup: if connection { ProcessStartupPolicy::Connection } else { ProcessStartupPolicy::Required },
                    stop_timeout_seconds: 2,
                    reload_signal: None,
                    restart_digest: None,
                    user: None,
                    bound_to: None,
                }],
                resources,
                depends_on: vec![],
                probes: vec![],
                checks: vec![],
                reload: vec![],
            });
        }
        Ok(PlanResult { plans, warnings: vec![] })
    }
}

fn main() {
    run(&Fake, "fake-plugin", "0.0.0")
}
