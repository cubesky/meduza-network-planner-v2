//! Mihomo wrapper (`run` mode): subscription download (bounded, atomic, last good kept), normalization into the
//! final config, process supervision, API readiness, hot reload via the API, and filling the TPROXY
//! proxy-server set with resolved addresses so proxied traffic never loops back into Mihomo.

use std::collections::BTreeMap;
use std::io::Write;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Spec {
    #[serde(default)]
    dns_context: crate::dns::Context,
    controlled: serde_json::Map<String, Value>,
    subscriptions: Vec<Sub>,
    active: Option<String>,
    api: String,
    download_proxy: Option<String>,
    state_dir: String,
    tproxy: bool,
    table: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Sub {
    id: String,
    url: String,
    refresh_seconds: u64,
}

/// Request to the local controller.
pub fn api_call(addr: &str, secret: &str, method: &str, path: &str, body: Option<&str>) -> Result<(u16, String), String> {
    meduza_np_plugin_sdk::localhttp::call(addr, secret, method, path, body)
}

fn resolve_v4(hosts: &[String]) -> Vec<String> {
    let hosts = hosts.to_vec();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = std::collections::BTreeSet::new();
        for h in hosts.iter().take(2000) {
            if let Ok(ip) = h.parse::<std::net::Ipv4Addr>() {
                out.insert(ip.to_string());
                continue;
            }
            if let Ok(addrs) = (h.as_str(), 0).to_socket_addrs() {
                for a in addrs {
                    if let SocketAddr::V4(v4) = a {
                        out.insert(v4.ip().to_string());
                    }
                }
            }
        }
        let _ = tx.send(out.into_iter().collect::<Vec<_>>());
    });
    rx.recv_timeout(Duration::from_secs(20)).unwrap_or_default()
}

fn fill_set(table: &str, ips: &[String]) -> Result<(), String> {
    let mut script = format!("flush set inet {table} proxy_servers_v4\n");
    if !ips.is_empty() {
        script.push_str(&format!("add element inet {table} proxy_servers_v4 {{ {} }}\n", ips.join(", ")));
    }
    let mut child =
        Command::new("nft").args(["-f", "-"]).stdin(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    child.stdin.take().map(|mut s| s.write_all(script.as_bytes())).transpose().map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn write_atomic(path: &Path, content: &str, mode: u32) {
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    if let Ok(mut file) = options.open(&tmp) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if file.set_permissions(std::fs::Permissions::from_mode(mode)).is_err() {
                return;
            }
        }
        if file.write_all(content.as_bytes()).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
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

struct SubState {
    status: &'static str,
    updated_at: Option<String>,
    last_error: Option<String>,
    overridden: Vec<String>,
    next: Instant,
    failures: u32,
}

fn publish_subscriptions(path: &Path, subs: &BTreeMap<String, SubState>) {
    let mut state: Value =
        std::fs::read(path).ok().and_then(|s| serde_json::from_slice(&s).ok()).unwrap_or_else(|| json!({"apiReady":false}));
    state["subscriptions"] = json!(subs
        .iter()
        .map(|(id, s)| (
            id.clone(),
            json!({"status":s.status,"updatedAt":s.updated_at,"lastError":s.last_error,"overriddenKeys":s.overridden})
        ))
        .collect::<serde_json::Map<_, _>>());
    state["updatedAt"] = json!(meduza_np_plugin_sdk::now_rfc3339());
    write_atomic(path, &state.to_string(), 0o644);
}

fn ui_path(yaml: &str) -> Option<String> {
    serde_norway::from_str::<Value>(yaml).ok()?.get("external-ui")?.as_str().map(str::to_string)
}

/// Kept out of the public state and reports; only the privileged local control method reads this file.
fn publish_panel(path: &Path, work: &Path, yaml: &str, api_ready: bool, address: &str, secret: &str) {
    let enabled = ui_path(yaml).is_some();
    let ready = api_ready && ui_path(yaml).is_some_and(|p| work.join(p).join("index.html").is_file());
    write_atomic(
        &path.with_file_name("mihomo-panel-status.json"),
        &json!({"enabled":enabled,"ready":ready,"address":address}).to_string(),
        0o644,
    );
    if ready {
        write_atomic(path, &json!({"address":address,"secret":secret}).to_string(), 0o600);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// Only remove obsolete directories of our own URL-derived cache, after the active panel is complete.
fn prune_ui_cache(work: &Path, yaml: &str) {
    let Some(active) = ui_path(yaml) else { return };
    let Some(name) = active.strip_prefix("ui/") else { return };
    if name.len() != 64 || !name.bytes().all(|c| c.is_ascii_hexdigit()) {
        return;
    }
    if !work.join(&active).join("index.html").is_file() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(work.join("ui")) else { return };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let candidate = file_name.to_string_lossy();
        if candidate != name
            && candidate.len() == 64
            && candidate.bytes().all(|c| c.is_ascii_hexdigit())
            && entry.file_type().is_ok_and(|t| t.is_dir() && !t.is_symlink())
        {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

pub fn run(args: &[String]) -> ! {
    let get = |k: &str| args.windows(2).find(|w| w[0] == k).map(|w| w[1].clone());
    let (Some(mihomo), Some(spec_path), Some(run_dir)) = (get("--mihomo"), get("--spec"), get("--run-dir")) else {
        eprintln!("usage: run --mihomo BIN --spec FILE --run-dir DIR");
        std::process::exit(2)
    };
    let spec: Spec = std::fs::read_to_string(&spec_path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| {
        eprintln!("cannot read spec {spec_path}");
        std::process::exit(2)
    });
    term();
    let secret = spec.controlled.get("secret").and_then(Value::as_str).unwrap_or_default().to_string();
    let base = PathBuf::from(&spec.state_dir).join("mihomo");
    let work = base.join("work");
    let _ = std::fs::create_dir_all(&work);
    let final_path = PathBuf::from(&run_dir).join("mihomo-final.yaml");
    let state_path = PathBuf::from(&run_dir).join("mihomo-main.state");
    let panel_path = PathBuf::from(&run_dir).join("mihomo-panel.json");
    let _ = std::fs::remove_file(&panel_path);
    let _ = std::fs::remove_file(panel_path.with_file_name("mihomo-panel-status.json"));
    let mut subs: BTreeMap<String, SubState> = spec
        .subscriptions
        .iter()
        .map(|s| {
            (
                s.id.clone(),
                SubState {
                    status: "pending",
                    updated_at: None,
                    last_error: None,
                    overridden: Vec::new(),
                    next: Instant::now(),
                    failures: 0,
                },
            )
        })
        .collect();
    let mut child: Option<Child> = None;
    write_atomic(&state_path, r#"{"apiReady":false}"#, 0o644);
    publish_subscriptions(&state_path, &subs);
    let mut last_final = String::new();
    let mut set_filled = String::new();
    let mut next_set = Instant::now();
    loop {
        if term() {
            let _ = std::fs::remove_file(&panel_path);
            let _ = std::fs::remove_file(panel_path.with_file_name("mihomo-panel-status.json"));
            if let Some(mut c) = child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            std::process::exit(0);
        }
        // A forced refresh (`refresh-subscription` action): every subscription is due now.
        if std::fs::remove_file(crate::refresh_trigger(&run_dir)).is_ok() {
            for st in subs.values_mut() {
                st.next = Instant::now();
                st.failures = 0;
            }
        }
        // 1. Refresh due subscriptions into the persistent cache (last good file is kept on failure).
        for s in &spec.subscriptions {
            let st = subs.get_mut(&s.id).expect("state");
            if Instant::now() < st.next {
                continue;
            }
            st.status = "downloading";
            publish_subscriptions(&state_path, &subs);
            let st = subs.get_mut(&s.id).expect("state");
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
                crate::normalize_with_context(&text, &spec.controlled, &spec.dns_context).map(|_| text)
            }) {
                Ok(_) => {
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
            publish_subscriptions(&state_path, &subs);
        }
        // 2. Build the final configuration from the active subscription.
        let active = spec.active.clone().unwrap_or_default();
        let cached = std::fs::read_to_string(base.join(format!("sub-{active}.yaml"))).ok();
        if let Some(text) = cached {
            match crate::normalize_with_context(&text, &spec.controlled, &spec.dns_context) {
                Ok((yaml, overridden)) => {
                    if let Some(st) = subs.get_mut(&active) {
                        st.overridden = overridden;
                    }
                    if yaml != last_final {
                        write_atomic(&final_path, &yaml, 0o600);
                        // Config reload does not recreate the controller/static-file router. Restart the core
                        // when the UI path changes, including enabling or disabling an inherited panel.
                        let reloaded = child.is_some()
                            && ui_path(&yaml) == ui_path(&last_final)
                            && api_call(
                                &spec.api,
                                &secret,
                                "PUT",
                                "/configs?force=true",
                                Some(&json!({"path": final_path.to_string_lossy()}).to_string()),
                            )
                            .map(|(c, _)| (200..300).contains(&c))
                            .unwrap_or(false);
                        if !reloaded {
                            if let Some(mut c) = child.take() {
                                let _ = c.kill();
                                let _ = c.wait();
                            }
                            match Command::new(&mihomo)
                                .args(["-d", &work.to_string_lossy(), "-f", &final_path.to_string_lossy()])
                                .stdin(Stdio::null())
                                .spawn()
                            {
                                Ok(c) => child = Some(c),
                                Err(e) => eprintln!("cannot start {mihomo}: {e}"),
                            }
                        }
                        last_final = yaml;
                        next_set = Instant::now();
                    }
                }
                Err(e) => {
                    eprintln!("active subscription unusable: {e}");
                    if let Some(st) = subs.get_mut(&active) {
                        st.status = "failed";
                        st.last_error = Some(e);
                    }
                }
            }
        }
        if let Some(c) = child.as_mut() {
            if let Ok(Some(status)) = c.try_wait() {
                let _ = std::fs::remove_file(&panel_path);
                let _ = std::fs::remove_file(panel_path.with_file_name("mihomo-panel-status.json"));
                eprintln!("mihomo exited: {status}");
                std::process::exit(status.code().unwrap_or(1));
            }
        }
        prune_ui_cache(&work, &last_final);
        // 3. Keep the proxy-server bypass set current (also after planner re-created the table).
        if spec.tproxy && !last_final.is_empty() && Instant::now() >= next_set {
            let ips = resolve_v4(&crate::proxy_servers(&last_final));
            match fill_set(&spec.table, &ips) {
                Ok(()) => set_filled = format!("{} address(es)", ips.len()),
                Err(e) => set_filled = format!("error: {e}"),
            }
            next_set = Instant::now() + Duration::from_secs(300);
        }
        // 4. Status for the probe: API readiness, groups and connections summary.
        let version = api_call(&spec.api, &secret, "GET", "/version", None)
            .ok()
            .filter(|(c, _)| *c == 200)
            .and_then(|(_, b)| serde_json::from_str::<Value>(&b).ok());
        let groups: Vec<Value> = api_call(&spec.api, &secret, "GET", "/proxies", None)
            .ok()
            .and_then(|(_, b)| serde_json::from_str::<Value>(&b).ok())
            .and_then(|v| v.get("proxies").and_then(Value::as_object).cloned())
            .map(|m| {
                m.values()
                    .filter(|p| p.get("all").is_some())
                    .take(50)
                    .map(|p| json!({"name": p.get("name"), "type": p.get("type"), "now": p.get("now")}))
                    .collect()
            })
            .unwrap_or_default();
        let connections = api_call(&spec.api, &secret, "GET", "/connections", None)
            .ok()
            .and_then(|(_, b)| serde_json::from_str::<Value>(&b).ok())
            .and_then(|v| v.get("connections").and_then(Value::as_array).map(|a| a.len()));
        publish_panel(&panel_path, &work, &last_final, version.is_some(), &spec.api, &secret);
        let state = json!({
            "apiReady": version.is_some(),
            "version": version.and_then(|v| v.get("version").cloned()),
            "groups": groups,
            "connections": connections,
            "proxyServerSet": set_filled,
            "subscriptions": subs.iter().map(|(k, s)| (k.clone(), json!({"status": s.status, "updatedAt": s.updated_at, "lastError": s.last_error, "overriddenKeys": s.overridden}))).collect::<serde_json::Map<_, _>>(),
            "updatedAt": meduza_np_plugin_sdk::now_rfc3339(),
        });
        write_atomic(&state_path, &state.to_string(), 0o644);
        for _ in 0..100 {
            if term() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_credentials_are_private_and_published_only_while_the_panel_and_api_are_ready() {
        let root = std::env::temp_dir().join(format!("mnp-panel-file-{}", std::process::id()));
        let ui = format!("ui/{}", "d".repeat(64));
        std::fs::create_dir_all(root.join(&ui)).unwrap();
        let path = root.join("mihomo-panel.json");
        let yaml = format!("external-ui: {ui}\n");
        publish_panel(&path, &root, &yaml, true, "0.0.0.0:9090", "private-fixture");
        assert!(!path.exists());
        std::fs::write(root.join(ui).join("index.html"), "ready").unwrap();
        publish_panel(&path, &root, &yaml, true, "0.0.0.0:9090", "private-fixture");
        let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["secret"], "private-fixture");
        let public = std::fs::read_to_string(root.join("mihomo-panel-status.json")).unwrap();
        assert!(!public.contains("private-fixture") && !public.contains("secret"));
        assert!(serde_json::from_str::<Value>(&public).unwrap()["enabled"].as_bool().unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        publish_panel(&path, &root, &yaml, false, "0.0.0.0:9090", "private-fixture");
        assert!(!path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ui_cache_is_pruned_only_after_replacement_is_ready_and_only_for_owned_directories() {
        let root = std::env::temp_dir().join(format!("mnp-ui-cache-{}", std::process::id()));
        let old = "a".repeat(64);
        let current = "b".repeat(64);
        let link = "c".repeat(64);
        std::fs::create_dir_all(root.join("ui").join(&old)).unwrap();
        std::fs::create_dir_all(root.join("ui/custom")).unwrap();
        std::fs::create_dir_all(root.join("keep")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("keep"), root.join("ui").join(link)).unwrap();
        let yaml = format!("external-ui: ui/{current}\n");
        prune_ui_cache(&root, &yaml);
        assert!(root.join("ui").join(&old).exists(), "failed/incomplete download keeps previous cache");
        std::fs::create_dir_all(root.join("ui").join(&current)).unwrap();
        std::fs::write(root.join("ui").join(&current).join("index.html"), "ready").unwrap();
        prune_ui_cache(&root, &yaml);
        assert!(!root.join("ui").join(&old).exists());
        assert!(root.join("ui").join(&current).join("index.html").exists());
        assert!(root.join("ui/custom").exists() && root.join("keep").exists());
        #[cfg(unix)]
        assert!(root.join("ui").join("c".repeat(64)).is_symlink());
        std::fs::remove_dir_all(root).unwrap();
    }
}
