//! Bounded, node-local FRR forwarding-route snapshot for authenticated Access connect hooks.
use std::sync::Arc;
use std::time::{Duration, Instant};

use ipnet::Ipv4Net;
use meduza_np_plugin_sdk::{PluginContext, PluginRequest};
use serde_json::{json, Value};

use crate::sync::Planner;

#[derive(Default)]
pub struct Cache {
    entry: Option<Entry>,
}

struct Entry {
    revision: String,
    at: Instant,
    result: Result<Vec<Ipv4Net>, String>,
}

/// Pool routes never leave the server through the VPN client; never turn this option into a default route.
fn client_routes(routes: &[Ipv4Net], pools: &[Ipv4Net]) -> Result<Vec<String>, String> {
    let routes: Vec<_> = routes.iter().filter(|net| net.prefix_len() > 0 && !pools.iter().any(|p| p.contains(*net))).collect();
    if routes.len() > 256 {
        return Err("FRR Access push exceeds 256 routes; use route aggregation".into());
    }
    Ok(routes.iter().map(|net| format!("route {} {}", net.network(), net.netmask())).collect())
}

pub async fn push(p: &Arc<Planner>, instance: &str) -> Result<Vec<String>, String> {
    let applied = crate::state::AppliedState::load(&p.paths).map_err(|e| e.to_string())?;
    if !applied.instances.contains_key(&format!("openvpn/{instance}")) {
        return Err("Access instance is not applied".into());
    }
    let mut lg = crate::state::load_last_good(&p.paths).map_err(|e| e.to_string())?.ok_or("Access configuration unavailable")?;
    let service = lg.config.services.get("openvpn").ok_or("Access configuration unavailable")?;
    if service.get("enabled").and_then(Value::as_bool) == Some(false) {
        return Err("OpenVPN is disabled".into());
    }
    let instances = service.get("instances").and_then(Value::as_array).ok_or("Access instances unavailable")?;
    let enabled_access =
        |v: &&Value| v.get("enabled").and_then(Value::as_bool) != Some(false) && v.get("kind").and_then(Value::as_str) == Some("access");
    let inst = instances
        .iter()
        .filter(enabled_access)
        .find(|i| i.get("id").and_then(Value::as_str) == Some(instance))
        .ok_or("Access instance disabled")?;
    if inst.pointer("/access/pushRoutes/frr").and_then(Value::as_bool) != Some(true) {
        return Ok(Vec::new());
    }
    let pools: Vec<Ipv4Net> = instances
        .iter()
        .filter(enabled_access)
        .filter_map(|i| i.pointer("/access/pool").and_then(Value::as_str).and_then(|s| s.parse().ok()))
        .collect();
    let registry = p.registry();
    let pkg = registry.get("meduza-np-frr").filter(|pkg| pkg.registered()).ok_or("FRR is not registered")?;
    let program = registry.plugin_path(pkg).ok_or("FRR plugin unavailable")?;
    let provider = registry.providers.get("frr.daemons").ok_or("FRR provider unavailable")?.clone();
    let mut cache = p.access_routes.lock().await;
    if let Some(entry) = &cache.entry {
        if entry.revision == lg.digest && entry.at.elapsed() < Duration::from_secs(5) {
            return client_routes(entry.result.as_ref().map_err(Clone::clone)?, &pools);
        }
    }
    lg.config.secrets.clear();
    let req = PluginRequest {
        protocol_version: meduza_np_protocol::PLUGIN_PROTOCOL_VERSION,
        request_id: "access-frr-routes".into(),
        operation: "action".into(),
        config: lg.config,
        instance: None,
        providers: [("frr.daemons".into(), provider)].into(),
        context: PluginContext {
            generation_root: p.paths.current_generation_link().to_string_lossy().into_owned(),
            state_dir: p.paths.state.to_string_lossy().into_owned(),
            run_dir: p.paths.run.to_string_lossy().into_owned(),
            libexec_dir: p.paths.libexec.to_string_lossy().into_owned(),
            platform: p.platform.clone(),
            capabilities: registry.capabilities(),
        },
        params: json!({"action":"access-routes"}),
    };
    let result = match tokio::time::timeout(Duration::from_secs(6), p.engine.runner.call(&program, &req)).await {
        Ok(Ok(result)) => result
            .get("routes")
            .cloned()
            .ok_or("FRR route export missing".into())
            .and_then(|v| serde_json::from_value::<Vec<Ipv4Net>>(v).map_err(|_| "invalid FRR route export".into())),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("FRR Access route query timed out".into()),
    };
    cache.entry = Some(Entry { revision: lg.digest, at: Instant::now(), result: result.clone() });
    client_routes(&result?, &pools)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn excludes_all_access_pools_and_default_and_bounds_push_size() {
        let routes = ["0.0.0.0/0", "10.8.0.0/24", "10.8.0.128/25", "10.9.0.0/24", "192.0.2.0/24"].map(|s| s.parse().unwrap());
        let pools = ["10.8.0.0/24", "10.9.0.0/24"].map(|s| s.parse().unwrap());
        assert_eq!(client_routes(&routes, &pools).unwrap(), ["route 192.0.2.0 255.255.255.0"]);
        assert!(client_routes(&vec!["192.0.2.0/24".parse().unwrap(); 257], &[]).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn connect_queries_registered_plugin_and_refreshes_without_secrets_or_stale_error_fallback() {
        use crate::{
            apply::Engine,
            keystore::KeyStore,
            netbackend::RecordingBackend,
            plugins::PluginRunner,
            registry::{Package, Registry},
            state::{AppliedInstance, AppliedState},
            supervisor::Supervisor,
        };
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("mnp-access-routes-{}", std::process::id()));
        let paths = meduza_np_runtime::paths::NodePaths::under(&root);
        std::fs::create_dir_all(&root).unwrap();
        let program = root.join("frr-plugin");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\ncat > '{}/request.json'\necho query >> '{}/calls'\ncat '{}/response.json'\n",
                root.display(),
                root.display(),
                root.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut manifest: meduza_np_protocol::manifest::PackageManifest =
            serde_json::from_str(include_str!("../../meduza-np-frr/manifest.json")).unwrap();
        manifest.entrypoint = Some(program.to_string_lossy().into_owned());
        let mut registry = Registry::default();
        registry.packages.insert(
            "meduza-np-frr".into(),
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
        registry.providers.insert(
            "frr.daemons".into(),
            meduza_np_plugin_sdk::ResolvedProvider {
                path: root.join("zebra").to_string_lossy().into_owned(),
                source: "system".into(),
                version: None,
            },
        );
        let engine = Engine {
            paths: paths.clone(),
            runner: PluginRunner::new(1, Duration::from_secs(2)),
            supervisor: Supervisor::persistent(&paths.run.join("supervisor")),
            net: Arc::new(RecordingBackend::default()),
            keystore: KeyStore::new(&paths.etc.join("keys")),
            settle: Duration::ZERO,
            settle_deadline: Duration::from_secs(1),
            loaded: Default::default(),
        };
        let p = Planner::new(paths.clone(), Default::default(), engine, registry);
        let mut applied = AppliedState::default();
        applied.instances.insert("openvpn/access".into(), AppliedInstance { package: "meduza-np-openvpn".into(), ..Default::default() });
        applied.save(&paths).unwrap();
        let mut raw = json!({"schemaVersion":1,"revision":"rev-000001","node":{"id":"node"},"services":{
            "frr":{"localAsn":65001},"openvpn":{"instances":[{"id":"access","kind":"access","access":{"pool":"10.8.0.0/24","pushRoutes":{"frr":true}}}]}
        },"secrets":{"private":{"kind":"text","value":"never-deliver-to-frr"}}});
        let save = |raw: &Value| {
            crate::state::save_last_good(&paths, raw, &meduza_np_protocol::digest::canonical_digest(raw).unwrap(), &[]).unwrap()
        };
        save(&raw);
        let respond = |result: Value| {
            std::fs::write(
                root.join("response.json"),
                json!({"protocolVersion":1,"requestId":"access-frr-routes","ok":true,"result":result}).to_string(),
            )
            .unwrap()
        };
        respond(json!({"routes":["0.0.0.0/0","10.8.0.0/24","192.0.2.0/24"]}));
        assert_eq!(push(&p, "access").await.unwrap(), ["route 192.0.2.0 255.255.255.0"]);
        let request: Value = serde_json::from_slice(&std::fs::read(root.join("request.json")).unwrap()).unwrap();
        assert!(request["config"].get("secrets").is_none());
        assert_eq!(request["params"]["action"], "access-routes");
        respond(json!({"routes":["198.51.100.0/24"]}));
        assert_eq!(push(&p, "access").await.unwrap(), ["route 192.0.2.0 255.255.255.0"], "short cache coalesces connections");
        p.access_routes.lock().await.entry.as_mut().unwrap().at = Instant::now() - Duration::from_secs(6);
        assert_eq!(push(&p, "access").await.unwrap(), ["route 198.51.100.0 255.255.255.0"], "withdrawn routes disappear on a new snapshot");
        p.access_routes.lock().await.entry.as_mut().unwrap().at = Instant::now() - Duration::from_secs(6);
        std::fs::write(root.join("response.json"),json!({"protocolVersion":1,"requestId":"access-frr-routes","ok":false,"error":{"code":"routes-unavailable","message":"query failed"}}).to_string()).unwrap();
        assert!(push(&p, "access").await.is_err());
        assert!(push(&p, "access").await.is_err(), "query failure never falls back to stale routes");
        assert_eq!(std::fs::read_to_string(root.join("calls")).unwrap().lines().count(), 3);
        raw["services"]["openvpn"]["instances"][0]["access"]["pushRoutes"]["frr"] = json!(false);
        save(&raw);
        assert!(push(&p, "access").await.unwrap().is_empty());
        raw["services"]["openvpn"]["instances"][0]["enabled"] = json!(false);
        save(&raw);
        assert!(push(&p, "access").await.is_err());
        assert!(push(&p, "other").await.is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
