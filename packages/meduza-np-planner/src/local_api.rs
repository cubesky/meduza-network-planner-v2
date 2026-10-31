//! Local control API over Unix sockets for CLI, dashboards and the updater (§12.3, §13.1). JSON lines framing
//! (`meduza_np_protocol::local`). Mutating methods require root, the planner's own uid or the `meduza-np` group.

use std::sync::Arc;
use std::time::{Duration, Instant};

use meduza_np_protocol::local::{
    method_is_mutating, LocalRequest, LocalResponse, MaintenanceRequest, LOCAL_PROTOCOL_VERSION, PLANNER_METHODS,
};
use meduza_np_protocol::report::MaintenanceStatus;
use serde_json::{json, Value};

use crate::sync::Planner;

/// Health and dependency diagnostics (CLI `doctor`, diagnostics job).
pub fn doctor(p: &Arc<Planner>) -> Value {
    let registry = p.registry();
    let mut checks = Vec::new();
    let mut push = |id: &str, ok: bool, detail: String| checks.push(json!({"check": id, "ok": ok, "detail": detail}));
    let settings = crate::settings::ManagerSettings::load(&p.paths.manager_settings()).ok().flatten();
    push(
        "enrolled",
        settings.is_some(),
        settings.as_ref().map(|s| format!("{} as {}", s.manager_url, s.node_id)).unwrap_or_else(|| "not enrolled".into()),
    );
    for pkg in registry.packages.values() {
        push(&format!("package:{}", pkg.manifest.package), pkg.registered(), pkg.reason.clone().unwrap_or_else(|| "registered".into()));
    }
    for (slot, why) in &registry.provider_errors {
        push(&format!("provider:{slot}"), false, why.clone());
    }
    for (slot, prov) in &registry.providers {
        push(&format!("provider:{slot}"), true, format!("{} ({})", prov.path, prov.source));
    }
    for tool in ["ip", "nft"] {
        let found = crate::platform::which(tool);
        push(&format!("tool:{tool}"), found.is_some(), found.map(|f| f.display().to_string()).unwrap_or_else(|| "not found".into()));
    }
    let free = meduza_np_runtime::fsutil::free_space(&p.paths.state).unwrap_or(0);
    push("state-disk", free > 2 * 1024 * 1024, format!("{} KiB free in {}", free / 1024, p.paths.state.display()));
    let st = p.status.lock().unwrap();
    push("sync", st.last_error.is_none(), st.last_error.clone().unwrap_or_else(|| format!("{:?}", st.state)));
    if let Some(c) = p.client() {
        let b = c.gate.lock().unwrap().status(Instant::now());
        push("backoff", b.failure_level == 0, format!("level {}, next attempt in {:?} ms", b.failure_level, b.next_attempt_in_ms));
    }
    Value::Array(checks)
}

/// Dispatches one request. `authorized` = peer may call mutating methods.
pub async fn dispatch(p: &Arc<Planner>, req: LocalRequest, authorized: bool) -> LocalResponse {
    if req.protocol != LOCAL_PROTOCOL_VERSION {
        return LocalResponse::err(req.id, "invalid-params", format!("protocol {} not supported", req.protocol));
    }
    let Some(mutating) = method_is_mutating(PLANNER_METHODS, &req.method) else {
        return LocalResponse::err(req.id, "not-found", format!("unknown method {}", req.method));
    };
    if mutating && !authorized {
        return LocalResponse::err(req.id, "forbidden", "this method needs root or the meduza-np group");
    }
    let id = req.id;
    let ok = |v: Value| LocalResponse::ok(id, v);
    match req.method.as_str() {
        "status" | "health" => ok(serde_json::to_value(crate::reporter::build(p)).unwrap_or_default()),
        "features" => {
            let (running, healthy) = p.running_packages();
            let reg = p.registry();
            let mut features = reg.features(&p.enabled_services(), &running, &healthy);
            reg.mark_network_scope(&mut features, crate::platform::network_mode(&p.platform));
            ok(serde_json::to_value(features).unwrap_or_default())
        }
        "packages" => {
            let reg = p.registry();
            ok(json!({"packages": crate::reporter::packages(p), "providers": reg.providers, "providerErrors": reg.provider_errors}))
        }
        "instances" => {
            let applied = crate::state::AppliedState::load(&p.paths).unwrap_or_default();
            let owners: serde_json::Map<String, Value> = applied
                .instances
                .iter()
                .map(|(id, a)| (id.clone(), json!({"package": a.package, "processes": a.processes.keys().collect::<Vec<_>>()})))
                .collect();
            ok(json!({"processes": p.engine.supervisor.statuses(), "probes": p.status.lock().unwrap().probes, "applied": owners}))
        }
        "config.current" => match crate::state::load_last_good(&p.paths) {
            Ok(Some(lg)) => ok(json!({"digest": lg.digest, "config": crate::support::redact_value(lg.raw)})),
            Ok(None) => ok(Value::Null),
            Err(e) => LocalResponse::err(id, "internal", e.to_string()),
        },
        "logs" => {
            let n = req.params.get("lines").and_then(Value::as_u64).unwrap_or(100).min(500) as usize;
            ok(json!(crate::log::tail(n)))
        }
        "doctor" => ok(doctor(p)),
        "support.bundle" => ok(crate::support::bundle(p).await),
        "validate" | "plan.preview" => {
            let lg = match crate::state::load_last_good(&p.paths) {
                Ok(Some(lg)) => lg,
                Ok(None) => return LocalResponse::err(id, "not-found", "no configuration applied yet"),
                Err(e) => return LocalResponse::err(id, "internal", e.to_string()),
            };
            match p.engine.prepare(&lg.raw, &lg.digest, &p.registry(), &p.platform).await {
                Ok(prepared) => {
                    let plans: Vec<Value> = prepared
                        .plans
                        .iter()
                        .map(|(pkg, plan)| {
                            json!({
                                "package": pkg,
                                "instance": plan.instance,
                                "files": plan.files.iter().map(|f| json!({"path": f.path, "mode": f.mode, "bytes": f.content.len(), "sensitive": f.sensitive,
                                    "content": if f.sensitive { Value::Null } else { Value::String(f.content.clone()) }})).collect::<Vec<_>>(),
                                "processes": plan.processes,
                                "resources": plan.resources,
                                "dependsOn": plan.depends_on,
                            })
                        })
                        .collect();
                    ok(
                        json!({"valid": true, "revision": prepared.revision, "warnings": prepared.warnings, "plans": if req.method == "plan.preview" { Value::Array(plans) } else { Value::Null }}),
                    )
                }
                Err(r) => ok(json!({"valid": false, "code": r.code, "message": r.message, "details": r.details})),
            }
        }
        "enroll" => enroll(p, id, &req.params).await,
        "sync.now" => {
            if let Some(c) = p.client() {
                c.gate.lock().unwrap().clear_credential_error();
            }
            p.wake.notify_one();
            ok(json!({"scheduled": true}))
        }
        "instance.start" | "instance.stop" | "instance.restart" => {
            let Some(instance) = req.params.get("instance").and_then(Value::as_str) else {
                return LocalResponse::err(id, "invalid-params", "instance required");
            };
            let applied = crate::state::AppliedState::load(&p.paths).unwrap_or_default();
            let Some(inst) = applied.instances.get(instance) else {
                return LocalResponse::err(id, "not-found", format!("{instance} is not applied"));
            };
            // Manual stop/start only holds during maintenance; otherwise the next reconcile restores it.
            if req.method == "instance.stop" && p.status.lock().unwrap().maintenance.is_none() {
                return LocalResponse::err(id, "maintenance", "start a maintenance window first, otherwise reconcile would undo the stop");
            }
            for proc_id in inst.processes.keys() {
                match req.method.as_str() {
                    "instance.stop" => p.engine.supervisor.stop(proc_id).await,
                    _ => {
                        if !p.engine.supervisor.restart(proc_id).await {
                            p.wake.notify_one();
                        }
                    }
                }
            }
            info!("{instance}: {} requested locally", req.method);
            ok(json!({"instance": instance}))
        }
        "maintenance.status" => ok(serde_json::to_value(&p.status.lock().unwrap().maintenance).unwrap_or_default()),
        "maintenance.start" | "maintenance.lock" | "apply.pause" => {
            let mut m: MaintenanceRequest = match serde_json::from_value(req.params.clone()) {
                Ok(m) => m,
                Err(e) if req.method == "apply.pause" => {
                    let _ = e;
                    MaintenanceRequest { reason: "apply paused".into(), duration_seconds: 3600, pause_apply: true, stop_instances: vec![] }
                }
                Err(e) => return LocalResponse::err(id, "invalid-params", e.to_string()),
            };
            if req.method != "maintenance.start" {
                m.pause_apply = true;
            }
            let seconds = m.duration_seconds.clamp(60, 24 * 3600);
            let until_unix =
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) + u64::from(seconds);
            let by = if req.method == "maintenance.lock" { "updater" } else { "local" };
            let status = MaintenanceStatus {
                active: true,
                reason: m.reason.chars().take(200).collect(),
                by: by.into(),
                until: meduza_np_plugin_sdk::unix_to_rfc3339(until_unix),
                apply_paused: m.pause_apply,
            };
            {
                let mut st = p.status.lock().unwrap();
                st.maintenance = Some(status.clone());
                st.maintenance_until = Some(Instant::now() + Duration::from_secs(seconds.into()));
            }
            let _ = meduza_np_runtime::fsutil::write_json_atomic(&p.paths.maintenance(), &status, 0o600);
            for inst in &m.stop_instances {
                if let Some(a) = crate::state::AppliedState::load(&p.paths).unwrap_or_default().instances.get(inst) {
                    for proc_id in a.processes.keys() {
                        p.engine.supervisor.stop(proc_id).await;
                    }
                }
            }
            info!("maintenance started by {by} for {seconds}s: {}", status.reason);
            ok(serde_json::to_value(status).unwrap_or_default())
        }
        "maintenance.stop" | "maintenance.unlock" | "apply.resume" => {
            p.end_maintenance();
            ok(json!({"active": false}))
        }
        "drain" => {
            // Updater asks to drain before replacing binaries: stop instances listed (Anycast withdrawal is the
            // FRR plugin's job via its maintenance-aware plan).
            let instances: Vec<String> =
                req.params.get("instances").and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
            let applied = crate::state::AppliedState::load(&p.paths).unwrap_or_default();
            for inst in &instances {
                if let Some(a) = applied.instances.get(inst) {
                    for proc_id in a.processes.keys() {
                        p.engine.supervisor.stop(proc_id).await;
                    }
                }
            }
            ok(json!({"drained": instances}))
        }
        "registry.rescan" => {
            p.rescan().await;
            ok(serde_json::to_value(crate::reporter::packages(p)).unwrap_or_default())
        }
        "dashboard.login" | "dashboard.exchange" => {
            // Relay for meduza-np-dashboard: the node session authenticates it to the manager, which binds the
            // login to this node. Only the nonce *hash* leaves the node at `login`.
            let Some(client) = p.client() else {
                return LocalResponse::err(
                    id,
                    "unavailable",
                    "not enrolled with a manager; use a local login link (`meduza-np dashboard-link`)",
                );
            };
            let step = req.method.trim_start_matches("dashboard.");
            match client.dashboard(step, &req.params).await {
                Ok(v) => ok(v),
                Err(crate::manager::CallError::Rejected { status, body }) => {
                    LocalResponse::err(id, if status == 403 { "forbidden" } else { "invalid-params" }, body)
                }
                Err(e) => LocalResponse::err(id, "unavailable", e.to_string()),
            }
        }
        "planner.restart" => {
            // Used by the updater after switching binaries. The service manager that started planner restarts
            // it (procd respawn, systemd Restart=on-failure, Podman restart policy); exit code 75 = EX_TEMPFAIL.
            // Supervised daemons keep running and are adopted by the next planner when unchanged (D32).
            let reason = req.params.get("reason").and_then(Value::as_str).unwrap_or("restart requested").to_string();
            info!("planner restart requested locally: {reason}");
            let p2 = p.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                p2.engine.supervisor.detach_all().await;
                std::process::exit(75);
            });
            ok(json!({"restarting": true}))
        }
        "action.run" => {
            LocalResponse::err(id, "unavailable", "package actions are triggered through the manager so they are authorized and audited")
        }
        other => LocalResponse::err(id, "not-found", format!("unhandled method {other}")),
    }
}

async fn enroll(p: &Arc<Planner>, id: u64, params: &Value) -> LocalResponse {
    let url = params.get("managerUrl").and_then(Value::as_str).unwrap_or_default().to_string();
    let token = params.get("token").and_then(Value::as_str).unwrap_or_default().to_string();
    let ca_file = params.get("caFile").and_then(Value::as_str).map(str::to_string);
    let allow_insecure = params.get("allowInsecure").and_then(Value::as_bool).unwrap_or(false);
    if url.is_empty() || token.is_empty() {
        return LocalResponse::err(id, "invalid-params", "managerUrl and token are required");
    }
    match crate::enroll(p, &url, &token, ca_file, allow_insecure).await {
        Ok(node_id) => LocalResponse::ok(id, json!({"nodeId": node_id})),
        Err(e) => LocalResponse::err(id, "enroll-failed", e),
    }
}

#[cfg(unix)]
pub mod server {
    use super::*;
    use std::path::Path;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};

    fn group_gid(name: &str) -> Option<u32> {
        std::fs::read_to_string("/etc/group").ok()?.lines().find_map(|l| {
            let mut f = l.split(':');
            (f.next()? == name).then(|| f.nth(1)?.parse().ok()).flatten()
        })
    }

    fn bind(path: &Path, mode: u32) -> std::io::Result<UnixListener> {
        let _ = std::fs::remove_file(path);
        if let Some(parent) = path.parent() {
            meduza_np_runtime::fsutil::ensure_dir(parent, 0o755)?;
        }
        let listener = UnixListener::bind(path)?;
        meduza_np_runtime::fsutil::set_mode(path, mode)?;
        if let Some(gid) = group_gid("meduza-np") {
            let c = std::ffi::CString::new(path.as_os_str().to_string_lossy().as_bytes()).unwrap();
            // SAFETY: chown on the socket we just created.
            unsafe { libc::chown(c.as_ptr(), u32::MAX, gid) };
        }
        Ok(listener)
    }

    fn authorized(stream: &UnixStream) -> bool {
        let Ok(cred) = stream.peer_cred() else { return false };
        // SAFETY: geteuid has no preconditions.
        let me = unsafe { libc::geteuid() };
        cred.uid() == 0 || cred.uid() == me || group_gid("meduza-np").map(|g| g == cred.gid()).unwrap_or(false)
    }

    async fn serve_conn(p: Arc<Planner>, stream: UnixStream, access_only: bool) {
        let authorized = authorized(&stream);
        let (r, mut w) = stream.into_split();
        let mut lines = BufReader::new(r.take(meduza_np_protocol::local::MAX_FRAME as u64 * 16)).lines();
        while let Ok(Some(line)) = tokio::time::timeout(Duration::from_secs(300), lines.next_line()).await.unwrap_or(Ok(None)) {
            if line.len() > meduza_np_protocol::local::MAX_FRAME {
                break;
            }
            let resp = match serde_json::from_str::<LocalRequest>(&line) {
                Err(e) => LocalResponse::err(0, "invalid-params", e.to_string()),
                Ok(req) if access_only => access(&p, req).await,
                Ok(req) => dispatch(&p, req, authorized).await,
            };
            let mut out = serde_json::to_vec(&resp).unwrap_or_default();
            out.push(b'\n');
            if w.write_all(&out).await.is_err() {
                break;
            }
        }
    }

    /// The Access socket only accepts `access.auth` and `access.session`, so the OpenVPN user gains nothing else (§9.3).
    async fn access(p: &Arc<Planner>, req: LocalRequest) -> LocalResponse {
        if req.method == "access.session" {
            return access_session(p, req);
        }
        if req.method != "access.auth" {
            return LocalResponse::err(req.id, "forbidden", "only access.auth and access.session are served here");
        }
        let auth: meduza_np_protocol::access::AccessAuthRequest = match serde_json::from_value(req.params) {
            Ok(a) => a,
            Err(e) => return LocalResponse::err(req.id, "invalid-params", e.to_string()),
        };
        // Only instances that are applied Access instances on this node may ask.
        let applied = crate::state::AppliedState::load(&p.paths).unwrap_or_default();
        let deny = |code: &str| {
            LocalResponse::ok(
                req.id,
                serde_json::to_value(meduza_np_protocol::access::AccessAuthResponse::deny(auth.request_id.clone(), code)).unwrap(),
            )
        };
        if !applied.instances.contains_key(&format!("openvpn/{}", auth.instance_id)) {
            return deny("instance-not-allowed");
        }
        let resp = match p.client() {
            Some(client) => client.access_auth(&auth, Duration::from_secs(8)).await,
            None => meduza_np_protocol::access::AccessAuthResponse::deny(auth.request_id.clone(), "manager-unavailable"),
        };
        let resp = crate::access::decide(&auth, resp);
        p.access_sessions.lock().unwrap().record(&auth, &resp, std::time::Instant::now());
        info!(
            "access {} on {}{}: {:?} ({})",
            auth.username,
            auth.instance_id,
            if auth.reauth { " (renegotiation)" } else { "" },
            resp.decision,
            resp.reason_code
        );
        LocalResponse::ok(req.id, serde_json::to_value(resp).unwrap_or_default())
    }

    /// OpenVPN's client-connect hook: the session lifetime of the connection the auth hook just allowed (D38).
    /// `{instanceId, username, ip, port}` → `{allow, sessionSeconds}`; no recorded decision → deny.
    fn access_session(p: &Arc<Planner>, req: LocalRequest) -> LocalResponse {
        let s = |k: &str| req.params.get(k).and_then(Value::as_str);
        let (Some(instance), Some(username)) = (s("instanceId"), s("username")) else {
            return LocalResponse::err(req.id, "invalid-params", "instanceId and username are required");
        };
        let port = req.params.get("port").and_then(Value::as_u64).and_then(|v| u16::try_from(v).ok());
        let found = p.access_sessions.lock().unwrap().take(instance, username, s("ip"), port, std::time::Instant::now());
        let result = match found {
            Some(crate::access::Handoff::Lifetime(secs)) => json!({"allow": true, "sessionSeconds": secs}),
            Some(crate::access::Handoff::Unverified) => {
                info!("access {username} on {instance}: new connection without a manager decision refused");
                json!({"allow": false, "reasonCode": "unverified"})
            }
            None => json!({"allow": false, "reasonCode": "no-session"}),
        };
        LocalResponse::ok(req.id, result)
    }

    pub async fn run(p: Arc<Planner>) -> std::io::Result<()> {
        let control = bind(&p.paths.planner_socket(), 0o660)?;
        let access_sock = bind(&p.paths.access_socket(), 0o666)?;
        let limit = Arc::new(tokio::sync::Semaphore::new(16));
        let p2 = p.clone();
        let limit2 = limit.clone();
        tokio::spawn(async move {
            loop {
                if let Ok((s, _)) = access_sock.accept().await {
                    let Ok(permit) = limit2.clone().try_acquire_owned() else { continue };
                    let p = p2.clone();
                    tokio::spawn(async move {
                        serve_conn(p, s, true).await;
                        drop(permit);
                    });
                }
            }
        });
        loop {
            let (s, _) = control.accept().await?;
            let Ok(permit) = limit.clone().try_acquire_owned() else { continue };
            let p = p.clone();
            tokio::spawn(async move {
                serve_conn(p, s, false).await;
                drop(permit);
            });
        }
    }
}
