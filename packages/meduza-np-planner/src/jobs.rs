//! Execution of manager jobs (§7.3 `GET /nodes/self/jobs`): bounded, idempotent by job id, with results
//! reported back. Jobs run one at a time and never hold the apply lock longer than an apply would.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meduza_np_protocol::api::{Job, JobResult};
use serde_json::{json, Value};

use crate::manager::{CallError, ManagerClient};
use crate::sync::Planner;

static DONE: Mutex<Option<BTreeSet<String>>> = Mutex::new(None);

fn already_done(id: &str) -> bool {
    DONE.lock().unwrap().as_ref().map(|s| s.contains(id)).unwrap_or(false)
}

fn mark_done(id: &str) {
    let mut g = DONE.lock().unwrap();
    let set = g.get_or_insert_with(BTreeSet::new);
    if set.len() > 512 {
        set.clear();
    }
    set.insert(id.to_string());
}

pub async fn run_pending(p: &Arc<Planner>, client: &Arc<ManagerClient>) -> Result<(), CallError> {
    let jobs = client.jobs().await?;
    for job in jobs {
        if already_done(&job.job_id) {
            continue;
        }
        let expired = meduza_np_plugin_sdk::rfc3339_to_unix(&job.expires_at)
            .map(|t| t < std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0))
            .unwrap_or(false);
        let result = if expired {
            JobResult { job_id: job.job_id.clone(), ok: false, result: Value::Null, error_code: Some("expired".into()), message: None }
        } else {
            match tokio::time::timeout(Duration::from_secs(120), execute(p, &job)).await {
                Ok(Ok(v)) => JobResult { job_id: job.job_id.clone(), ok: true, result: v, error_code: None, message: None },
                Ok(Err((code, msg))) => {
                    JobResult { job_id: job.job_id.clone(), ok: false, result: Value::Null, error_code: Some(code), message: Some(msg) }
                }
                Err(_) => JobResult {
                    job_id: job.job_id.clone(),
                    ok: false,
                    result: Value::Null,
                    error_code: Some("timeout".into()),
                    message: None,
                },
            }
        };
        match client.job_result(&result).await {
            Ok(()) | Err(CallError::Rejected { .. }) => mark_done(&job.job_id),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

async fn execute(p: &Arc<Planner>, job: &Job) -> Result<Value, (String, String)> {
    let err = |c: &str, m: String| (c.to_string(), m);
    match job.kind.as_str() {
        "update-check" | "update-apply" => crate::updater::request(&p.paths, job).await,
        "restart-instance" | "start-instance" | "stop-instance" => {
            let instance = job.instance.clone().ok_or_else(|| err("invalid-params", "instance required".into()))?;
            let applied = crate::state::AppliedState::load(&p.paths).map_err(|e| err("internal", e.to_string()))?;
            let inst = applied.instances.get(&instance).ok_or_else(|| err("not-found", format!("{instance} is not applied here")))?;
            // `params.process`: only that process of the instance (a restart button next to one process).
            let only = job.params.get("process").and_then(Value::as_str);
            if let Some(p) = only {
                if !inst.processes.contains_key(p) {
                    return Err(err("not-found", format!("{p} is not a process of {instance}")));
                }
            }
            for proc_id in inst.processes.keys().filter(|id| only.is_none_or(|p| p == id.as_str())) {
                match job.kind.as_str() {
                    "restart-instance" | "start-instance" => {
                        if !p.engine.supervisor.restart(proc_id).await {
                            return Err(err("not-running", format!("{proc_id} is not supervised")));
                        }
                    }
                    _ => p.engine.supervisor.stop(proc_id).await,
                }
            }
            Ok(json!({"instance": instance}))
        }
        "generate-keypair" => {
            let secret_id =
                job.params.get("secretId").and_then(Value::as_str).ok_or_else(|| err("invalid-params", "secretId required".into()))?;
            let kind = job.params.get("keyKind").and_then(Value::as_str).ok_or_else(|| err("invalid-params", "keyKind required".into()))?;
            let replace = job.params.get("replace").and_then(Value::as_bool).unwrap_or(false);
            let result = match p.engine.keystore.generate(secret_id, kind, replace) {
                Ok(public) => Ok(json!({"publicKey": public})),
                Err(e) if e.kind() == std::io::ErrorKind::Unsupported => run_action(p, job, "generate-keypair").await,
                Err(e) => Err(err("keygen-failed", e.to_string())),
            };
            if result.is_ok() {
                p.reapply_after_key(secret_id).await;
            }
            result
        }
        "run-action" => {
            let action = job.params.get("action").and_then(Value::as_str).unwrap_or_default().to_string();
            run_action(p, job, &action).await
        }
        // the redacted support bundle; bounded below the manager's job result limit
        "collect-diagnostics" => Ok(crate::support::bundle(p).await),
        "sync-now" => {
            p.wake.notify_one();
            Ok(json!({"scheduled": true}))
        }
        other => Err(err("unsupported", format!("unknown job kind {other}"))),
    }
}

/// Calls a package action declared in its UI descriptor (`action` operation of the plugin).
async fn run_action(p: &Arc<Planner>, job: &Job, action: &str) -> Result<Value, (String, String)> {
    let err = |c: &str, m: String| (c.to_string(), m);
    let package = job.package.clone().ok_or_else(|| err("invalid-params", "package required".into()))?;
    let registry = p.registry();
    let pkg =
        registry.get(&package).filter(|x| x.registered()).ok_or_else(|| err("not-installed", format!("{package} is not registered")))?;
    let program = registry.plugin_path(pkg).ok_or_else(|| err("not-installed", "plugin executable missing".into()))?;
    let lg = crate::state::load_last_good(&p.paths).ok().flatten();
    let config = match lg {
        Some(lg) => lg.config,
        None => serde_json::from_value(json!({"schemaVersion": 1, "revision": "rev-000000", "node": {"id": "unconfigured"}})).unwrap(),
    };
    let mut providers = std::collections::BTreeMap::new();
    if let Some(slot) = &pkg.manifest.provider_slot {
        if let Some(rp) = registry.providers.get(slot) {
            providers.insert(slot.clone(), rp.clone());
        }
    }
    let mut params = job.params.clone();
    if let Value::Object(map) = &mut params {
        map.insert("action".into(), Value::String(action.to_string()));
    }
    let req = meduza_np_plugin_sdk::PluginRequest {
        protocol_version: meduza_np_protocol::PLUGIN_PROTOCOL_VERSION,
        request_id: job.job_id.clone(),
        operation: "action".into(),
        config,
        instance: job.instance.clone(),
        providers,
        context: meduza_np_plugin_sdk::PluginContext {
            generation_root: p.paths.current_generation_link().to_string_lossy().into_owned(),
            state_dir: p.paths.state.to_string_lossy().into_owned(),
            run_dir: p.paths.run.to_string_lossy().into_owned(),
            libexec_dir: p.paths.libexec.to_string_lossy().into_owned(),
            platform: p.platform.clone(),
            capabilities: registry.capabilities(),
        },
        params,
    };
    let result = p.engine.runner.call(&program, &req).await.map_err(|e| err(&e.code(), e.to_string()))?;
    // Provider preparation (check/activate/download) changes what the registry resolves.
    if pkg.manifest.role == meduza_np_protocol::manifest::PackageRole::BinaryProvider {
        p.rescan().await;
    }
    // Plugin-generated private keys are stored locally; only the public part goes back to the manager.
    if let (Some(secret_id), Some(private)) =
        (job.params.get("secretId").and_then(Value::as_str), result.get("privateKey").and_then(Value::as_str))
    {
        let kind = job.params.get("keyKind").and_then(Value::as_str).unwrap_or("text");
        let public = result.get("publicKey").and_then(Value::as_str).map(str::to_string);
        p.engine
            .keystore
            .put(secret_id, &crate::keystore::StoredKey { kind: kind.into(), value: private.into(), public_value: public.clone() })
            .map_err(|e| err("keystore", e.to_string()))?;
        return Ok(json!({"publicKey": public}));
    }
    Ok(result)
}
