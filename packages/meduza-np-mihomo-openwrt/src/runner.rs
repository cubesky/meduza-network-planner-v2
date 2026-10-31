//! OpenClash integration loop (`run` mode) and hand-back (`release` mode).
//!
//! `run`: detect OpenClash (tested range only) → refresh subscriptions (bounded, last good kept) → write the
//! managed config file (only a file the adapter owns) → record originals, then set the managed UCI options →
//! debounced controlled restart when something changed → status from the OpenClash-owned controller API.
//! `release`: restore the recorded options, delete only adapter-created files, restart OpenClash once.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::openclash::{self, CliUci, Takeover, Uci};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Spec {
    enabled: bool,
    managed_path: String,
    options: BTreeMap<String, String>,
    subscriptions: Vec<Sub>,
    active: Option<String>,
    restart_on_change: bool,
    dns_integration: bool,
    download_proxy: Option<String>,
    state_dir: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Sub {
    id: String,
    url: String,
    refresh_seconds: u64,
}

struct SubState {
    status: &'static str,
    updated_at: Option<String>,
    last_error: Option<String>,
    dropped: Vec<String>,
    next: Instant,
    failures: u32,
}

const MIN_RESTART_INTERVAL: Duration = Duration::from_secs(60);

fn write_atomic(path: &Path, content: &str, mode: u32) -> Result<(), String> {
    let tmp = path.with_extension("meduza-np.tmp");
    std::fs::write(&tmp, content).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = mode;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

#[cfg(unix)]
fn term() -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    static TERM: AtomicBool = AtomicBool::new(false);
    extern "C" fn on_term(_: libc::c_int) {
        TERM.store(true, Ordering::SeqCst);
    }
    if !INSTALLED.swap(true, Ordering::SeqCst) {
        // SAFETY: handler only stores to an atomic.
        unsafe {
            libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
            libc::signal(libc::SIGINT, on_term as *const () as libc::sighandler_t);
        }
    }
    TERM.load(Ordering::SeqCst)
}

#[cfg(not(unix))]
fn term() -> bool {
    false
}

fn read_spec(args: &[String]) -> (Spec, String) {
    let get = |k: &str| args.windows(2).find(|w| w[0] == k).map(|w| w[1].clone());
    let (Some(spec_path), Some(run_dir)) = (get("--spec"), get("--run-dir")) else {
        eprintln!("usage: run|release --spec FILE --run-dir DIR");
        std::process::exit(2)
    };
    let spec: Spec = std::fs::read_to_string(&spec_path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| {
        eprintln!("cannot read spec {spec_path}");
        std::process::exit(2)
    });
    (spec, run_dir)
}

/// Hands OpenClash back. Idempotent: without a recorded takeover nothing is changed.
pub fn release_takeover(state_dir: &str, uci: &mut dyn Uci) -> Result<Value, String> {
    let path = crate::takeover_path(state_dir);
    let Some(t) = Takeover::load(&path) else { return Ok(json!({"restored": false, "reason": "nothing was taken over"})) };
    let changed = t.restore(uci)?;
    let mut removed = Vec::new();
    for f in &t.managed_files {
        if std::fs::remove_file(f).is_ok() {
            removed.push(f.clone());
        }
    }
    let restart = if changed > 0 || !removed.is_empty() {
        // Back to the user's original state: running if it was enabled before, stopped otherwise.
        let verb = if t.options.get("enable").cloned().flatten().as_deref() == Some("1") || !t.options.contains_key("enable") {
            "restart"
        } else {
            "stop"
        };
        Some(openclash::init_script(verb).map(|_| verb.to_string()))
    } else {
        None
    };
    let _ = std::fs::remove_file(&path);
    Ok(json!({"restored": true, "options": changed, "removedFiles": removed, "openclash": restart.map(|r| r.unwrap_or_else(|e| e))}))
}

pub fn release(args: &[String]) -> ! {
    let (spec, run_dir) = read_spec(args);
    let state_path = PathBuf::from(&run_dir).join("mihomo-openwrt.state");
    match release_takeover(&spec.state_dir, &mut CliUci) {
        Ok(v) => {
            eprintln!("released: {v}");
            let _ = std::fs::remove_file(&state_path);
            std::process::exit(0)
        }
        Err(e) => {
            eprintln!("release failed: {e}");
            std::process::exit(1)
        }
    }
}

pub fn run(args: &[String]) -> ! {
    let (spec, run_dir) = read_spec(args);
    if !spec.enabled {
        release(args);
    }
    term();
    let base = PathBuf::from(&spec.state_dir).join("mihomo-openwrt");
    let _ = std::fs::create_dir_all(&base);
    let takeover_path = crate::takeover_path(&spec.state_dir);
    let state_path = PathBuf::from(&run_dir).join("mihomo-openwrt.state");
    let mut subs: BTreeMap<String, SubState> = spec
        .subscriptions
        .iter()
        .map(|s| {
            (
                s.id.clone(),
                SubState { status: "pending", updated_at: None, last_error: None, dropped: Vec::new(), next: Instant::now(), failures: 0 },
            )
        })
        .collect();
    let mut uci = CliUci;
    let mut last_written = std::fs::read_to_string(&spec.managed_path).unwrap_or_default();
    let mut restart_pending = false;
    let mut last_restart: Option<Instant> = None;
    let mut last_restart_at: Option<String> = None;
    let mut last_error: Option<String> = None;
    loop {
        if term() {
            // Stopping the adapter (upgrade, planner restart) leaves OpenClash running as configured.
            std::process::exit(0);
        }
        let detection = openclash::detect(Path::new("/"));
        if detection.compatible {
            last_error = None;
            // A forced refresh (`refresh-subscription` action): every subscription is due now.
            if std::fs::remove_file(crate::refresh_trigger(&run_dir)).is_ok() {
                for st in subs.values_mut() {
                    st.next = Instant::now();
                    st.failures = 0;
                }
            }
            // 1. Refresh due subscriptions into the persistent cache (last good file kept on failure).
            for s in &spec.subscriptions {
                let st = subs.get_mut(&s.id).expect("state");
                if Instant::now() < st.next {
                    continue;
                }
                let dest = base.join(format!("sub-{}.yaml", s.id));
                let tmp = base.join(format!(".sub-{}.download", s.id));
                let opts = meduza_np_plugin_sdk::fetch::FetchOptions {
                    timeout_seconds: 60,
                    max_bytes: 16 * 1024 * 1024,
                    proxy: spec.download_proxy.as_deref(),
                    header: None,
                };
                match meduza_np_plugin_sdk::fetch::fetch_to_file(&s.url, &tmp, &opts).and_then(|_| {
                    let text = std::fs::read_to_string(&tmp).map_err(|e| e.to_string())?;
                    crate::managed_config(&text).map(|_| ())
                }) {
                    Ok(()) => {
                        let _ = std::fs::rename(&tmp, &dest);
                        st.status = "ok";
                        st.updated_at = Some(meduza_np_plugin_sdk::now_rfc3339());
                        st.last_error = None;
                        st.failures = 0;
                        st.next = Instant::now() + Duration::from_secs(s.refresh_seconds.max(60));
                    }
                    Err(e) => {
                        let _ = std::fs::remove_file(&tmp);
                        eprintln!("subscription {}: {e}", s.id);
                        st.status = if dest.exists() { "stale" } else { "failed" };
                        st.last_error = Some(e);
                        st.failures += 1;
                        st.next = Instant::now() + Duration::from_secs((60u64 << st.failures.min(6)).min(3600));
                    }
                }
            }
            // 2. Takeover record first, then the managed file, then the options.
            let active = spec.active.clone().unwrap_or_default();
            let cached = std::fs::read_to_string(base.join(format!("sub-{active}.yaml"))).ok();
            if let Some(text) = cached {
                match step(&spec, &text, &takeover_path, &detection, &mut uci, &mut last_written) {
                    Ok((changed, dropped)) => {
                        if let Some(st) = subs.get_mut(&active) {
                            st.dropped = dropped;
                        }
                        restart_pending |= changed;
                    }
                    Err(e) => {
                        eprintln!("{e}");
                        last_error = Some(e);
                    }
                }
            }
            // 3. Controlled, debounced restart (OpenClash re-reads config_path/en_mode only on restart). Without
            //    restartOnChange a running core is left alone until the operator triggers `reload-openclash`.
            let wanted = restart_pending && (spec.restart_on_change || !openclash::core_running());
            if wanted && last_restart.map(|t| t.elapsed() >= MIN_RESTART_INTERVAL).unwrap_or(true) {
                last_restart = Some(Instant::now());
                match openclash::init_script("restart") {
                    Ok(()) => {
                        restart_pending = false;
                        last_restart_at = Some(meduza_np_plugin_sdk::now_rfc3339());
                    }
                    Err(e) => {
                        eprintln!("{e}");
                        last_error = Some(e);
                    }
                }
            }
        }
        // 4. Status from what OpenClash actually runs.
        let uci_now = if detection.installed { CliUci::show() } else { BTreeMap::new() };
        let state = status(
            &spec,
            &detection,
            &uci_now,
            &subs,
            restart_pending,
            last_restart_at.as_deref(),
            last_error.as_deref(),
            Takeover::load(&takeover_path).is_some(),
        );
        let _ = write_atomic(&state_path, &state.to_string(), 0o644);
        for _ in 0..if detection.compatible { 100 } else { 600 } {
            if term() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// One integration step for a downloaded subscription. Returns (something changed, dropped keys).
fn step(
    spec: &Spec,
    sub_text: &str,
    takeover_path: &Path,
    detection: &openclash::Detection,
    uci: &mut dyn Uci,
    last_written: &mut String,
) -> Result<(bool, Vec<String>), String> {
    let (yaml, dropped) = crate::managed_config(sub_text).map_err(|e| format!("active subscription unusable: {e}"))?;
    let mut t = Takeover::load(takeover_path).unwrap_or_else(|| Takeover {
        taken_at: meduza_np_plugin_sdk::now_rfc3339(),
        openclash_version: detection.version.clone(),
        ..Default::default()
    });
    let keys: Vec<&str> = spec.options.keys().map(String::as_str).collect();
    let mut dirty = t.record(uci, &keys);
    let owned = t.managed_files.contains(&spec.managed_path);
    if !owned {
        if Path::new(&spec.managed_path).exists() {
            return Err(format!("{} exists and was not created by this adapter; refusing to overwrite it", spec.managed_path));
        }
        t.managed_files.push(spec.managed_path.clone());
        dirty = true;
    }
    if dirty || !takeover_path.exists() {
        t.save(takeover_path)?;
    }
    let mut changed = false;
    if yaml != *last_written {
        if let Some(dir) = Path::new(&spec.managed_path).parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        write_atomic(Path::new(&spec.managed_path), &yaml, 0o600)?;
        *last_written = yaml;
        changed = true;
    }
    changed |= openclash::apply_options(uci, &t, &spec.options)? > 0;
    Ok((changed, dropped))
}

#[allow(clippy::too_many_arguments)]
fn status(
    spec: &Spec,
    detection: &openclash::Detection,
    uci: &BTreeMap<String, String>,
    subs: &BTreeMap<String, SubState>,
    restart_pending: bool,
    last_restart: Option<&str>,
    last_error: Option<&str>,
    taken_over: bool,
) -> Value {
    let port = uci.get("cn_port").and_then(|p| p.parse::<u16>().ok()).unwrap_or(9090);
    let api = format!("127.0.0.1:{port}");
    let secret = uci.get("dashboard_password").cloned().unwrap_or_default();
    let get = |path: &str| {
        meduza_np_plugin_sdk::localhttp::call(&api, &secret, "GET", path, None)
            .ok()
            .filter(|(c, _)| *c == 200)
            .and_then(|(_, b)| serde_json::from_str::<Value>(&b).ok())
    };
    let version = if detection.compatible { get("/version") } else { None };
    let groups: Vec<Value> = version
        .as_ref()
        .and_then(|_| get("/proxies"))
        .and_then(|v| v.get("proxies").and_then(Value::as_object).cloned())
        .map(|m| {
            m.values()
                .filter(|p| p.get("all").is_some())
                .take(50)
                .map(|p| json!({"name": p.get("name"), "type": p.get("type"), "now": p.get("now")}))
                .collect()
        })
        .unwrap_or_default();
    let connections =
        version.as_ref().and_then(|_| get("/connections")).and_then(|v| v.get("connections").and_then(Value::as_array).map(|a| a.len()));
    json!({
        "openclash": detection,
        "apiReady": version.is_some(),
        "coreVersion": version.and_then(|v| v.get("version").cloned()),
        "groups": groups,
        "connections": connections,
        "configPath": uci.get("config_path"),
        "selected": uci.get("config_path") == Some(&spec.managed_path),
        "enMode": uci.get("en_mode"),
        "coreType": uci.get("core_type"),
        "dnsPort": uci.get("dns_port").and_then(|p| p.parse::<u16>().ok()),
        "redirectDns": uci.get("enable_redirect_dns"),
        "dnsIntegration": spec.dns_integration,
        "takenOver": taken_over,
        "restartPending": restart_pending,
        "lastRestart": last_restart,
        "lastError": last_error,
        "subscriptions": subs.iter().map(|(k, s)| (k.clone(), json!({"status": s.status, "updatedAt": s.updated_at, "lastError": s.last_error, "overriddenKeys": s.dropped}))).collect::<serde_json::Map<_, _>>(),
        "updatedAt": meduza_np_plugin_sdk::now_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openclash::MemUci;

    #[test]
    fn step_owns_only_its_file_and_records_originals_first() {
        let dir = std::env::temp_dir().join(format!("mnp-ocrun-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let managed = dir.join("config/meduza-np.yaml").to_string_lossy().into_owned();
        let spec = Spec {
            enabled: true,
            managed_path: managed.clone(),
            options: BTreeMap::from([("enable".into(), "1".into()), ("config_path".into(), managed.clone())]),
            subscriptions: Vec::new(),
            active: Some("main".into()),
            restart_on_change: true,
            dns_integration: false,
            download_proxy: None,
            state_dir: dir.to_string_lossy().into_owned(),
        };
        let det = openclash::Detection { installed: true, version: Some("0.46.0".into()), compatible: true, reason: None };
        let tp = crate::takeover_path(&spec.state_dir);
        let mut uci = MemUci::default();
        uci.values.insert("config_path".into(), "/etc/openclash/config/user.yaml".into());
        let sub = "secret: x\nproxies:\n  - {name: a, type: ss, server: s, port: 1}\nrules: [MATCH,a]\n";
        let mut last = String::new();
        let (changed, dropped) = step(&spec, sub, &tp, &det, &mut uci, &mut last).unwrap();
        assert!(changed);
        assert_eq!(dropped, vec!["secret"]);
        let t = Takeover::load(&tp).unwrap();
        assert_eq!(t.options.get("config_path").cloned().flatten().as_deref(), Some("/etc/openclash/config/user.yaml"));
        assert_eq!(t.options.get("enable"), Some(&None));
        assert_eq!(t.managed_files, vec![managed.clone()]);
        assert_eq!(uci.get("config_path").as_deref(), Some(managed.as_str()));
        let (changed, _) = step(&spec, sub, &tp, &det, &mut uci, &mut last).unwrap();
        assert!(!changed, "unchanged input → no restart");

        // Release restores the originals and deletes only the adapter's file (init script is absent here).
        let user_file = dir.join("config/user.yaml");
        std::fs::write(&user_file, "user").unwrap();
        let out = release_takeover(&spec.state_dir, &mut uci).unwrap();
        assert_eq!(out["restored"], json!(true));
        assert_eq!(uci.get("config_path").as_deref(), Some("/etc/openclash/config/user.yaml"));
        assert_eq!(uci.get("enable"), None);
        assert!(!Path::new(&managed).exists() && user_file.exists());
        assert!(!tp.exists());
        assert_eq!(release_takeover(&spec.state_dir, &mut uci).unwrap()["restored"], json!(false), "idempotent");

        // A user's file with the managed name is never overwritten.
        std::fs::write(&managed, "mine").unwrap();
        let mut last = String::new();
        assert!(step(&spec, sub, &tp, &det, &mut uci, &mut last).unwrap_err().contains("refusing"));
        assert_eq!(std::fs::read_to_string(&managed).unwrap(), "mine");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
