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
                instance: "mihomo-pkg/prepare".into(),
                files: vec![],
                processes: vec![ProcessSpec {
                    id: "mihomo-pkg/prepare".into(),
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
        let state = match st.as_ref().map(|s| s.state.as_str()) {
            Some("registered") => ServiceState::Up,
            Some("invalid") | Some("download-failed") => ServiceState::Down,
            _ => ServiceState::Pending,
        };
        Ok(ProbeResult {
            instances: vec![InstanceStatus {
                id: "mihomo-pkg/provider".into(),
                kind: "mihomo-provider".into(),
                state,
                observed_at: Some(meduza_np_plugin_sdk::now_rfc3339()),
                reason: st.and_then(|s| s.last_error),
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
                mp::download(&d, &cfg, &assets()).map_err(|e| PluginError::new("download-failed", e))?
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
    let mut failures = 0u32;
    loop {
        let current = mp::load_state(&d);
        let done = current.as_ref().map(|s| s.state == "registered" && s.version == cfg.target_version && s.verified).unwrap_or(false);
        if done {
            std::thread::sleep(Duration::from_secs(6 * 3600));
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
                failures += 1;
                eprintln!("mihomo download failed: {e}");
                let mut st = current.unwrap_or_default();
                // The previous usable binary stays registered; only a first failure reports download-failed.
                if st.state != "registered" {
                    st.state = "download-failed".into();
                }
                st.mode = "auto-download".into();
                st.last_error = Some(e);
                st.updated_at = meduza_np_plugin_sdk::now_rfc3339();
                let _ = mp::save_state(&d, &st);
                // Exponential backoff with jitter: 1, 2, 4 … 60 minutes.
                let base = 60u64 << failures.min(6);
                let jitter = u64::from(std::process::id()) % 30;
                std::thread::sleep(Duration::from_secs(base.min(3600) + jitter));
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
        std::fs::remove_dir_all(root).unwrap();
    }
}
