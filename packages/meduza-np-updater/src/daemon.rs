//! Long-running updater: own bounded timer (independent of planner, §13.1), `updater.sock` for planner/CLI/dashboard,
//! one worker doing check → plan → apply. Requests never run network work on the socket thread.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use meduza_np_protocol::local::{method_is_mutating, LocalRequest, LocalResponse, MAX_FRAME, UPDATER_METHODS};
use meduza_np_runtime::jitter::OsJitter;
use serde_json::{json, Value};

use crate::engine::{Engine, Outcome, EXIT_SELF_RESTART};
use crate::resolve::Selection;

#[derive(Default)]
struct Shared {
    /// `Some(force_apply)` when a run was requested.
    request: Option<bool>,
    busy: Option<String>,
    last_plan: Option<Value>,
}

type State = Arc<(Mutex<Shared>, Condvar)>;

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn run(engine: &Engine<'_>, running_new_updater: bool) -> ! {
    if let Err(e) = engine.store().init() {
        eprintln!("store: {e}");
    }
    let activity = engine.cfg.store_dir().join("activity.json");
    let _ = meduza_np_runtime::fsutil::write_json_atomic(&activity, &json!({"busy":null}), 0o644);
    if let Some(msg) = engine.recover(running_new_updater) {
        eprintln!("recovery: {msg}");
    }
    let state: State = Arc::new((Mutex::new(Shared::default()), Condvar::new()));
    let socket = engine.cfg.paths.updater_socket();
    let _ = std::fs::remove_file(&socket);
    if let Some(parent) = socket.parent() {
        let _ = meduza_np_runtime::fsutil::ensure_dir(parent, 0o755);
    }
    match UnixListener::bind(&socket) {
        Ok(listener) => {
            let _ = meduza_np_runtime::fsutil::set_mode(&socket, 0o660);
            let st = state.clone();
            let store_root = engine.cfg.store_dir();
            let run_dir = engine.cfg.paths.run.clone();
            let cfg_summary = json!({
                "backend": engine.cfg.backend.name(), "platform": engine.cfg.platform, "channel": engine.cfg.channel,
                "pinnedGroupVersion": engine.cfg.group_version, "source": engine.cfg.base_url, "autoApply": engine.cfg.auto_apply,
                "components": engine.cfg.components,
            });
            std::thread::spawn(move || {
                for conn in listener.incoming().flatten() {
                    let st = st.clone();
                    let root = store_root.clone();
                    let summary = cfg_summary.clone();
                    let run = run_dir.clone();
                    std::thread::spawn(move || serve(conn, st, &root, &run, &summary));
                }
            });
        }
        Err(e) => eprintln!("cannot bind {}: {e} (local control unavailable)", socket.display()),
    }

    let mut jitter = OsJitter::new();
    let mut failures: u32 = 0;
    // Spread fleet checks: first check at a random point of the first period, except when the operator asked for
    // something: a blank install (bootstrap) or a changed component selection is applied right away (§14.3.1).
    let installed = engine.installed().ok();
    let bootstrap = installed.as_ref().map(|s| s.packages.is_empty()).unwrap_or(true) && engine.cfg.components.is_some();
    let selection_changed = match (engine.selection(), &installed) {
        (Selection::Explicit(wanted), Some(s)) => s.selected != wanted,
        _ => false,
    };
    // Features enabled in the manager change the planner's wanted components: install them without waiting for
    // the next scheduled check (§14.3.1 operator request).
    let mut seen_wanted = engine.wanted_components();
    if selection_changed {
        eprintln!("component selection changed; applying it now");
    }
    let mut next_due = if bootstrap || selection_changed {
        now()
    } else {
        now() + meduza_np_runtime::jitter::initial_phase(&mut jitter, engine.cfg.check_interval.min(Duration::from_secs(3600))).as_secs()
    };
    loop {
        let forced = {
            let (lock, cv) = &*state;
            let mut s = lock.lock().unwrap();
            while s.request.is_none() && now() < next_due {
                let wait = Duration::from_secs((next_due - now()).min(60));
                s = cv.wait_timeout(s, wait).unwrap().0;
                let wanted = engine.wanted_components();
                if wanted != seen_wanted {
                    eprintln!("configuration needs components {wanted:?}; applying the selection now");
                    seen_wanted = wanted;
                    next_due = now();
                }
            }
            let forced = s.request.take();
            s.busy = Some(if forced == Some(true) { "applying".into() } else { "checking".into() });
            forced
        };
        let _ = meduza_np_runtime::fsutil::write_json_atomic(&activity, &json!({"busy":"checking"}), 0o644);
        let result = run_pass(engine, forced == Some(true));
        let _ = meduza_np_runtime::fsutil::write_json_atomic(&activity, &json!({"busy":null}), 0o644);
        {
            let (lock, _) = &*state;
            let mut s = lock.lock().unwrap();
            s.busy = None;
            if let Ok((Some(plan), _)) = &result {
                s.last_plan = serde_json::to_value(plan).ok();
            }
        }
        let mut st = engine.load_state();
        match result {
            Ok((_, Some(Outcome::AppliedRestartSelf))) => {
                eprintln!("self-update staged; exiting for takeover");
                std::process::exit(EXIT_SELF_RESTART);
            }
            Ok((_, outcome)) => {
                failures = 0;
                if let Some(o) = outcome {
                    eprintln!("update pass: {o:?}");
                }
                next_due = now() + meduza_np_runtime::jitter::jittered_period(&mut jitter, engine.cfg.check_interval, 0.1).as_secs();
            }
            Err(e) => {
                failures = failures.saturating_add(1);
                eprintln!("update pass failed: {e}");
                // exponential backoff 5 min .. check interval, with jitter (§14.3.1: no download storms)
                let base = Duration::from_secs(300u64.saturating_mul(1 << failures.min(8))).min(engine.cfg.check_interval);
                next_due = now() + meduza_np_runtime::jitter::jittered_period(&mut jitter, base, 0.3).as_secs();
            }
        }
        st.next_check_unix = Some(next_due);
        engine.save_state(&st);
    }
}

fn run_pass(engine: &Engine<'_>, forced: bool) -> Result<(Option<crate::resolve::Plan>, Option<Outcome>), String> {
    let control = engine.load_control();
    let n = now();
    let deferred = control.defer_until.map(|t| n < t).unwrap_or(false);
    let installed = engine.installed()?;
    // An explicit component selection that differs from what is installed is an operator request (§14.3.1).
    let explicit_change = match engine.selection() {
        Selection::Explicit(list) => installed.packages.is_empty() || installed.selected != list,
        Selection::Installed => false,
    };
    let allow = forced || explicit_change || (engine.cfg.auto_apply && !control.paused && !deferred && engine.cfg.in_window(n));
    if forced {
        let g = engine.check().map_err(|e| e.to_string())?;
        let plan = engine.plan(&g)?;
        let out = engine.apply(&g, &plan, true)?;
        return Ok((Some(plan), Some(out)));
    }
    engine.run_once(allow)
}

fn serve(conn: UnixStream, state: State, store_root: &std::path::Path, run_dir: &std::path::Path, summary: &Value) {
    let authorized = authorized(&conn);
    conn.set_read_timeout(Some(Duration::from_secs(60))).ok();
    let mut writer = match conn.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let reader = BufReader::new((&conn).take(MAX_FRAME as u64 * 16));
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let resp = match serde_json::from_str::<LocalRequest>(&line) {
            Err(e) => LocalResponse::err(0, "invalid-params", e.to_string()),
            Ok(req) => dispatch(&req, authorized, &state, store_root, run_dir, summary),
        };
        let mut out = serde_json::to_vec(&resp).unwrap_or_default();
        out.push(b'\n');
        if writer.write_all(&out).is_err() {
            break;
        }
    }
}

fn authorized(stream: &UnixStream) -> bool {
    let mut cred = libc::ucred { pid: 0, uid: u32::MAX, gid: u32::MAX };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    use std::os::fd::AsRawFd;
    // SAFETY: getsockopt writes at most `len` bytes into `cred`.
    let rc = unsafe {
        libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred as *mut _ as *mut libc::c_void, &mut len)
    };
    // SAFETY: geteuid has no preconditions.
    rc == 0 && (cred.uid == 0 || cred.uid == unsafe { libc::geteuid() })
}

fn read_json(path: std::path::PathBuf) -> Value {
    meduza_np_runtime::fsutil::read_json::<Value>(&path, 4 * 1024 * 1024).ok().flatten().unwrap_or(Value::Null)
}

fn dispatch(
    req: &LocalRequest,
    authorized: bool,
    state: &State,
    store_root: &std::path::Path,
    run_dir: &std::path::Path,
    summary: &Value,
) -> LocalResponse {
    let id = req.id;
    let Some(mutating) = method_is_mutating(UPDATER_METHODS, &req.method) else {
        return LocalResponse::err(id, "not-found", format!("unknown method {}", req.method));
    };
    if mutating && !authorized {
        return LocalResponse::err(id, "forbidden", "root or the updater's own user required");
    }
    let (lock, cv) = &**state;
    let state_path = store_root.join("state.json");
    let control_path = store_root.join("control.json");
    let update_control = |f: &dyn Fn(&mut Value)| {
        let mut v = read_json(control_path.clone());
        if !v.is_object() {
            v = json!({});
        }
        f(&mut v);
        meduza_np_runtime::fsutil::write_json_atomic(&control_path, &v, 0o644).map_err(|e| e.to_string())
    };
    match req.method.as_str() {
        "update.status" => {
            let s = lock.lock().unwrap();
            LocalResponse::ok(
                id,
                json!({
                    "config": summary, "busy": s.busy, "lastPlan": s.last_plan,
                    "installed": read_json(store_root.join("installed.json")),
                    "journal": read_json(store_root.join("journal.json")),
                    "lastJournal": read_json(store_root.join("journal.last.json")),
                    "state": read_json(state_path.clone()),
                    "control": read_json(control_path.clone()),
                    "progress": meduza_np_runtime::update::snapshot(store_root, run_dir),
                }),
            )
        }
        "update.plan" => LocalResponse::ok(id, lock.lock().unwrap().last_plan.clone().unwrap_or(Value::Null)),
        "update.check" | "update.apply" => {
            let mut s = lock.lock().unwrap();
            if let Some(b) = &s.busy {
                return LocalResponse::err(id, "busy", format!("already {b}"));
            }
            s.request = Some(req.method == "update.apply");
            cv.notify_all();
            LocalResponse::ok(id, json!({"scheduled": true}))
        }
        "update.pause" | "update.resume" => {
            let paused = req.method == "update.pause";
            match update_control(&|v| v["paused"] = Value::Bool(paused)) {
                Ok(()) => LocalResponse::ok(id, json!({"paused": paused})),
                Err(e) => LocalResponse::err(id, "internal", e),
            }
        }
        "update.defer" => {
            let secs = req.params.get("seconds").and_then(Value::as_u64).unwrap_or(86_400).min(30 * 86_400);
            let until = now() + secs;
            match update_control(&|v| v["deferUntil"] = json!(until)) {
                Ok(()) => LocalResponse::ok(id, json!({"deferUntil": meduza_np_plugin_sdk::unix_to_rfc3339(until)})),
                Err(e) => LocalResponse::err(id, "internal", e),
            }
        }
        other => LocalResponse::err(id, "not-found", format!("unhandled method {other}")),
    }
}
