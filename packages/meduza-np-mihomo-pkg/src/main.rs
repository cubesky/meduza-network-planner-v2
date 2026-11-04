use std::collections::BTreeMap;
use std::time::Duration;

use meduza_np_mihomo_pkg as mp;
use meduza_np_plugin_sdk::{run, PlanResult, Plugin, PluginError, PluginRequest, ProbeResult, ValidateResult};
use meduza_np_protocol::plan::{GenerationPlan, ProcessSpec, RestartPolicy};
use meduza_np_protocol::report::{InstanceStatus, ServiceState};
use serde_json::{json, Value};

/// Tested assets are compiled in: changing them changes this package's fingerprint and version (§3.3.1).
const ASSETS: &str = include_str!("../assets.json");

fn assets() -> mp::AssetManifest {
    serde_json::from_str(ASSETS).unwrap_or_default()
}

struct MihomoPkg;

#[cfg(unix)]
fn request_rescan(socket: &str) {
    use std::io::{BufRead, BufReader, Write};
    if let Ok(mut s) = std::os::unix::net::UnixStream::connect(socket) {
        let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
        let _ = s.write_all(b"{\"protocol\":1,\"id\":1,\"method\":\"registry.rescan\"}\n");
        let mut line = String::new();
        let _ = BufReader::new(s).read_line(&mut line);
    }
}

#[cfg(not(unix))]
fn request_rescan(_socket: &str) {}

/// "Download now" asks the existing supervisor to retry; it must not run a several-minute download inside
/// a bounded plugin action or race the preparer's staging writer.
#[cfg(unix)]
fn request_download(req: &PluginRequest) -> Result<Value, PluginError> {
    use std::io::{BufRead, BufReader, Write};
    let socket = std::path::Path::new(&req.context.run_dir).join("planner.sock");
    let call = || -> Result<Value, String> {
        let mut stream = std::os::unix::net::UnixStream::connect(socket).map_err(|e| e.to_string())?;
        stream.set_read_timeout(Some(Duration::from_secs(10))).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(Duration::from_secs(2))).map_err(|e| e.to_string())?;
        let mut message =
            serde_json::to_vec(&json!({"protocol":1,"id":1,"method":"instance.restart","params":{"instance":mp::progress::PREPARER}}))
                .map_err(|e| e.to_string())?;
        message.push(b'\n');
        stream.write_all(&message).map_err(|e| e.to_string())?;
        let mut line = String::new();
        use std::io::Read;
        BufReader::new(stream.take(64 * 1024)).read_line(&mut line).map_err(|e| e.to_string())?;
        serde_json::from_str(&line).map_err(|e| e.to_string())
    };
    let response = call().map_err(|e| PluginError::new("prepare-unavailable", e))?;
    if response["ok"] != true {
        return Err(PluginError::new(
            "prepare-unavailable",
            format!(
                "apply the binary preparation configuration first: {}",
                response["error"]["message"].as_str().unwrap_or("preparer unavailable")
            ),
        ));
    }
    Ok(json!({"scheduled":true,"instance":mp::progress::PREPARER}))
}

#[cfg(not(unix))]
fn request_download(_req: &PluginRequest) -> Result<Value, PluginError> {
    Err(PluginError::new("unsupported", "binary preparation needs a Unix planner socket"))
}

impl Plugin for MihomoPkg {
    fn validate(&self, req: &PluginRequest) -> Result<ValidateResult, PluginError> {
        Ok(ValidateResult { errors: mp::validate(&mp::load(req)?, &assets()), warnings: Vec::new() })
    }

    /// Auto mode: a supervised preparer downloads/verifies in the background with backoff; installation and
    /// planner start never wait for the network. Manual mode needs no process.
    fn plan(&self, req: &PluginRequest) -> Result<PlanResult, PluginError> {
        let cfg = mp::load(req)?;
        let errors = mp::validate(&cfg, &assets());
        if !errors.is_empty() {
            return Err(PluginError::invalid(errors));
        }
        if cfg.mode != mp::Mode::AutoDownload {
            return Ok(PlanResult::default());
        }
        let spec = serde_json::to_string(&cfg).map_err(|e| PluginError::new("internal", e.to_string()))?;
        let libexec = req.context.libexec_dir.trim_end_matches('/');
        Ok(PlanResult {
            plans: vec![GenerationPlan {
                protocol_version: 1,
                request_id: req.request_id.clone(),
                instance: mp::progress::PREPARER.into(),
                files: vec![],
                processes: vec![ProcessSpec {
                    id: mp::progress::PREPARER.into(),
                    provider_slot: None,
                    program: Some(format!("{libexec}/plugins/mihomo-pkg")),
                    argv: vec![
                        "prepare".into(),
                        "--state-root".into(),
                        "${STATE_DIR}".into(),
                        "--planner-socket".into(),
                        "${RUN_DIR}/planner.sock".into(),
                        "--spec".into(),
                        spec,
                    ],
                    env: Default::default(),
                    restart: RestartPolicy::Always,
                    startup: Default::default(),
                    stop_timeout_seconds: 5,
                    reload_signal: None,
                    restart_digest: None,
                    user: None,
                    bound_to: None,
                }],
                resources: vec![],
                depends_on: vec![],
                probes: vec![],
                checks: vec![],
                reload: vec![],
            }],
            warnings: vec![],
        })
    }

    fn probe(&self, req: &PluginRequest) -> Result<ProbeResult, PluginError> {
        let cfg = mp::load(req)?;
        let d = mp::Dirs::new(&req.context.state_dir, &cfg);
        let st = mp::load_state(&d);
        let progress = if cfg.mode == mp::Mode::AutoDownload { mp::progress::load(&d) } else { None };
        let mut detail = BTreeMap::new();
        detail.insert(
            "mode".into(),
            json!(match cfg.mode {
                mp::Mode::Manual => "manual",
                mp::Mode::AutoDownload => "auto-download",
            }),
        );
        detail.insert("dataDir".into(), Value::String(d.data_dir.to_string_lossy().into_owned()));
        detail.insert("adapterVersion".into(), Value::String(env!("CARGO_PKG_VERSION").into()));
        detail.insert("packageVersion".into(), Value::String(env!("CARGO_PKG_VERSION").into()));
        detail.insert("upstreamVersion".into(), json!(st.as_ref().and_then(|s| s.version.as_deref())));
        detail.insert("activeSource".into(), json!(req.providers.get("mihomo.binary").map(|p| &p.source)));
        detail.insert("downloadProgress".into(), json!(progress));
        detail.insert("preparerProcess".into(), json!(mp::progress::PREPARER));
        if let Some(s) = &st {
            for (k, v) in [
                ("source", json!(s.source)),
                ("version", json!(s.version)),
                ("path", json!(s.path)),
                ("platform", json!(s.platform)),
                ("sha256", json!(s.sha256)),
                ("verified", json!(s.verified)),
                ("downloadStage", json!(s.download_stage)),
                ("providerState", json!(s.state)),
            ] {
                detail.insert(k.into(), v);
            }
        }
        if let Some(p) = &progress {
            detail.insert("downloadStage".into(), json!(p.phase));
        }
        let state = match st.as_ref().map(|s| s.state.as_str()) {
            Some("registered") => ServiceState::Up,
            _ if progress.as_ref().is_some_and(|p| p.active || p.phase == "retrying") => ServiceState::Starting,
            _ if progress.as_ref().is_some_and(|p| matches!(p.phase.as_str(), "failed" | "interrupted")) => ServiceState::Down,
            Some("invalid") | Some("download-failed") => ServiceState::Down,
            _ => ServiceState::Pending,
        };
        Ok(ProbeResult {
            instances: vec![InstanceStatus {
                id: "mihomo-pkg/provider".into(),
                kind: "mihomo-provider".into(),
                state,
                observed_at: Some(meduza_np_plugin_sdk::now_rfc3339()),
                reason: if progress.as_ref().is_some_and(|p| p.active) {
                    None
                } else {
                    progress.and_then(|p| p.error).or_else(|| st.and_then(|s| s.last_error))
                },
                detail,
            }],
        })
    }

    /// `check-local` / `activate` (manual and offline flow), `download` (auto mode on demand).
    fn action(&self, req: &PluginRequest) -> Result<Value, PluginError> {
        let cfg = mp::load(req)?;
        let d = mp::Dirs::new(&req.context.state_dir, &cfg);
        let st = match req.params.get("action").and_then(Value::as_str) {
            Some("check-local") | Some("activate") => {
                // Shipped digests cover the compressed release assets, so a manually copied (extracted) file
                // is recorded as user-provided with its computed SHA-256 — never as "verified official" (§3.3.2).
                let mut st = mp::check_local(&d, &cfg, None);
                if req.params.get("action").and_then(Value::as_str) == Some("check-local") && st.state == "registered" {
                    st.download_stage = Some("checked".into());
                }
                st
            }
            Some("download") => {
                if cfg.mode != mp::Mode::AutoDownload {
                    return Err(PluginError::new(
                        "manual-mode",
                        "manual mode never downloads; copy the binary into the data directory and run check-local",
                    ));
                }
                return request_download(req);
            }
            other => return Err(PluginError::new("unsupported", format!("unknown action {other:?}"))),
        };
        mp::save_state(&d, &st).map_err(|e| PluginError::new("internal", e))?;
        Ok(serde_json::to_value(&st).unwrap_or_default())
    }
}

fn prepare(args: &[String]) -> ! {
    let get = |k: &str| args.windows(2).find(|w| w[0] == k).map(|w| w[1].clone());
    let (Some(root), Some(socket), Some(spec)) = (get("--state-root"), get("--planner-socket"), get("--spec")) else {
        eprintln!("usage: prepare --state-root DIR --planner-socket PATH --spec JSON");
        std::process::exit(2)
    };
    let cfg: mp::BinarySourceConfig = serde_json::from_str(&spec).unwrap_or_else(|e| {
        eprintln!("invalid spec: {e}");
        std::process::exit(2)
    });
    let d = mp::Dirs::new(&root, &cfg);
    let _owner = mp::progress::preparer(&d).unwrap_or_else(|e| {
        eprintln!("cannot own binary preparer: {e}");
        std::process::exit(2)
    });
    let mut failures = 0u32;
    loop {
        let current = mp::load_state(&d);
        let done = current
            .as_ref()
            .map(|s| s.state == "registered" && s.version == cfg.target_version && s.verified && d.binary().is_file())
            .unwrap_or(false);
        if done {
            if let Err(e) = mp::cleanup_downloads(&d, &cfg, &assets()) {
                if e != mp::progress::BUSY {
                    eprintln!("cannot clean binary downloads: {e}");
                }
            }
            std::thread::sleep(Duration::from_secs(30));
            continue;
        }
        match mp::download(&d, &cfg, &assets()) {
            Ok(st) => {
                failures = 0;
                let _ = mp::save_state(&d, &st);
                eprintln!("mihomo {} prepared ({})", st.version.unwrap_or_default(), st.path);
                request_rescan(&socket);
            }
            Err(e) => {
                if e == mp::progress::BUSY {
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
                failures = failures.saturating_add(1);
                eprintln!("mihomo download failed: {e}");
                let mut st = current.unwrap_or_default();
                // The previous usable binary stays registered; only a first failure reports download-failed.
                if st.state != "registered" {
                    st.state = "download-failed".into();
                }
                if st.path.is_empty() {
                    st.path = d.binary().to_string_lossy().into_owned();
                }
                st.mode = "auto-download".into();
                st.last_error = Some(e);
                st.updated_at = meduza_np_plugin_sdk::now_rfc3339();
                let _ = mp::save_state(&d, &st);
                // Exponential backoff with jitter: 1, 2, 4 … 60 minutes.
                let base = 60u64 << failures.saturating_sub(1).min(6);
                let jitter = u64::from(std::process::id()) % 30;
                let delay = Duration::from_secs(base.min(3600) + jitter);
                mp::progress::retry(&d, delay, st.last_error.as_deref().unwrap_or("download failed"));
                std::thread::sleep(delay);
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("prepare") {
        prepare(&args[2..]);
    }
    run(&MihomoPkg, env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use meduza_np_plugin_sdk::{test_request, ResolvedProvider};

    #[cfg(unix)]
    #[test]
    fn download_action_schedules_the_preparer_and_reports_unapplied_configuration() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        for success in [true, false] {
            let root = std::env::temp_dir().join(format!("mnp-binary-action-{}-{success}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            let listener = UnixListener::bind(root.join("planner.sock")).unwrap();
            listener.set_nonblocking(true).unwrap();
            let server = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(10))
                        }
                        other => panic!("preparer restart request not received: {other:?}"),
                    }
                };
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["method"], "instance.restart");
                assert_eq!(request["params"]["instance"], mp::progress::PREPARER);
                writeln!(stream, "{}", json!({"protocol":1,"id":1,"ok":success,"error":{"message":"preparer is not applied"}})).unwrap();
            });
            let mut request = test_request("action", json!({"services":{"mihomo-pkg":{"mode":"auto-download"}}}));
            request.context.run_dir = root.to_string_lossy().into_owned();
            request.context.state_dir = root.join("state").to_string_lossy().into_owned();
            request.params = json!({"action":"download"});
            let result = MihomoPkg.action(&request);
            if success {
                assert_eq!(result.unwrap()["scheduled"], true);
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.code, "prepare-unavailable");
                assert!(error.message.contains("apply the binary preparation configuration first"));
            }
            server.join().unwrap();
            assert!(!root.join(".staging").exists(), "the action must never start a synchronous transfer");
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn resource_probe_separates_package_binary_and_active_provider_versions() {
        let root = std::env::temp_dir().join(format!("mnp-mihomo-resource-{}", std::process::id()));
        let mut req = test_request("probe", json!({"services":{"mihomo-pkg":{"mode":"manual"}}}));
        req.context.state_dir = root.to_string_lossy().into_owned();
        let dirs = mp::Dirs::new(&req.context.state_dir, &mp::load(&req).unwrap());
        let pending = MihomoPkg.probe(&req).unwrap().instances.remove(0);
        assert_eq!(pending.state, ServiceState::Pending);
        assert_eq!(pending.detail["packageVersion"], env!("CARGO_PKG_VERSION"));
        assert!(pending.detail["upstreamVersion"].is_null());
        assert!(pending.detail["activeSource"].is_null());
        let mut state = mp::ProviderState {
            state: "registered".into(),
            mode: "manual".into(),
            path: dirs.binary().to_string_lossy().into_owned(),
            version: Some("v1.19.17".into()),
            source: Some("user-provided".into()),
            ..Default::default()
        };
        mp::save_state(&dirs, &state).unwrap();
        let inactive = MihomoPkg.probe(&req).unwrap().instances.remove(0);
        assert!(inactive.detail["activeSource"].is_null(), "prepared metadata alone does not prove registry selection");
        req.providers.insert(
            "mihomo.binary".into(),
            ResolvedProvider { path: state.path.clone(), source: "user-provided".into(), version: state.version.clone() },
        );
        let active = MihomoPkg.probe(&req).unwrap().instances.remove(0);
        assert_eq!(active.detail["packageVersion"], env!("CARGO_PKG_VERSION"));
        assert_eq!(active.detail["upstreamVersion"], "v1.19.17");
        assert_eq!(active.detail["activeSource"], "user-provided");
        assert_eq!(active.detail["version"], "v1.19.17", "preserve the existing status contract");
        req.providers.clear();
        state.state = "download-failed".into();
        state.last_error = Some("download interrupted".into());
        state.download_stage = Some("fetching".into());
        mp::save_state(&dirs, &state).unwrap();
        let failed = MihomoPkg.probe(&req).unwrap().instances.remove(0);
        assert_eq!(failed.state, ServiceState::Down);
        assert_eq!(failed.detail["downloadStage"], "fetching");
        assert_eq!(failed.reason.as_deref(), Some("download interrupted"));
        let transfer = mp::progress::Transfer::start(&dirs, &dirs.state_dir.join("asset.gz"), "v1.20.0", "linux/amd64", 1000).unwrap();
        let owner = mp::progress::preparer(&dirs).unwrap();
        std::fs::write(dirs.state_dir.join("asset.gz"), [0u8; 1000]).unwrap();
        transfer.stage("verifying").unwrap();
        assert!(
            MihomoPkg.probe(&req).unwrap().instances[0].detail["downloadProgress"].is_null(),
            "manual mode does not display an old automatic transfer as current"
        );
        req.config.services.insert("mihomo-pkg".into(), json!({"mode":"auto-download"}));
        let verifying = MihomoPkg.probe(&req).unwrap().instances.remove(0);
        assert_eq!(verifying.state, ServiceState::Starting);
        assert_eq!(verifying.detail["downloadProgress"]["phase"], "verifying");
        assert_eq!(verifying.detail["downloadProgress"]["downloadedBytes"], 1000);
        assert_eq!(verifying.detail["preparerProcess"], mp::progress::PREPARER);
        assert!(verifying.reason.is_none(), "an active retry hides the previous failed attempt's error");
        drop(transfer);
        let interrupted = MihomoPkg.probe(&req).unwrap().instances.remove(0);
        assert_eq!(interrupted.detail["downloadStage"], "interrupted");
        assert_eq!(interrupted.state, ServiceState::Down);
        mp::progress::retry(&dirs, Duration::from_secs(60), "TLS EOF");
        let retry = MihomoPkg.probe(&req).unwrap().instances.remove(0);
        assert_eq!(retry.state, ServiceState::Starting);
        assert_eq!(retry.detail["downloadProgress"]["phase"], "retrying");
        assert!(retry.detail["downloadProgress"]["retryAtUnix"].is_number());
        drop(owner);
        std::fs::remove_dir_all(root).unwrap();
    }
}
