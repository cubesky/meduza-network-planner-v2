use std::collections::BTreeMap;
use std::time::Duration;

use meduza_np_plugin_sdk::{run, run_status_command, PlanResult, Plugin, PluginError, PluginRequest, ProbeResult, ValidateResult};
use meduza_np_wireguard as wg;
use serde_json::{json, Value};

struct WireGuard;

impl Plugin for WireGuard {
    fn validate(&self, req: &PluginRequest) -> Result<ValidateResult, PluginError> {
        let cfg = wg::load(req)?;
        Ok(ValidateResult { errors: wg::validate(&cfg, req), warnings: Vec::new() })
    }

    fn plan(&self, req: &PluginRequest) -> Result<PlanResult, PluginError> {
        let cfg = wg::load(req)?;
        let errors = wg::validate(&cfg, req);
        if !errors.is_empty() {
            return Err(PluginError::invalid(errors));
        }
        Ok(PlanResult { plans: wg::plan(&cfg, req)?, warnings: Vec::new() })
    }

    fn probe(&self, req: &PluginRequest) -> Result<ProbeResult, PluginError> {
        let cfg = wg::load(req)?;
        let wg_bin = req.provider(wg::SLOT)?.path.clone();
        let mut dumps = BTreeMap::new();
        for inst in cfg.instances.iter().filter(|i| i.enabled) {
            let dev = inst.device();
            let out = run_status_command(&wg_bin, &["show", &dev, "dump"], Duration::from_secs(5), 1024 * 1024)
                .map_err(|e| e.to_string())
                .and_then(|o| if o.status == Some(0) { Ok(o.stdout) } else { Err(o.stderr.trim().to_string()) });
            dumps.insert(dev, out);
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let mut instances = wg::statuses(&cfg, &dumps, now);
        wg::endpoints::attach_statuses(&cfg, &mut instances, std::path::Path::new(&req.context.run_dir), now);
        Ok(ProbeResult { instances })
    }

    fn action(&self, req: &PluginRequest) -> Result<Value, PluginError> {
        match req.params.get("action").and_then(Value::as_str) {
            Some("derive-public-key") => {
                let private = req
                    .params
                    .get("privateKey")
                    .and_then(Value::as_str)
                    .ok_or_else(|| PluginError::new("invalid-params", "privateKey required"))?;
                let public =
                    wg::public_from_private(private).ok_or_else(|| PluginError::new("invalid-params", "not a WireGuard private key"))?;
                Ok(json!({"publicKey": public}))
            }
            Some("export-wg-quick") => {
                let cfg = wg::load(req)?;
                let id = req
                    .instance
                    .as_deref()
                    .and_then(|i| i.strip_prefix("wireguard/"))
                    .ok_or_else(|| PluginError::new("invalid-params", "instance required"))?;
                let inst =
                    cfg.instances.iter().find(|i| i.id.as_str() == id).ok_or_else(|| PluginError::new("not-found", "no such interface"))?;
                Ok(json!({"format": "wg-quick", "sensitive": true, "content": wg::render_wg_quick_export(inst, req)?}))
            }
            other => Err(PluginError::new("unsupported", format!("unknown action {other:?}"))),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("resolve-endpoint") => Some(
            args.get(2)
                .ok_or_else(|| "endpoint required".into())
                .and_then(|ep| wg::endpoints::resolve(ep))
                .map(|address| println!("{address}")),
        ),
        Some("watch-endpoints") => Some(match (args.get(2), args.get(3)) {
            (Some(spec), Some(run)) => wg::endpoints::watch(std::path::Path::new(spec), std::path::Path::new(run)),
            _ => Err("endpoint spec and run directory required".into()),
        }),
        _ => None,
    };
    if let Some(result) = result {
        if let Err(error) = result {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    run(&WireGuard, env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}
