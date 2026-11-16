//! Optional, on-demand diagnostics. No generic remote plugin or shell endpoint.
use crate::sync::Planner;
use meduza_np_plugin_sdk::{PluginContext, PluginRequest};
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

static SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
static LAST_QUERY: Mutex<Option<Instant>> = Mutex::new(None);

pub fn available(p: &Arc<Planner>) -> bool {
    let reg = p.registry();
    reg.get("meduza-np-lookingglass").is_some_and(|pkg| pkg.registered() && reg.plugin_path(pkg).is_some_and(|path| path.is_file()))
}

pub async fn query(p: &Arc<Planner>, params: Value) -> Result<Value, (String, String)> {
    let err = |code: &str, msg: String| (code.to_string(), msg);
    let reg = p.registry();
    let pkg = reg
        .get("meduza-np-lookingglass")
        .filter(|pkg| pkg.registered())
        .ok_or_else(|| err("not-found", "LookingGlass is not installed and registered".into()))?;
    let program =
        reg.plugin_path(pkg).filter(|path| path.is_file()).ok_or_else(|| err("not-found", "LookingGlass executable unavailable".into()))?;
    for (operation, package, slot) in [("mtr", "meduza-np-mtr-pkg", "mtr.binary"), ("iperf", "meduza-np-iperf3-pkg", "iperf3.binary")] {
        if params.get("operation").and_then(Value::as_str) == Some(operation)
            && reg.get(package).is_some()
            && (!reg.get(package).is_some_and(|p| p.registered()) || !reg.providers.contains_key(slot))
        {
            return Err(err("unavailable", format!("installed provider is unavailable; repair {package} before querying")));
        }
    }
    let _permit = SLOT.try_acquire().map_err(|_| err("busy", "another LookingGlass query is running".into()))?;
    {
        let mut last = LAST_QUERY.lock().unwrap();
        if last.is_some_and(|at| at.elapsed() < Duration::from_secs(2)) {
            return Err(err("busy", "wait two seconds between queries".into()));
        }
        *last = Some(Instant::now());
    }
    // This package consumes no configuration. Do not read node secrets just to discard them,
    // and allow diagnostics before the first configuration is applied.
    let config = serde_json::from_value(json!({"schemaVersion":1,"revision":"rev-000001","node":{"id":"lookingglass"}}))
        .expect("valid diagnostic context");
    let req = PluginRequest {
        protocol_version: meduza_np_protocol::PLUGIN_PROTOCOL_VERSION,
        request_id: "lookingglass".into(),
        operation: "action".into(),
        config,
        instance: None,
        providers: reg
            .providers
            .iter()
            .filter(|(slot, _)| matches!(slot.as_str(), "mtr.binary" | "iperf3.binary" | "frr.daemons"))
            .map(|(slot, provider)| (slot.clone(), provider.clone()))
            .collect(),
        context: PluginContext {
            generation_root: p.paths.current_generation_link().to_string_lossy().into_owned(),
            state_dir: p.paths.state.to_string_lossy().into_owned(),
            run_dir: p.paths.run.to_string_lossy().into_owned(),
            libexec_dir: p.paths.libexec.to_string_lossy().into_owned(),
            platform: p.platform.clone(),
            capabilities: reg.capabilities(),
        },
        params: json!({"action":"query","operation":params.get("operation"),"target":params.get("target")}),
    };
    crate::plugins::PluginRunner::new(1, Duration::from_secs(25)).call(&program, &req).await.map_err(|e| (e.code(), e.to_string()))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{
        apply::Engine,
        keystore::KeyStore,
        netbackend::RecordingBackend,
        plugins::PluginRunner,
        registry::{Package, Registry},
        supervisor::Supervisor,
    };
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn gates_registration_authorization_and_executable_and_bounds_queries_without_secrets() {
        let root = std::env::temp_dir().join(format!("mnp-planner-lg-{}", std::process::id()));
        let paths = meduza_np_runtime::paths::NodePaths::under(&root);
        std::fs::create_dir_all(&root).unwrap();
        let make = |registry| {
            Planner::new(
                paths.clone(),
                Default::default(),
                Engine {
                    paths: paths.clone(),
                    runner: PluginRunner::new(1, Duration::from_secs(2)),
                    supervisor: Supervisor::persistent(&paths.run.join("supervisor")),
                    net: Arc::new(RecordingBackend::default()),
                    keystore: KeyStore::new(&paths.etc.join("keys")),
                    settle: Duration::ZERO,
                    settle_deadline: Duration::from_secs(1),
                    loaded: Default::default(),
                },
                registry,
            )
        };
        let absent = make(Registry::default());
        assert!(!available(&absent));
        assert_eq!(query(&absent, json!({})).await.unwrap_err().0, "not-found");
        let local = meduza_np_protocol::local::LocalRequest { protocol: 1, id: 1, method: "lookingglass.query".into(), params: json!({}) };
        assert_eq!(crate::local_api::dispatch(&absent, local, false).await.error.unwrap().code, "forbidden");
        let program = root.join("plugin");
        std::fs::write(&program, format!("#!/bin/sh\ncat > '{}/request.json'\nprintf '%s\\n' '{{\"protocolVersion\":1,\"requestId\":\"lookingglass\",\"ok\":true,\"result\":{{\"results\":[]}}}}'\n",root.display())).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut manifest: meduza_np_protocol::manifest::PackageManifest =
            serde_json::from_str(include_str!("../../meduza-np-lookingglass/manifest.json")).unwrap();
        manifest.entrypoint = Some(program.to_string_lossy().into_owned());
        let mut registry = Registry::default();
        for (slot, source) in [
            ("mtr.binary", "meduza-np-mtr-pkg"),
            ("iperf3.binary", "meduza-np-iperf3-pkg"),
            ("frr.daemons", "system"),
            ("unrelated.binary", "other"),
        ] {
            registry.providers.insert(
                slot.into(),
                meduza_np_plugin_sdk::ResolvedProvider {
                    path: program.to_string_lossy().into_owned(),
                    source: source.into(),
                    version: None,
                },
            );
        }
        registry.packages.insert(
            "meduza-np-lookingglass".into(),
            Package {
                manifest,
                dir: root.clone(),
                manifest_digest: String::new(),
                ui_digest: None,
                state: meduza_np_protocol::report::RegistrationState::Registered,
                reason_code: None,
                reason: None,
            },
        );
        let p = make(registry);
        let raw =
            json!({"schemaVersion":1,"revision":"rev-000001","node":{"id":"test"},"secrets":{"key":{"kind":"text","value":"do-not-send"}}});
        crate::state::save_last_good(&paths, &raw, &meduza_np_protocol::digest::canonical_digest(&raw).unwrap(), &[]).unwrap();
        assert!(available(&p));
        assert_eq!(query(&p, json!({"operation":"ping","target":"10.0.0.1"})).await.unwrap(), json!({"results":[]}));
        let req: Value = serde_json::from_slice(&std::fs::read(root.join("request.json")).unwrap()).unwrap();
        assert!(req["config"].get("secrets").is_none());
        assert_eq!(req["config"]["node"]["id"], "lookingglass", "does not forward the node configuration");
        assert_eq!(req["params"], json!({"action":"query","operation":"ping","target":"10.0.0.1"}));
        assert_eq!(req["providers"]["mtr.binary"]["source"], "meduza-np-mtr-pkg");
        assert_eq!(req["providers"]["iperf3.binary"]["source"], "meduza-np-iperf3-pkg");
        assert_eq!(req["providers"]["frr.daemons"]["source"], "system");
        assert!(req["providers"].get("unrelated.binary").is_none());
        let mut broken_registry = (*p.registry()).clone();
        broken_registry.providers.remove("iperf3.binary");
        broken_registry.packages.insert(
            "meduza-np-iperf3-pkg".into(),
            Package {
                manifest: serde_json::from_str(include_str!("../../meduza-np-iperf3-pkg/manifest.json")).unwrap(),
                dir: root.clone(),
                manifest_digest: String::new(),
                ui_digest: None,
                state: meduza_np_protocol::report::RegistrationState::Registered,
                reason_code: None,
                reason: None,
            },
        );
        assert_eq!(
            query(&make(broken_registry), json!({"operation":"iperf","target":"10.0.0.1"})).await.unwrap_err().0,
            "unavailable",
            "installed but unresolved iperf provider fails before query dispatch"
        );
        assert_eq!(query(&p, json!({})).await.unwrap_err().0, "busy");
        *LAST_QUERY.lock().unwrap() = None;
        let permit = SLOT.try_acquire().unwrap();
        assert_eq!(query(&p, json!({})).await.unwrap_err().0, "busy");
        drop(permit);
        std::fs::remove_file(&program).unwrap();
        assert!(!available(&p));
        assert_eq!(query(&p, json!({})).await.unwrap_err().0, "not-found");
        let _ = std::fs::remove_dir_all(root);
    }
}
