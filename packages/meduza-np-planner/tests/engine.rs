//! Apply engine integration test with real plugin executables (the WireGuard plugin and a test plugin from
//! `examples/`), a fake native tool, the real supervisor and a recording network backend.
//! First run `cargo build --locked -p meduza-np-wireguard -p meduza-np-openvpn -p meduza-np-planner --bins --examples`,
//! then `CI=true cargo test --locked --workspace` to require all executable dependencies.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meduza_np_planner::apply::Engine;
use meduza_np_planner::keystore::KeyStore;
use meduza_np_planner::netbackend::RecordingBackend;
use meduza_np_planner::plugins::PluginRunner;
use meduza_np_planner::registry;
use meduza_np_planner::supervisor::Supervisor;
use meduza_np_protocol::digest::canonical_digest;
use meduza_np_runtime::paths::NodePaths;
use serde_json::{json, Value};

const EXE: &str = std::env::consts::EXE_SUFFIX;
const PRIV: &str = "yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=";
const PEER: &str = "xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=";
// The native-tool recording hook is process-wide; keep cases from appending to another case's log.
static NATIVE_TOOL_ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn target_dir() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().parent().unwrap().to_path_buf() // target/<profile>/deps/x → target/<profile>
}

fn write_json(path: &Path, v: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_string_pretty(v).unwrap()).unwrap();
}

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Env {
    paths: NodePaths,
    engine: Engine,
    net: RecordingBackend,
    log: PathBuf,
    have_wg: bool,
    have_openvpn: bool,
}

fn setup(name: &str) -> Option<Env> {
    let target = target_dir();
    let fake_tool = target.join("examples").join(format!("fake_tool{EXE}"));
    let fake_plugin = target.join("examples").join(format!("fake_plugin{EXE}"));
    let wg_plugin = target.join(format!("meduza-np-wireguard{EXE}"));
    if !fake_tool.exists() || !fake_plugin.exists() {
        // CI must never skip silently: the workflow explicitly builds the executable dependencies first.
        assert!(std::env::var_os("CI").is_none(), "example binaries missing in CI: {}", fake_tool.display());
        eprintln!("skipping: example binaries not built ({}); run cargo build --examples", fake_tool.display());
        return None;
    }
    let root = std::env::temp_dir().join(format!("mnp-engine-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let paths = NodePaths::under(&root);
    let pkgs = &paths.packages;
    std::fs::copy(repo().join("packages/meduza-np-planner/manifest.json"), {
        std::fs::create_dir_all(pkgs.join("meduza-np-planner")).unwrap();
        pkgs.join("meduza-np-planner/manifest.json")
    })
    .unwrap();

    // Test plugin registered as the avahi adapter.
    let avahi = pkgs.join("meduza-np-avahi");
    write_json(
        &avahi.join("manifest.json"),
        &json!({
            "manifestVersion": 1, "package": "meduza-np-avahi", "version": "0.1.0", "role": "service-adapter",
            "plannerApi": {"min": 1, "max": 1}, "requires": ["meduza-np-planner"], "provides": ["mdns.avahi"],
            "providerSlot": "avahi.daemon",
            "schemas": {"config": "schema/config.json"}, "ui": {"descriptor": "ui/descriptor.json", "version": 1},
            "entrypoint": "/nonexistent/avahi", "operations": ["validate", "plan"]
        }),
    );
    write_json(
        &avahi.join("ui/descriptor.json"),
        &json!({
            "uiVersion": 1, "package": "meduza-np-avahi", "requiresCapability": "mdns.avahi", "title": "test",
            "settings": [{"path": "services.avahi", "title": "t", "sections": [{"title": "s", "fields": ["instances"]}]}]
        }),
    );
    std::fs::create_dir_all(avahi.join("bin")).unwrap();
    std::fs::copy(&fake_plugin, avahi.join(format!("bin/avahi{EXE}"))).unwrap();

    // Provider package serving both slots with the fake tool.
    write_json(
        &pkgs.join("meduza-np-openvpn-pkg/manifest.json"),
        &json!({
            "manifestVersion": 1, "package": "meduza-np-openvpn-pkg", "version": "0.1.0", "role": "binary-provider",
            "plannerApi": {"min": 1, "max": 1}, "requires": ["meduza-np-planner"], "provides": ["test.tools"],
            "providers": [
                {"slot": "avahi.daemon", "binary": fake_tool.to_string_lossy()},
                {"slot": "openvpn.binary", "binary": fake_tool.to_string_lossy()},
                {"slot": "wireguard.tools", "binary": fake_tool.to_string_lossy()}
            ]
        }),
    );

    let have_wg = wg_plugin.exists();
    assert!(
        have_wg || std::env::var_os("CI").is_none(),
        "WireGuard plugin binary missing in CI: {}; run cargo build --locked -p meduza-np-wireguard",
        wg_plugin.display()
    );
    if have_wg {
        let wg = pkgs.join("meduza-np-wireguard");
        let mut manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(repo().join("packages/meduza-np-wireguard/manifest.json")).unwrap()).unwrap();
        // The kernel module and iproute2 are host facts this test cannot provide; keep only the tools slot.
        manifest["systemDependencies"] = json!([{"kind": "binary", "name": "wg", "providerSlot": "wireguard.tools"}]);
        write_json(&wg.join("manifest.json"), &manifest);
        std::fs::create_dir_all(wg.join("ui")).unwrap();
        std::fs::copy(repo().join("packages/meduza-np-wireguard/ui/descriptor.json"), wg.join("ui/descriptor.json")).unwrap();
        std::fs::create_dir_all(wg.join("bin")).unwrap();
        std::fs::copy(&wg_plugin, wg.join(format!("bin/wireguard{EXE}"))).unwrap();
    } else {
        eprintln!("note: WireGuard plugin binary not built; WireGuard steps skipped");
    }

    let ovpn_plugin = target.join(format!("meduza-np-openvpn{EXE}"));
    let have_openvpn = ovpn_plugin.exists();
    assert!(have_openvpn || std::env::var_os("CI").is_none(), "OpenVPN test plugin binary missing: {}", ovpn_plugin.display());
    if have_openvpn {
        let dir = pkgs.join("meduza-np-openvpn");
        let mut manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(repo().join("packages/meduza-np-openvpn/manifest.json")).unwrap()).unwrap();
        manifest["systemDependencies"] = json!([{"kind":"binary","name":"openvpn","providerSlot":"openvpn.binary"}]);
        write_json(&dir.join("manifest.json"), &manifest);
        std::fs::create_dir_all(dir.join("ui")).unwrap();
        std::fs::copy(repo().join("packages/meduza-np-openvpn/ui/descriptor.json"), dir.join("ui/descriptor.json")).unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::copy(ovpn_plugin, dir.join(format!("bin/openvpn{EXE}"))).unwrap();
    }
    let log = root.join("tool.log");
    let net = RecordingBackend::default();
    let engine = engine(&paths, &net);
    Some(Env { paths, engine, net, log, have_wg, have_openvpn })
}

/// A fresh engine over the same node directories, like a newly started planner process.
fn engine(paths: &NodePaths, net: &RecordingBackend) -> Engine {
    Engine {
        paths: paths.clone(),
        runner: PluginRunner::new(2, Duration::from_secs(20)),
        supervisor: Supervisor::persistent(&paths.run.join("supervisor")),
        net: Arc::new(net.clone()),
        keystore: KeyStore::new(&paths.etc.join("keys")),
        settle: Duration::from_millis(300),
        settle_deadline: Duration::from_secs(3),
        loaded: Default::default(),
    }
}

fn config(env: &Env, rev: u64, avahi: Value) -> (Value, String) {
    let mut services = json!({ "avahi": { "instances": avahi } });
    if env.have_wg {
        services["wireguard"] = json!({"instances": [{
            "id": "uplink", "privateKeyRef": "wg-uplink", "listenPort": 51820, "addresses": ["10.9.0.2/24"],
            "peers": [{"id": "peer-b", "publicKey": PEER, "allowedIps": ["10.9.0.1/32"], "endpoint": "198.51.100.1:51820"}]
        }]});
    }
    let raw = json!({
        "schemaVersion": 1,
        "revision": format!("rev-{rev:06}"),
        "node": {"id": "gw-a"},
        "services": services,
        "secrets": {"wg-uplink": {"kind": "wireguard-private-key", "value": PRIV}}
    });
    let digest = canonical_digest(&raw).unwrap();
    (raw, digest)
}

fn log_lines(env: &Env) -> Vec<String> {
    std::fs::read_to_string(&env.log).unwrap_or_default().lines().map(str::to_string).collect()
}

fn tunnel_config(env: &Env, short: &str, rev: u64, ids: &[&str]) -> (Value, String) {
    let (mut raw, _) = config(env, rev, json!([{"id":"local"}]));
    raw["services"].as_object_mut().unwrap().remove("wireguard");
    raw["secrets"]["ovpn"] =
        json!({"kind":"openvpn-static-key","value":"-----BEGIN OpenVPN Static key V1-----\nab\n-----END OpenVPN Static key V1-----"});
    let instances: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            if short == "wireguard" {
                json!({"id":id,"privateKeyRef":"wg-uplink","listenPort":51820+index,"addresses":[format!("10.9.{index}.2/24")],"peers":[]})
            } else {
                json!({"id":id,"kind":"site2site","role":"server","staticKeyRef":"ovpn","port":1194+index,
            "ifconfig":{"local":format!("10.8.{index}.1"),"remote":format!("10.8.{index}.2")}})
            }
        })
        .collect();
    raw["services"][short] = json!({"instances":instances});
    let digest = canonical_digest(&raw).unwrap();
    (raw, digest)
}

#[tokio::test(flavor = "current_thread")]
async fn isolated_access_keeps_frr_context_without_replanning_other_services_or_accepting_broken_siblings() {
    let Some(env) = setup("access-frr-context") else { return };
    if !env.have_openvpn {
        return;
    }
    // Only the capability is needed for OpenVPN preparation; the separate FRR action tests cover route queries.
    write_json(
        &env.paths.packages.join("meduza-np-frr/manifest.json"),
        &json!({
            "manifestVersion":1,"package":"meduza-np-frr","version":"0.1.0","role":"binary-provider",
            "plannerApi":{"min":1,"max":1},"requires":["meduza-np-planner"],"provides":["frr.bgp"],
            "providers":[{"slot":"frr.daemons","binary":target_dir().join("examples").join(format!("fake_tool{EXE}"))}]
        }),
    );
    let reg = registry::scan(&env.paths.packages, &Default::default());
    assert!(reg.by_short("frr").unwrap().registered(), "{:?}", reg.by_short("frr").unwrap().reason);
    let (mut raw, _) = tunnel_config(&env, "openvpn", 1, &["good", "broken"]);
    raw["services"]["openvpn"]["instances"][1]["port"] = json!("invalid");
    raw["services"]["openvpn"]["instances"].as_array_mut().unwrap().push(json!({
        "id":"access","kind":"access","port":1443,"tls":{"caRef":"ca","certRef":"crt","keyRef":"key"},
        "access":{"pool":"10.88.0.0/24","pushRoutes":{"frr":true,"lan":true}}
    }));
    raw["secrets"]["ca"] = json!({"kind":"pem-certificate","value":"-----BEGIN CERTIFICATE-----\nCA\n-----END CERTIFICATE-----"});
    raw["secrets"]["crt"] = json!({"kind":"pem-certificate","value":"-----BEGIN CERTIFICATE-----\nX\n-----END CERTIFICATE-----"});
    raw["secrets"]["key"] = json!({"kind":"pem-private-key","value":"-----BEGIN PRIVATE KEY-----\nK\n-----END PRIVATE KEY-----"});
    raw["services"]["frr"] = json!({"enabled":true,"localAsn":65001});
    raw["networks"] = json!({"lan":["10.42.10.0/24"]});
    let prepare = |config: &Value| canonical_digest(config).unwrap();
    let prepared = env.engine.prepare(&raw, &prepare(&raw), &reg, &Default::default()).await.unwrap();
    assert_eq!(prepared.instance_errors.len(), 1, "only the broken sibling is rejected: {:?}", prepared.instance_errors);
    assert!(prepared.instance_errors.contains_key("openvpn/broken"));
    assert_eq!(prepared.plans.iter().filter(|(_, p)| p.instance == "avahi/local").count(), 1, "other services are planned once");
    assert_eq!(prepared.plans.iter().filter(|(_, p)| p.instance == "openvpn/good").count(), 1);
    let access = &prepared.plans.iter().find(|(_, p)| p.instance == "openvpn/access").unwrap().1;
    let config = &access.files.iter().find(|f| f.path.ends_with("config.ovpn")).unwrap().content;
    assert!(config.contains("setenv MEDUZA_NP_ACCESS_PUSH_FRR 1\n"));
    assert!(config.contains("push \"route 10.42.10.0 255.255.255.0\"\n"));

    for frr in [Some(json!({"enabled":false})), None] {
        if let Some(frr) = frr {
            raw["services"]["frr"] = frr;
        } else {
            raw["services"].as_object_mut().unwrap().remove("frr");
        }
        let prepared = env.engine.prepare(&raw, &prepare(&raw), &reg, &Default::default()).await.unwrap();
        assert!(prepared.instance_errors["openvpn/access"].message.contains("enabled FRR configuration"));
        assert!(prepared.plans.iter().any(|(_, p)| p.instance == "openvpn/good"));
    }
    raw["services"]["openvpn"]["instances"][2]["access"]["pushRoutes"]["frr"] = json!(false);
    let prepared = env.engine.prepare(&raw, &prepare(&raw), &reg, &Default::default()).await.unwrap();
    assert!(!prepared.instance_errors.contains_key("openvpn/access"), "option-off still works without FRR");
    std::fs::remove_dir_all(env.paths.state.parent().unwrap()).unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "current_thread")]
async fn openvpn_without_management_passes_checks_and_is_observed_via_its_owned_pid() {
    use meduza_np_planner::state::AppliedState;
    use meduza_np_plugin_sdk::{test_request, ProbeResult};
    use meduza_np_protocol::plan::CommandSpec;
    use std::os::unix::fs::PermissionsExt;
    let _native_env = NATIVE_TOOL_ENV.lock().await;
    let Some(env) = setup("openvpn-small") else { return };
    if !env.have_openvpn {
        return;
    }
    let checker = env.paths.etc.parent().unwrap().join("openvpn-small-check");
    std::fs::write(
        &checker,
        r#"#!/bin/sh
if ! grep -q '^ignore-unknown-option management$' "$2"; then
    echo 'Options error: Unrecognized option or missing or extra parameter(s): management (2.7.4)' >&2
    exit 1
fi
case "$2" in */broken/*) echo 'unsupported local cipher' >&2; exit 1 ;; esac
exit 0
"#,
    )
    .unwrap();
    std::fs::set_permissions(&checker, std::fs::Permissions::from_mode(0o755)).unwrap();
    let reg = registry::scan(&env.paths.packages, &Default::default());
    let (raw, digest) = tunnel_config(&env, "openvpn", 1, &["good", "broken", "later"]);
    let native_check = CommandSpec {
        provider_slot: None,
        program: Some(checker.to_string_lossy().into_owned()),
        argv: vec!["--config".into(), "${GENERATION_ROOT}/PLACEHOLDER/config.ovpn".into()],
        required: true,
        timeout_seconds: 3,
    };
    let mut prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.unwrap();
    for (_, plan) in prepared.plans.iter_mut().filter(|(p, _)| p == "meduza-np-openvpn") {
        let mut check = native_check.clone();
        check.argv[1] = format!("${{GENERATION_ROOT}}/{}/config.ovpn", plan.instance);
        plan.checks = vec![check];
        for file in &mut plan.files {
            file.content = file.content.replace("ignore-unknown-option management\n", "");
        }
    }
    env.engine.commit(&prepared, &reg).await.unwrap();
    let before = AppliedState::load(&env.paths).unwrap();
    assert_eq!(before.instance_errors.len(), 3, "reproduce the unsupported management error for every tunnel");
    assert!(before.instance_errors.values().all(|e| e.message.contains("management (2.7.4)")));
    let mut prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.unwrap();
    for (_, plan) in prepared.plans.iter_mut().filter(|(p, _)| p == "meduza-np-openvpn") {
        let mut check = native_check.clone();
        check.argv[1] = format!("${{GENERATION_ROOT}}/{}/config.ovpn", plan.instance);
        plan.checks = vec![check];
    }
    env.engine.commit(&prepared, &reg).await.unwrap();
    let after = AppliedState::load(&env.paths).unwrap();
    assert!(after.instances.contains_key("openvpn/good") && after.instances.contains_key("openvpn/later"));
    assert_eq!(after.instance_errors.len(), 1);
    assert_eq!(after.instance_errors["openvpn/broken"].code, "syntax-check-failed");
    assert!(after.instance_errors["openvpn/broken"].message.contains("unsupported local cipher"));
    let mut request = test_request("probe", raw);
    request.providers = reg.providers.clone();
    request.context.generation_root = env.paths.current_generation_link().to_string_lossy().into_owned();
    request.context.run_dir = env.paths.run.to_string_lossy().into_owned();
    let program = reg.plugin_path(reg.by_short("openvpn").unwrap()).unwrap();
    let observed: ProbeResult = serde_json::from_value(env.engine.runner.call(&program, &request).await.unwrap()).unwrap();
    for id in ["openvpn/good", "openvpn/later"] {
        let row = observed.instances.iter().find(|r| r.id == id).unwrap();
        assert_eq!(row.detail["processRunning"], json!(true), "a missing management socket does not mean no process");
        assert_eq!(row.detail["sessionStateAvailable"], json!(false));
        assert_eq!(row.detail["rxBytes"], json!(123));
        assert_ne!(row.reason.as_deref(), Some("process not running"));
    }
    env.engine.supervisor.stop_all().await;
    let observed: ProbeResult = serde_json::from_value(env.engine.runner.call(&program, &request).await.unwrap()).unwrap();
    assert!(
        observed.instances.iter().all(|r| r.detail["processRunning"] == json!(false)),
        "stale PID/status files cannot keep stopped instances alive"
    );
    let _ = std::fs::remove_dir_all(env.paths.etc.parent().unwrap());
}

#[tokio::test(flavor = "current_thread")]
async fn tunnel_prepare_errors_are_per_instance_and_group_conflicts_remain_checked() {
    let _native_env = NATIVE_TOOL_ENV.lock().await;
    let Some(env) = setup("tunnel-prepare-isolation") else { return };
    let reg = registry::scan(&env.paths.packages, &Default::default());
    for short in ["wireguard", "openvpn"] {
        if (short == "wireguard" && !env.have_wg) || (short == "openvpn" && !env.have_openvpn) {
            continue;
        }
        for defect in ["secret", "malformed", "invalid", "port", "device"] {
            let (mut raw, _) = tunnel_config(&env, short, 1, &["good", "broken", "later"]);
            let broken = &mut raw["services"][short]["instances"][1];
            match defect {
                "secret" => broken[if short == "wireguard" { "privateKeyRef" } else { "staticKeyRef" }] = json!("missing-key"),
                "malformed" => broken["enabled"] = json!("not-a-boolean"),
                "invalid" => broken["device"] = json!("bad/device"),
                "port" => {
                    broken[if short == "wireguard" { "listenPort" } else { "port" }] =
                        json!(if short == "wireguard" { 51820 } else { 1194 })
                }
                "device" => {
                    raw["services"][short]["instances"][0]["device"] = json!("same-dev");
                    raw["services"][short]["instances"][1]["device"] = json!("same-dev");
                }
                _ => unreachable!(),
            }
            let prepared = env.engine.prepare(&raw, &canonical_digest(&raw).unwrap(), &reg, &Default::default()).await.unwrap();
            assert_eq!(prepared.instance_errors.len(), 1, "{short}/{defect}: {:?}", prepared.instance_errors);
            assert!(prepared.instance_errors.contains_key(&format!("{short}/broken")), "{short}/{defect}");
            for id in ["good", "later"] {
                assert!(prepared.plans.iter().any(|(_, p)| p.instance == format!("{short}/{id}")), "{short}/{defect}/{id}");
            }
            assert!(prepared.plans.iter().any(|(_, p)| p.instance == "avahi/local"));
        }
    }
    let _ = std::fs::remove_dir_all(env.paths.etc.parent().unwrap());
}

#[tokio::test(flavor = "current_thread")]
async fn tunnel_delivery_failures_retry_only_the_failed_instance() {
    use meduza_np_planner::state::AppliedState;
    use meduza_np_protocol::plan::CommandSpec;
    let _native_env = NATIVE_TOOL_ENV.lock().await;
    for short in ["wireguard", "openvpn"] {
        for defect in ["syntax", "resource", "reload", "spawn"] {
            let Some(env) = setup(&format!("isolated-{short}-{defect}")) else { return };
            if (short == "wireguard" && !env.have_wg) || (short == "openvpn" && !env.have_openvpn) {
                continue;
            }
            let reg = registry::scan(&env.paths.packages, &Default::default());
            let (raw, digest) = tunnel_config(&env, short, 1, &["good"]);
            let prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.unwrap();
            env.engine.commit(&prepared, &reg).await.unwrap();
            let local_pid = env.engine.supervisor.status("avahi/local").unwrap().pid;
            let good_id = format!("{short}/good");
            let good_pid = env.engine.supervisor.status(&good_id).and_then(|s| s.pid);
            let (raw, digest) = tunnel_config(&env, short, 2, &["good", "broken", "later"]);
            let mut prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.unwrap();
            assert!(prepared.instance_errors.is_empty(), "{:?}", prepared.instance_errors);
            let bad_id = format!("{short}/broken");
            let bad = &mut prepared.plans.iter_mut().find(|(_, p)| p.instance == bad_id).unwrap().1;
            let crash = CommandSpec {
                provider_slot: Some("avahi.daemon".into()),
                program: None,
                argv: vec!["crash".into()],
                required: true,
                timeout_seconds: 3,
            };
            let expected = match defect {
                "syntax" => {
                    bad.checks.push(crash);
                    "syntax-check-failed"
                }
                "reload" => {
                    bad.reload.push(crash);
                    "reload-failed"
                }
                "resource" => {
                    let resource = meduza_np_protocol::plan::NetworkResource::Link {
                        device: "broken-test".into(),
                        link_type: "dummy".into(),
                        mtu: None,
                        up: true,
                    };
                    bad.resources.push(resource.clone());
                    env.net.fail_on.lock().unwrap().push(resource);
                    "network-apply-failed"
                }
                "spawn" => {
                    let mut proc = prepared.plans.iter().find(|(_, p)| p.instance == "avahi/local").unwrap().1.processes[0].clone();
                    proc.id = bad_id.clone();
                    proc.provider_slot = None;
                    proc.program = Some("/nonexistent/meduza-np-tunnel-test".into());
                    prepared.plans.iter_mut().find(|(_, p)| p.instance == bad_id).unwrap().1.processes = vec![proc];
                    "process-failed"
                }
                _ => unreachable!(),
            };
            let outcomes = env.engine.commit(&prepared, &reg).await.expect("instance failure must not abort the revision");
            assert!(outcomes.iter().any(|o| o.instance == bad_id && !o.ok));
            assert!(outcomes.iter().any(|o| o.instance == format!("{short}/later") && o.ok));
            let applied = AppliedState::load(&env.paths).unwrap();
            assert_eq!(applied.revision.as_deref(), Some("rev-000002"));
            assert_eq!(applied.instance_errors[&bad_id].code, expected, "{short}/{defect}");
            assert!(!applied.instances.contains_key(&bad_id));
            assert!(applied.instance_errors[&bad_id].retry_at_unix > 0);
            let mut rows = Vec::new();
            applied.overlay_instance_errors(&mut rows);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].id, bad_id);
            assert!(rows[0].reason.as_ref().unwrap().contains(expected));
            assert_eq!(env.engine.supervisor.status("avahi/local").unwrap().pid, local_pid);
            assert_eq!(env.engine.supervisor.status(&good_id).and_then(|s| s.pid), good_pid);
            assert_eq!(meduza_np_planner::state::load_last_good(&env.paths).unwrap().unwrap().digest, digest);
            let generation = std::fs::canonicalize(env.paths.current_generation_link()).unwrap();
            env.net.fail_on.lock().unwrap().clear();
            let mut retry = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.unwrap();
            retry.retry_instances = Some([bad_id.clone()].into());
            retry.keep_last_good = true;
            // Healthy native checks must not even execute on a scoped retry.
            retry.plans.iter_mut().find(|(_, p)| p.instance == good_id).unwrap().1.checks.push(CommandSpec {
                provider_slot: Some("avahi.daemon".into()),
                program: None,
                argv: vec!["crash".into()],
                required: true,
                timeout_seconds: 3,
            });
            env.engine.commit(&retry, &reg).await.unwrap();
            let applied = AppliedState::load(&env.paths).unwrap();
            assert!(applied.instance_errors.is_empty(), "{short}/{defect}: {:?}", applied.instance_errors);
            assert!(applied.instances.contains_key(&bad_id));
            assert_ne!(
                std::fs::canonicalize(env.paths.current_generation_link()).unwrap(),
                generation,
                "same revision retries must not delete the live generation"
            );
            assert_eq!(env.engine.supervisor.status("avahi/local").unwrap().pid, local_pid);
            assert_eq!(env.engine.supervisor.status(&good_id).and_then(|s| s.pid), good_pid);
            env.engine.supervisor.stop_all().await;
            let _ = std::fs::remove_dir_all(env.paths.etc.parent().unwrap());
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn disabled_secret_refs_do_not_hold_back_enabled_instances_or_peers() {
    let _native_env = NATIVE_TOOL_ENV.lock().await;
    let Some(env) = setup("disabled-secret-refs") else { return };
    if !env.have_wg {
        return;
    }
    let reg = registry::scan(&env.paths.packages, &Default::default());
    let (mut raw, _) = config(&env, 1, json!([{"id":"active"}]));
    raw["services"]["wireguard"]["instances"].as_array_mut().unwrap().push(json!({
        "id":"paused", "enabled":false, "privateKeyRef":"not-delivered-paused-key"
    }));
    raw["services"]["wireguard"]["instances"][0]["peers"].as_array_mut().unwrap().push(json!({
        "id":"paused-peer", "enabled":false, "publicKey":PRIV, "presharedKeyRef":"not-delivered-paused-psk", "allowedIps":[]
    }));
    let prepared = env.engine.prepare(&raw, &canonical_digest(&raw).unwrap(), &reg, &Default::default()).await.unwrap();
    assert!(prepared.held_back.is_empty(), "disabled references cannot hold back the enabled service: {:?}", prepared.warnings);
    let wg: Vec<_> = prepared.plans.iter().filter(|(p, _)| p == "meduza-np-wireguard").collect();
    assert_eq!(wg.len(), 1, "the enabled instance is actually planned");
    assert_eq!(wg[0].1.instance, "wireguard/uplink");

    // A genuinely missing active key holds back only its own instance.
    raw["services"]["wireguard"]["instances"][0]["peers"][0]["presharedKeyRef"] = json!("missing-active-psk");
    let digest = canonical_digest(&raw).unwrap();
    let prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.unwrap();
    assert!(prepared.held_back.is_empty());
    assert_eq!(prepared.instance_errors["wireguard/uplink"].code, "waiting-dependency");
    assert!(prepared.warnings.iter().any(|w| w.contains("missing-active-psk") && !w.contains("not-delivered-paused")));
    assert!(prepared.plans.iter().any(|(_, p)| p.instance == "avahi/active"));
    assert!(prepared.plans.iter().all(|(p, _)| p != "meduza-np-wireguard"));

    env.engine
        .keystore
        .put(
            "missing-active-psk",
            &meduza_np_planner::keystore::StoredKey { kind: "wireguard-preshared-key".into(), value: PRIV.into(), public_value: None },
        )
        .unwrap();
    let prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.unwrap();
    assert!(prepared.held_back.is_empty());
    assert!(prepared.plans.iter().any(|(_, p)| p.instance == "wireguard/uplink"), "node-held active keys still complete the feature");
}

#[tokio::test(flavor = "current_thread")]
async fn blocked_tunnels_keep_previous_files_and_do_not_lose_their_resources() {
    use meduza_np_planner::state::AppliedState;
    use meduza_np_protocol::plan::CommandSpec;
    let _native_env = NATIVE_TOOL_ENV.lock().await;
    for short in ["wireguard", "openvpn"] {
        for defect in ["secret", "syntax"] {
            let Some(env) = setup(&format!("retained-{short}-{defect}")) else { return };
            if (short == "wireguard" && !env.have_wg) || (short == "openvpn" && !env.have_openvpn) {
                continue;
            }
            let reg = registry::scan(&env.paths.packages, &Default::default());
            let (raw, digest) = tunnel_config(&env, short, 1, &["good", "broken"]);
            let prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.unwrap();
            env.engine.commit(&prepared, &reg).await.unwrap();
            let id = format!("{short}/broken");
            let bad = &prepared.plans.iter().find(|(_, p)| p.instance == id).unwrap().1;
            let file = &bad.files[0].path;
            let content = std::fs::read(env.paths.current_generation_link().join(file)).unwrap();
            let old_pid = env.engine.supervisor.status(&id).and_then(|s| s.pid);
            let mut raw = raw.clone();
            raw["revision"] = json!("rev-000002");
            if defect == "secret" {
                raw["services"][short]["instances"][1][if short == "wireguard" { "privateKeyRef" } else { "staticKeyRef" }] =
                    json!("missing");
            }
            let digest = canonical_digest(&raw).unwrap();
            let mut prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.unwrap();
            if defect == "syntax" {
                let bad = &mut prepared.plans.iter_mut().find(|(_, p)| p.instance == id).unwrap().1;
                bad.files[0].content = "deliberately invalid replacement".into();
                bad.checks.push(CommandSpec {
                    provider_slot: Some("avahi.daemon".into()),
                    program: None,
                    argv: vec!["crash".into()],
                    required: true,
                    timeout_seconds: 3,
                });
            }
            env.engine.commit(&prepared, &reg).await.unwrap();
            assert_eq!(std::fs::read(env.paths.current_generation_link().join(file)).unwrap(), content);
            assert_eq!(env.engine.supervisor.status(&id).and_then(|s| s.pid), old_pid);
            let applied = AppliedState::load(&env.paths).unwrap();
            assert!(applied.instances.contains_key(&id));
            let mut rows = vec![];
            applied.overlay_instance_errors(&mut rows);
            assert_eq!(rows[0].state, meduza_np_protocol::report::ServiceState::Degraded);
            env.engine.supervisor.stop_all().await;
            let _ = std::fs::remove_dir_all(env.paths.etc.parent().unwrap());
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_instance_key_is_reported_and_recovers_manually_or_automatically() {
    use meduza_np_planner::{local_api, state::AppliedState, sync::Planner};
    use meduza_np_protocol::local::LocalRequest;
    let _native_env = NATIVE_TOOL_ENV.lock().await;
    let Some(env) = setup("instance-retry-report") else { return };
    if !env.have_wg {
        return;
    }
    let reg = registry::scan(&env.paths.packages, &Default::default());
    let (mut raw, _) = tunnel_config(&env, "wireguard", 1, &["good", "manual", "automatic"]);
    let p = Planner::new(env.paths.clone(), Default::default(), env.engine, reg);
    raw["services"]["wireguard"]["instances"][1]["privateKeyRef"] = json!("manual-key");
    raw["services"]["wireguard"]["instances"][2]["privateKeyRef"] = json!("automatic-key");
    p.offer(raw.clone(), canonical_digest(&raw).unwrap(), "rev-000001".into()).await;
    let report = meduza_np_planner::reporter::build(&p);
    assert_eq!(report.sync.state, meduza_np_protocol::report::SyncState::InSync);
    assert!(report.sync.last_error.is_none());
    assert_eq!(report.instances.iter().filter(|r| r.reason.as_ref().is_some_and(|r| r.contains("waiting-dependency"))).count(), 2);
    let response =
        local_api::dispatch(&p, LocalRequest { protocol: 1, id: 1, method: "instances".into(), params: Value::Null }, true).await;
    assert!(serde_json::to_string(&response).unwrap().contains("waiting-dependency"));
    let pid = p.engine.supervisor.status("avahi/local").unwrap().pid;
    assert!(p.retry_instance("wireguard/manual").await.is_err());
    let waiting = AppliedState::load(&p.paths).unwrap();
    assert_eq!(waiting.instance_errors["wireguard/manual"].attempt, 2);
    assert_eq!(waiting.instance_errors["wireguard/automatic"].attempt, 1);
    p.engine
        .keystore
        .put(
            "manual-key",
            &meduza_np_planner::keystore::StoredKey { kind: "wireguard-private-key".into(), value: PRIV.into(), public_value: None },
        )
        .unwrap();
    p.retry_instance("wireguard/manual").await.unwrap();
    let waiting = AppliedState::load(&p.paths).unwrap();
    assert!(!waiting.instance_errors.contains_key("wireguard/manual"));
    assert_eq!(waiting.instance_errors["wireguard/automatic"].attempt, 1);
    p.engine
        .keystore
        .put(
            "automatic-key",
            &meduza_np_planner::keystore::StoredKey { kind: "wireguard-private-key".into(), value: PRIV.into(), public_value: None },
        )
        .unwrap();
    let mut due = AppliedState::load(&p.paths).unwrap();
    due.instance_errors.get_mut("wireguard/automatic").unwrap().retry_at_unix = 0;
    due.save(&p.paths).unwrap();
    let retry_loop = tokio::spawn(p.clone().instance_retry_loop());
    let end = std::time::Instant::now() + Duration::from_secs(12);
    while !AppliedState::load(&p.paths).unwrap().instance_errors.is_empty() && std::time::Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    retry_loop.abort();
    let _ = retry_loop.await;
    let applied = AppliedState::load(&p.paths).unwrap();
    assert!(applied.instance_errors.is_empty());
    assert!(applied.instances.contains_key("wireguard/automatic"));
    assert_eq!(p.engine.supervisor.status("avahi/local").unwrap().pid, pid);
    assert!(meduza_np_planner::reporter::build(&p).instances.iter().all(|r| !r.detail.contains_key("applyError")));
    p.engine.supervisor.stop_all().await;
    let _ = std::fs::remove_dir_all(p.paths.etc.parent().unwrap());
}

#[tokio::test(flavor = "current_thread")]
async fn disabling_tunnels_stops_resources_clears_failures_and_never_retries_them() {
    use meduza_np_planner::{local_api, state::AppliedState, sync::Planner};
    use meduza_np_protocol::{
        local::LocalRequest,
        plan::CommandSpec,
        report::{InstanceStatus, ServiceState},
    };
    let _native_env = NATIVE_TOOL_ENV.lock().await;
    for short in ["wireguard", "openvpn"] {
        for mode in ["instance", "service"] {
            let Some(env) = setup(&format!("disable-{short}-{mode}")) else { return };
            if (short == "wireguard" && !env.have_wg) || (short == "openvpn" && !env.have_openvpn) {
                continue;
            }
            let reg = registry::scan(&env.paths.packages, &Default::default());
            let (baseline, digest) = tunnel_config(&env, short, 1, &["good", "paused", "broken"]);
            let p = Planner::new(env.paths.clone(), Default::default(), env.engine, reg);
            p.offer(baseline.clone(), digest, "rev-000001".into()).await;
            let local_pid = p.engine.supervisor.status("avahi/local").unwrap().pid;
            let good_id = format!("{short}/good");
            let paused_id = format!("{short}/paused");
            let broken_id = format!("{short}/broken");
            let good_pid = p.engine.supervisor.status(&good_id).and_then(|s| s.pid);
            let mut failed = baseline.clone();
            failed["revision"] = json!("rev-000002");
            let mut prepared = p.engine.prepare(&failed, &canonical_digest(&failed).unwrap(), &p.registry(), &p.platform).await.unwrap();
            prepared.plans.iter_mut().find(|(_, plan)| plan.instance == broken_id).unwrap().1.files[0]
                .content
                .push_str("\n# force local reload\n");
            prepared.plans.iter_mut().find(|(_, plan)| plan.instance == broken_id).unwrap().1.reload.push(CommandSpec {
                provider_slot: Some("avahi.daemon".into()),
                program: None,
                argv: vec!["crash".into()],
                required: true,
                timeout_seconds: 3,
            });
            p.engine.commit(&prepared, &p.registry()).await.unwrap();
            let error = AppliedState::load(&p.paths).unwrap().instance_errors[&broken_id].clone();
            let mut disabled = failed.clone();
            disabled["revision"] = json!("rev-000003");
            for index in [1, 2] {
                disabled["services"][short]["instances"][index]["enabled"] = json!(false);
                disabled["services"][short]["instances"][index]["device"] = json!("bad/device");
                disabled["services"][short]["instances"][index][if short == "wireguard" { "privateKeyRef" } else { "staticKeyRef" }] =
                    json!("undelivered");
                disabled["services"][short]["instances"][index][if short == "wireguard" { "addresses" } else { "ifconfig" }] =
                    json!("unfinished");
            }
            if mode == "service" {
                disabled["services"][short]["enabled"] = json!(false);
            } else {
                disabled["services"][short]["instances"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"id":"good","enabled":false,"port":"unfinished"}));
            }
            p.status.lock().unwrap().probes = vec![InstanceStatus {
                id: paused_id.clone(),
                kind: "probe".into(),
                state: ServiceState::Up,
                reason: Some("cached old failure".into()),
                observed_at: None,
                detail: [("applyError".into(), json!("cached failure"))].into(),
            }];
            p.offer(disabled.clone(), canonical_digest(&disabled).unwrap(), "rev-000003".into()).await;
            let applied = AppliedState::load(&p.paths).unwrap();
            assert!(applied.instance_errors.is_empty());
            assert!(!applied.instances.contains_key(&paused_id));
            assert!(!applied.instances.contains_key(&broken_id));
            assert!(p.engine.supervisor.status(&paused_id).is_none());
            assert!(meduza_np_runtime::journal::OwnershipJournal::load(&p.paths.ownership())
                .unwrap()
                .possibly_owned(&paused_id)
                .is_empty());
            assert_eq!(p.engine.supervisor.status("avahi/local").unwrap().pid, local_pid);
            if mode == "instance" {
                assert_eq!(p.engine.supervisor.status(&good_id).and_then(|s| s.pid), good_pid);
            }
            let report = meduza_np_planner::reporter::build(&p);
            assert_eq!(report.sync.state, meduza_np_protocol::report::SyncState::InSync);
            for id in [&paused_id, &broken_id] {
                let row = report.instances.iter().find(|r| &r.id == id).unwrap();
                assert_eq!(row.state, ServiceState::Disabled);
                assert_eq!(row.reason.as_deref(), Some("disabled"));
                assert!(!row.detail.contains_key("applyError"));
            }
            // Even legacy/stale persisted failures cannot revive a disabled instance.
            let mut stale = applied;
            stale.instance_errors.insert(broken_id.clone(), error);
            stale.save(&p.paths).unwrap();
            let generation = std::fs::canonicalize(p.paths.current_generation_link()).unwrap();
            assert!(p.retry_instance(&broken_id).await.unwrap_err().contains("disabled"));
            for action in ["start", "restart"] {
                let response = local_api::dispatch(
                    &p,
                    LocalRequest { protocol: 1, id: 1, method: format!("instance.{action}"), params: json!({"instance":broken_id}) },
                    true,
                )
                .await;
                assert!(!response.ok);
                assert_eq!(response.error.unwrap().code, "disabled");
            }
            if short == "openvpn" && mode == "instance" {
                let task = tokio::spawn(p.clone().instance_retry_loop());
                tokio::time::sleep(Duration::from_secs(7)).await;
                task.abort();
                let _ = task.await;
                assert_eq!(std::fs::canonicalize(p.paths.current_generation_link()).unwrap(), generation);
                assert_eq!(AppliedState::load(&p.paths).unwrap().instance_errors[&broken_id].attempt, 1);
            }
            let mut enabled = baseline.clone();
            enabled["revision"] = json!("rev-000004");
            enabled["services"][short]["instances"][2]["enabled"] = json!(false);
            p.offer(enabled.clone(), canonical_digest(&enabled).unwrap(), "rev-000004".into()).await;
            let applied = AppliedState::load(&p.paths).unwrap();
            assert!(applied.instances.contains_key(&paused_id));
            assert!(applied.instance_errors.is_empty());
            assert!(p.effective_probes().iter().filter(|r| r.id == paused_id).all(|r| r.state != ServiceState::Disabled));
            p.engine.supervisor.stop_all().await;
            let _ = std::fs::remove_dir_all(p.paths.etc.parent().unwrap());
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn disabled_wireguard_peers_are_removed_from_native_configuration_and_reported_disabled() {
    use meduza_np_planner::sync::Planner;
    use meduza_np_protocol::report::{InstanceStatus, ServiceState};
    let _native_env = NATIVE_TOOL_ENV.lock().await;
    let Some(env) = setup("disable-wg-peer") else { return };
    if !env.have_wg {
        return;
    }
    let (mut raw, _) = tunnel_config(&env, "wireguard", 1, &["good"]);
    raw["services"]["wireguard"]["instances"][0]["peers"] = json!([
        {"id":"peer","publicKey":PEER,"allowedIps":["10.9.0.1/32"],"endpoint":"198.51.100.1:51820"},
        {"id":"draft","enabled":false,"publicKey":PEER,"allowedIps":["unfinished"],"endpoint":false,"presharedKeyRef":"undelivered"}
    ]);
    let reg = registry::scan(&env.paths.packages, &Default::default());
    let p = Planner::new(env.paths.clone(), Default::default(), env.engine, reg);
    p.offer(raw.clone(), canonical_digest(&raw).unwrap(), "rev-000001".into()).await;
    let path = p.paths.current_generation_link().join("wireguard/good/wg.conf");
    assert_eq!(std::fs::read_to_string(&path).unwrap().matches(PEER).count(), 1);
    p.status.lock().unwrap().probes = vec![InstanceStatus {
        id: "wireguard/good/peer".into(),
        kind: "wireguard-peer".into(),
        state: ServiceState::Down,
        observed_at: None,
        reason: Some("old endpoint failure".into()),
        detail: [("endpointResolution".into(), json!({"phase":"resolution-failed"}))].into(),
    }];
    raw["revision"] = json!("rev-000002");
    raw["services"]["wireguard"]["instances"][0]["peers"][0]["enabled"] = json!(false);
    p.offer(raw.clone(), canonical_digest(&raw).unwrap(), "rev-000002".into()).await;
    assert!(!std::fs::read_to_string(&path).unwrap().contains(PEER));
    let rows = p.effective_probes();
    let peer = rows.iter().find(|r| r.id == "wireguard/good/peer").unwrap();
    assert_eq!(peer.state, ServiceState::Disabled);
    assert!(!peer.detail.contains_key("endpointResolution"));
    assert_eq!(rows.iter().find(|r| r.id == "wireguard/good/draft").unwrap().state, ServiceState::Disabled);
    assert!(p.running_packages().1.contains("wireguard"), "disabled peers do not degrade the active interface's package health");
    raw["revision"] = json!("rev-000003");
    raw["services"]["wireguard"]["instances"][0]["peers"][0]["enabled"] = json!(true);
    p.offer(raw.clone(), canonical_digest(&raw).unwrap(), "rev-000003".into()).await;
    assert!(std::fs::read_to_string(&path).unwrap().contains(PEER));
    p.engine.supervisor.stop_all().await;
    let _ = std::fs::remove_dir_all(p.paths.etc.parent().unwrap());
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn wireguard_dns_and_reload_errors_do_not_abort_other_instances() {
    use std::os::unix::fs::PermissionsExt;
    let _native_env = NATIVE_TOOL_ENV.lock().await;
    let Some(env) = setup("wireguard-dns") else { return };
    if !env.have_wg {
        return;
    }
    let root = env.paths.etc.parent().unwrap();
    let wg = root.join("wg-test");
    let failure = root.join("fail-local");
    let script = format!(
        r#"#!/bin/sh
if [ "$1" = syncconf ]; then
    if [ -e '{}' ]; then echo 'local configuration rejected' >&2; exit 1; fi
    if sed -n '/^Endpoint = /p' "$3" | cut -d= -f2 | tr -d ' ' | grep -q '\.invalid'; then
        echo 'Name does not resolve: external.invalid:51820 Configuration parsing error' >&2; exit 1
    fi
    exit 0
fi
if [ "$1" = show ]; then printf '%s\t(none)\n' '{}'; exit 0; fi
exit 0
"#,
        failure.display(),
        PEER
    );
    std::fs::write(&wg, script).unwrap();
    std::fs::set_permissions(&wg, std::fs::Permissions::from_mode(0o755)).unwrap();
    let helper = env.paths.libexec.join("plugins/wireguard");
    std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
    std::fs::copy(target_dir().join(format!("meduza-np-wireguard{EXE}")), helper).unwrap();
    let mut reg = registry::scan(&env.paths.packages, &Default::default());
    reg.providers.get_mut("wireguard.tools").unwrap().path = wg.to_string_lossy().into_owned();
    let (mut raw, _) = config(&env, 1, json!([{"id":"a","version":1}]));
    raw["services"]["wireguard"]["instances"][0]["peers"][0]["endpoint"] = json!("external.invalid:51820");
    let prepared = env.engine.prepare(&raw, &canonical_digest(&raw).unwrap(), &reg, &Default::default()).await.unwrap();
    env.engine.commit(&prepared, &reg).await.expect("unresolved peer must not abort applying WireGuard or the other service");
    assert!(env.engine.supervisor.status("avahi/a").unwrap().pid.is_some());
    assert_eq!(meduza_np_planner::state::AppliedState::load(&env.paths).unwrap().revision.as_deref(), Some("rev-000001"));
    let file = std::fs::read_to_string(env.paths.current_generation_link().join("wireguard/uplink/wg.conf")).unwrap();
    assert!(file.contains(PEER) && !file.contains("external.invalid"), "local peer/key delivery is independent of DNS");
    let status = env.paths.run.join("wireguard-uplink.endpoints.json");
    // NSS cancellation/reaping under WSL can exceed the resolver's five-second deadline.
    let end = std::time::Instant::now() + Duration::from_secs(20);
    let mut failure_reported = false;
    while std::time::Instant::now() < end {
        let state = std::fs::read(&status).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        if state.as_ref().is_some_and(|v| v.as_object().unwrap().values().any(|s| s["phase"] == "resolution-failed")) {
            failure_reported = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    std::fs::write(&failure, "fail").unwrap();
    raw["revision"] = json!("rev-000002");
    raw["services"]["wireguard"]["instances"][0]["listenPort"] = json!(51900);
    let prepared = env.engine.prepare(&raw, &canonical_digest(&raw).unwrap(), &reg, &Default::default()).await.unwrap();
    let result = env.engine.commit(&prepared, &reg).await;
    env.engine.supervisor.stop_all().await;
    assert!(failure_reported, "the per-peer DNS failure and retry must be observable while the revision stays applied");
    let outcomes = result.expect("a failed WireGuard instance does not abort other instances");
    assert!(outcomes.iter().any(|o| o.instance == "wireguard/uplink" && !o.ok));
    let applied = meduza_np_planner::state::AppliedState::load(&env.paths).unwrap();
    assert_eq!(applied.instance_errors["wireguard/uplink"].code, "reload-failed");
    assert_eq!(applied.revision.as_deref(), Some("rev-000002"));
    let _ = std::fs::remove_dir_all(root);
}

/// An unreachable peer is runtime health, not a failed delivery or a reason to undo other services/WireGuard.
#[tokio::test(flavor = "current_thread")]
async fn disconnected_connection_commits_and_recovers_without_reapply() {
    let _native_env = NATIVE_TOOL_ENV.lock().await;
    let Some(env) = setup("connection") else { return };
    let reg = registry::scan(&env.paths.packages, &meduza_np_plugin_sdk::Platform::default());
    let ready = env.paths.run.join("peer-ready");
    let (raw, digest) = config(
        &env,
        1,
        json!([
            {"id": "local"},
            {"id": "remote", "connection": true, "readyFile": ready.to_string_lossy()}
        ]),
    );
    let prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.unwrap();
    let outcomes = env.engine.commit(&prepared, &reg).await.expect("unreachable peer must not fail configuration apply");
    assert!(outcomes.iter().all(|o| o.ok));
    let local_pid = env.engine.supervisor.status("avahi/local").unwrap().pid;
    assert!(local_pid.is_some(), "unrelated local service stays up");
    let last_good = meduza_np_planner::state::load_last_good(&env.paths).unwrap().unwrap();
    assert_eq!(last_good.digest, digest);
    if env.have_wg {
        assert!(env.paths.current_generation_link().join("wireguard/uplink/wg.conf").exists(), "WireGuard applies without a handshake");
    }
    for _ in 0..100 {
        if env.engine.supervisor.status("avahi/remote").unwrap().state == meduza_np_planner::supervisor::ProcState::Backoff {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(env.engine.supervisor.status("avahi/remote").unwrap().state, meduza_np_planner::supervisor::ProcState::Backoff);

    // Publishing the same inputs during backoff still succeeds and does not roll back the saved configuration.
    let mut repeated = raw;
    repeated["revision"] = json!("rev-000002");
    let digest = canonical_digest(&repeated).unwrap();
    let prepared = env.engine.prepare(&repeated, &digest, &reg, &Default::default()).await.unwrap();
    env.engine.commit(&prepared, &reg).await.expect("backoff also counts as delivered on repeat apply");
    std::fs::write(ready, "peer recovered").unwrap();
    assert!(env.engine.supervisor.wait_settled("avahi/remote", Duration::from_millis(200), Duration::from_secs(5)).await);
    assert_eq!(env.engine.supervisor.status("avahi/local").unwrap().pid, local_pid);
    assert_eq!(meduza_np_planner::state::load_last_good(&env.paths).unwrap().unwrap().digest, digest);
    env.engine.supervisor.stop_all().await;
    let _ = std::fs::remove_dir_all(env.paths.etc.parent().unwrap());
}

#[tokio::test(flavor = "current_thread")]
async fn connection_delivery_errors_still_fail_apply() {
    let _native_env = NATIVE_TOOL_ENV.lock().await;
    use meduza_np_protocol::plan::CommandSpec;

    for failure in ["spawn", "syntax", "resource"] {
        let Some(env) = setup(&format!("connection-{failure}")) else { return };
        if failure == "wireguard-resource" && !env.have_wg {
            continue;
        }
        let reg = registry::scan(&env.paths.packages, &meduza_np_plugin_sdk::Platform::default());
        let (raw, digest) = config(&env, 1, json!([{"id": "local"}]));
        let prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.unwrap();
        env.engine.commit(&prepared, &reg).await.unwrap();
        let (raw, next_digest) = config(
            &env,
            2,
            json!([
                {"id": "local"}, {"id": "remote", "connection": true, "crash": true, "device": "dummy-remote"}
            ]),
        );
        let mut prepared = env.engine.prepare(&raw, &next_digest, &reg, &Default::default()).await.unwrap();
        let plan = &mut prepared.plans.iter_mut().find(|(_, p)| p.instance == "avahi/remote").unwrap().1;
        let expected = match failure {
            "spawn" => {
                plan.processes[0].provider_slot = None;
                plan.processes[0].program = Some("/nonexistent/meduza-np-test-daemon".into());
                "process-failed"
            }
            "syntax" => {
                plan.checks.push(CommandSpec {
                    provider_slot: Some("avahi.daemon".into()),
                    program: None,
                    argv: vec!["crash".into()],
                    timeout_seconds: 3,
                    required: true,
                });
                "syntax-check-failed"
            }
            "resource" => {
                env.net.fail_on.lock().unwrap().push(plan.resources[0].clone());
                "network-apply-failed"
            }
            "wireguard-resource" => {
                let plan = &mut prepared.plans.iter_mut().find(|(_, p)| p.instance == "wireguard/uplink").unwrap().1;
                // Make the already-owned link disappear and fail to recreate it locally.
                let link =
                    plan.resources.iter().find(|r| matches!(r, meduza_np_protocol::plan::NetworkResource::Link { .. })).unwrap().clone();
                env.net.missing.lock().unwrap().push(link.clone());
                env.net.fail_on.lock().unwrap().push(link);
                "network-apply-failed"
            }
            _ => unreachable!(),
        };
        let error = env.engine.commit(&prepared, &reg).await.expect_err("local delivery failure must still reject apply");
        assert_eq!(error.code, expected, "{failure}: {error:?}");
        assert_eq!(meduza_np_planner::state::load_last_good(&env.paths).unwrap().unwrap().digest, digest);
        env.engine.supervisor.stop_all().await;
        let _ = std::fs::remove_dir_all(env.paths.etc.parent().unwrap());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn apply_lifecycle() {
    let _native_env = NATIVE_TOOL_ENV.lock().await;
    let Some(env) = setup("lifecycle") else { return };
    // Commands run by planner (wg syncconf) inherit the environment; supervised daemons get --log= instead.
    std::env::set_var("MNP_FAKE_TOOL_LOG", &env.log);
    let reg = registry::scan(&env.paths.packages, &meduza_np_plugin_sdk::Platform::default());
    for p in reg.packages.values() {
        assert!(p.registered(), "{} not registered: {:?}", p.manifest.package, p.reason);
    }
    let log_arg = env.log.to_string_lossy().to_string();

    // 1. First apply.
    let (raw, digest) = config(&env, 1, json!([{"id": "a", "log": log_arg}]));
    let prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.expect("prepare");
    env.engine.commit(&prepared, &reg).await.expect("commit");
    let current = env.paths.current_generation_link();
    assert_eq!(std::fs::read_to_string(current.join("avahi/a/app.conf")).unwrap(), "version=1\n");
    let pid1 = env.engine.supervisor.status("avahi/a").expect("supervised").pid;
    assert!(pid1.is_some());
    if env.have_wg {
        let wg_conf = std::fs::read_to_string(current.join("wireguard/uplink/wg.conf")).unwrap();
        assert!(wg_conf.contains("PrivateKey = "));
        assert!(log_lines(&env).iter().any(|l| l.starts_with("syncconf wg-uplink ")), "wg syncconf ran: {:?}", log_lines(&env));
        assert!(env.net.log.lock().unwrap().iter().any(|l| l.contains("\"device\":\"wg-uplink\"") && l.contains("\"kind\":\"link\"")));
    }
    let lg = meduza_np_planner::state::load_last_good(&env.paths).unwrap().expect("last good saved");
    assert_eq!(lg.digest, digest);

    // 2. Same content under a new revision: no restart, no reload.
    let reloads_before = log_lines(&env).iter().filter(|l| l.starts_with("syncconf")).count();
    let (raw, digest) = config(&env, 2, json!([{"id": "a", "log": log_arg}]));
    let prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.unwrap();
    env.engine.commit(&prepared, &reg).await.unwrap();
    assert_eq!(env.engine.supervisor.status("avahi/a").unwrap().pid, pid1, "unchanged instance keeps running");
    assert_eq!(log_lines(&env).iter().filter(|l| l.starts_with("syncconf")).count(), reloads_before, "no reload without change");

    // 2b. Re-created planner container (bridge mode): the planner process over the same journal finds the
    // WireGuard link gone with the old network namespace. It creates the link again and reconfigures it, instead
    // of trusting the ownership journal and reloading an interface that does not exist.
    if env.have_wg {
        let link_adds = || env.net.log.lock().unwrap().iter().filter(|l| l.starts_with("add") && l.contains("\"kind\":\"link\"")).count();
        let link: meduza_np_protocol::plan::NetworkResource = env
            .net
            .log
            .lock()
            .unwrap()
            .iter()
            .find_map(|l| l.strip_prefix("add ").filter(|j| j.contains("\"device\":\"wg-uplink\"") && j.contains("\"kind\":\"link\"")))
            .map(|j| serde_json::from_str(j).unwrap())
            .expect("link added in step 1");
        let (adds, reloads) = (link_adds(), log_lines(&env).iter().filter(|l| l.starts_with("syncconf")).count());
        env.net.missing.lock().unwrap().push(link);
        // what a new planner process knows: nothing loaded yet (its daemons are adopted, see planner_restart_adopts_daemons)
        env.engine.loaded.lock().unwrap().clear();
        let prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.unwrap();
        env.engine.commit(&prepared, &reg).await.expect("lost link recreated");
        assert_eq!(link_adds(), adds + 1, "the lost link is added again, nothing else");
        assert!(env.net.missing.lock().unwrap().is_empty());
        assert_eq!(log_lines(&env).iter().filter(|l| l.starts_with("syncconf")).count(), reloads + 1, "and configured again");
        assert_eq!(env.engine.supervisor.status("avahi/a").unwrap().pid, pid1, "daemons untouched");
    }

    // 3. Changed content restarts only the affected instance.
    let (raw, digest) =
        config(&env, 3, json!([{"id": "a", "version": 2, "log": log_arg}, {"id": "b", "device": "dummy-b", "log": log_arg}]));
    let prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.unwrap();
    env.engine.commit(&prepared, &reg).await.unwrap();
    let pid2 = env.engine.supervisor.status("avahi/a").unwrap().pid;
    assert_ne!(pid2, pid1, "changed instance restarted");
    assert!(env.engine.supervisor.status("avahi/b").is_some());

    // 4. Removing an instance stops its process and removes its resources.
    let (raw, digest) = config(&env, 4, json!([{"id": "a", "version": 2, "log": log_arg}]));
    let prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.unwrap();
    env.engine.commit(&prepared, &reg).await.unwrap();
    assert!(env.engine.supervisor.status("avahi/b").is_none(), "removed instance stopped");
    assert!(env.net.log.lock().unwrap().iter().any(|l| l.starts_with("remove") && l.contains("dummy-b")));
    assert_eq!(env.engine.supervisor.status("avahi/a").unwrap().pid, pid2);

    // 5. Enabled service without a registered package waits (D20): the rest applies, a warning names the
    // package, and the updater is told to install it.
    let (mut raw, _) = config(&env, 5, json!([{"id": "a", "version": 2, "log": log_arg}]));
    raw["services"]["mosdns"] = json!({"enabled": true});
    let digest = canonical_digest(&raw).unwrap();
    let prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.expect("other features still apply");
    assert!(prepared.warnings.iter().any(|w| w.contains("waiting for meduza-np-mosdns")), "{:?}", prepared.warnings);
    let wanted_path = env.engine.paths.wanted_components();
    let read_wanted = || -> Vec<String> { serde_json::from_slice(&std::fs::read(&wanted_path).unwrap_or_default()).unwrap_or_default() };
    assert!(!read_wanted().contains(&"mosdns".to_string()), "prepare alone (validate, diagnostics) leaves the list alone");
    env.engine.publish_wanted_components(&raw, false);
    let wanted = read_wanted();
    assert!(wanted.contains(&"mosdns".to_string()) && wanted.contains(&"avahi".to_string()), "{wanted:?}");
    let mut with_mesh = raw.clone();
    with_mesh["services"]["tinc"] = json!({"enabled":true});
    env.engine.publish_wanted_components(&with_mesh, false);
    assert!(read_wanted().contains(&"frr".to_string()), "mesh automatically requests the FRR package without a handwritten FRR service");
    env.engine.publish_wanted_components(&raw, false);
    // D46: restoring an older configuration without mosdns merges, a newly received one replaces
    let (older, _) = config(&env, 4, json!([{"id": "a", "version": 2, "log": log_arg}]));
    env.engine.publish_wanted_components(&older, true);
    assert!(read_wanted().contains(&"mosdns".to_string()), "restore keeps the desired revision's packages");
    env.engine.publish_wanted_components(&older, false);
    assert!(!read_wanted().contains(&"mosdns".to_string()), "a received revision without the feature drops it");
    env.engine.publish_wanted_components(&raw, false);
    env.engine.commit(&prepared, &reg).await.unwrap();
    assert_eq!(env.engine.supervisor.status("avahi/a").unwrap().pid, pid2, "unchanged instance untouched");

    // 5b. A node-held key this node has not generated yet (D28) holds back only its feature; once the key is in
    // the keystore the same configuration plans it.
    if env.have_wg {
        let (mut raw, _) = config(&env, 5, json!([{"id": "a", "version": 2, "log": log_arg}]));
        raw["services"]["wireguard"]["instances"][0]["privateKeyRef"] = json!("wg-node-held");
        let digest = canonical_digest(&raw).unwrap();
        let prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.expect("other features still apply");
        assert!(prepared.warnings.iter().any(|w| w.contains("services.wireguard: waiting for wg-node-held")), "{:?}", prepared.warnings);
        assert!(prepared.plans.iter().all(|(p, _)| p != "meduza-np-wireguard"), "the feature waits");
        assert!(prepared.plans.iter().any(|(p, _)| p == "meduza-np-avahi"), "the rest applies");
        let key = meduza_np_planner::keystore::StoredKey { kind: "wireguard-private-key".into(), value: PRIV.into(), public_value: None };
        env.engine.keystore.put("wg-node-held", &key).unwrap();
        let prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.unwrap();
        assert!(prepared.plans.iter().any(|(p, _)| p == "meduza-np-wireguard"), "planned once the key exists");
    }

    // 6. Two instances claiming the same device conflict.
    let (raw, digest) = config(&env, 6, json!([{"id": "x", "device": "dup0"}, {"id": "y", "device": "dup0"}]));
    let rej = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.err().expect("conflict");
    assert_eq!(rej.code, "plan-conflict");

    // 7. A daemon that dies at startup fails the apply (not "process existed once").
    let (raw, digest) = config(&env, 7, json!([{"id": "a", "version": 2, "log": log_arg}, {"id": "c", "crash": true}]));
    let prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.unwrap();
    let failure = env.engine.commit(&prepared, &reg).await.expect_err("process failure");
    assert_eq!(failure.code, "process-failed");
    let lg = meduza_np_planner::state::load_last_good(&env.paths).unwrap().unwrap();
    assert_eq!(
        lg.config.revision.as_str(),
        "rev-000005",
        "last good (step 5, applied with a waiting feature) is unchanged after a failed apply"
    );

    env.engine.supervisor.stop_all().await;
    let _ = std::fs::remove_dir_all(env.paths.etc.parent().unwrap());
}

/// A provider prepared at runtime (mihomo-pkg style, D34) that has no binary yet holds back only the feature that
/// needs it: the revision still applies (it carries the preparer), and once the provider is registered the same
/// configuration plans the feature.
#[tokio::test(flavor = "current_thread")]
async fn preparing_provider_holds_back_only_its_feature() {
    let _native_env = NATIVE_TOOL_ENV.lock().await;
    let Some(env) = setup("preparing") else { return };
    let prep = env.paths.etc.parent().unwrap().join("prep");
    let fake_tool = target_dir().join("examples").join(format!("fake_tool{EXE}"));
    write_json(
        &env.paths.packages.join("meduza-np-openvpn-pkg/manifest.json"),
        &json!({
            "manifestVersion": 1, "package": "meduza-np-openvpn-pkg", "version": "0.1.0", "role": "binary-provider",
            "plannerApi": {"min": 1, "max": 1}, "requires": ["meduza-np-planner"], "provides": ["test.tools"],
            "providers": [
                {"slot": "avahi.daemon", "binary": "fake", "requiresPreparation": true, "dataDir": prep.to_string_lossy()},
                {"slot": "wireguard.tools", "binary": fake_tool.to_string_lossy()}
            ]
        }),
    );
    let plat = meduza_np_plugin_sdk::Platform::default();
    let reg = registry::scan(&env.paths.packages, &plat);
    assert!(!reg.providers.contains_key("avahi.daemon"), "not prepared yet");
    let (raw, digest) = config(&env, 1, json!([{"id": "a"}]));
    let prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.expect("the revision still applies");
    assert!(prepared.warnings.iter().any(|w| w.contains("services.avahi: waiting for its binary")), "{:?}", prepared.warnings);
    assert!(prepared.plans.iter().all(|(p, _)| p != "meduza-np-avahi"), "the feature waits");
    if env.have_wg {
        assert!(prepared.plans.iter().any(|(p, _)| p == "meduza-np-wireguard"), "the rest applies");
    }

    write_json(
        &prep.join("provider.json"),
        &json!({"state": "registered", "path": fake_tool.to_string_lossy(), "source": "github-release"}),
    );
    let reg = registry::scan(&env.paths.packages, &plat);
    let prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.unwrap();
    assert!(prepared.plans.iter().any(|(p, _)| p == "meduza-np-avahi"), "planned once the provider is registered");
    assert!(!prepared.warnings.iter().any(|w| w.contains("waiting for its binary")));
    let _ = std::fs::remove_dir_all(env.paths.etc.parent().unwrap());
}

/// D47: a last good revision applied while a feature waited for its package; once the package is installed and
/// rejects the feature, the restore applies the revision without it and keeps the saved last good as received.
#[tokio::test(flavor = "current_thread")]
async fn restore_drops_held_back_feature_rejected_later() {
    let _native_env = NATIVE_TOOL_ENV.lock().await;
    let Some(env) = setup("held-back") else { return };
    let plat = meduza_np_plugin_sdk::Platform::default();
    let avahi = env.paths.packages.join("meduza-np-avahi");
    let parked = env.paths.etc.parent().unwrap().join("parked-avahi");
    std::fs::rename(&avahi, &parked).unwrap();
    let reg = registry::scan(&env.paths.packages, &plat);
    // an instance without id: the fake avahi plugin rejects it, but only once it is installed
    let (raw, digest) = config(&env, 1, json!([{"version": 1}]));
    let prepared = env.engine.prepare(&raw, &digest, &reg, &plat).await.expect("applies without avahi");
    assert_eq!(prepared.held_back, vec!["avahi".to_string()]);
    env.engine.commit(&prepared, &reg).await.expect("commit");
    let lg = meduza_np_planner::state::load_last_good(&env.paths).unwrap().expect("last good saved");
    assert_eq!(lg.held_back, vec!["avahi".to_string()]);

    std::fs::rename(&parked, &avahi).unwrap();
    let reg = registry::scan(&env.paths.packages, &plat);
    assert!(env.engine.prepare(&lg.raw, &lg.digest, &reg, &plat).await.is_err(), "the feature is invalid now");
    let (prepared, rejected) = env.engine.prepare_last_good(&lg, &reg, &plat).await.expect("restorable without the feature");
    assert!(rejected.expect("rejection reported").message.contains("services.avahi"));
    assert!(prepared.plans.iter().all(|(p, _)| p != "meduza-np-avahi"));
    if env.have_wg {
        assert!(prepared.plans.iter().any(|(p, _)| p == "meduza-np-wireguard"), "the rest stays applied");
    }
    env.engine.commit(&prepared, &reg).await.expect("restore commit");
    let after = meduza_np_planner::state::load_last_good(&env.paths).unwrap().expect("still readable");
    assert_eq!((after.digest, after.raw, after.held_back), (lg.digest.clone(), lg.raw.clone(), lg.held_back.clone()), "kept as received");

    // A last good without held-back features gets no second chance: the rejection stands.
    let (raw, digest) = config(&env, 2, json!([{"version": 1}]));
    let lg = meduza_np_planner::state::LastGood { config: serde_json::from_value(raw.clone()).unwrap(), raw, digest, held_back: vec![] };
    assert!(env.engine.prepare_last_good(&lg, &reg, &plat).await.is_err());

    // A desired revision can fail for an unrelated reason before compensation rejects an older held-back
    // feature. The historical error must never replace the primary failure in the node/Manager report.
    let p = meduza_np_planner::sync::Planner::new(env.paths.clone(), plat.clone(), env.engine, registry::scan(&env.paths.packages, &plat));
    let primary = "openvpn/lemon: compression unsupported by the installed binary";
    {
        let mut st = p.status.lock().unwrap();
        st.state = meduza_np_protocol::report::SyncState::ApplyFailed;
        st.last_error = Some(primary.into());
        st.failed = Some(("rev-000010".into(), "digest".into()));
    }
    p.restore_last_good(&reg).await;
    let report = meduza_np_planner::reporter::build(&p);
    assert_eq!(report.sync.last_error.as_deref(), Some(primary));
    let recovery = report.sync.recovery_error.unwrap();
    assert!(
        recovery.contains("rev-000001") && recovery.contains("services.avahi"),
        "historical revision and validation failure must be identified: {recovery}"
    );
    std::fs::rename(&avahi, &parked).unwrap();
    let absent = registry::scan(&env.paths.packages, &plat);
    p.restore_last_good(&absent).await;
    let report = meduza_np_planner::reporter::build(&p);
    assert_eq!(report.sync.last_error.as_deref(), Some(primary), "successful compensation does not erase the desired revision's failure");
    assert!(report.sync.recovery_error.is_none(), "recovered errors must not remain stale");
    p.engine.supervisor.stop_all().await;
    let _ = std::fs::remove_dir_all(env.paths.etc.parent().unwrap());
}

/// Planned planner restart (D32): the daemon keeps running and the next planner adopts it for the same
/// configuration; a switched provider binary restarts it.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "current_thread")]
async fn planner_restart_adopts_daemons() {
    let _native_env = NATIVE_TOOL_ENV.lock().await;
    let Some(mut env) = setup("adopt") else { return };
    // No WireGuard: its `wg syncconf` would log into apply_lifecycle's tool log (MNP_FAKE_TOOL_LOG is process-wide).
    env.have_wg = false;
    let reg = registry::scan(&env.paths.packages, &meduza_np_plugin_sdk::Platform::default());
    let log_arg = env.log.to_string_lossy().to_string();
    let (raw, digest) = config(&env, 1, json!([{"id": "a", "log": log_arg}]));
    let prepared = env.engine.prepare(&raw, &digest, &reg, &Default::default()).await.expect("prepare");
    env.engine.commit(&prepared, &reg).await.expect("commit");
    let pid1 = env.engine.supervisor.status("avahi/a").unwrap().pid.unwrap();
    let alive = |pid: u32| {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|s| !s.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'))
            .unwrap_or(false)
    };

    env.engine.supervisor.detach_all().await;
    assert!(alive(pid1), "detached daemon keeps running");

    // The next planner restores the same configuration and adopts the daemon.
    let second = engine(&env.paths, &env.net);
    assert!(second.supervisor.live_recorded_pids().contains(&pid1));
    let prepared = second.prepare(&raw, &digest, &reg, &Default::default()).await.unwrap();
    second.commit(&prepared, &reg).await.expect("commit after restart");
    second.supervisor.reap_unclaimed().await;
    let st = second.supervisor.status("avahi/a").unwrap();
    assert_eq!((st.pid, st.adopted), (Some(pid1), true), "same daemon adopted");
    assert!(alive(pid1));

    // Provider upgrade while planner was down: the slot now resolves to another binary, so it is restarted.
    second.supervisor.detach_all().await;
    let target = target_dir();
    let tool2 = env.paths.packages.join("meduza-np-openvpn-pkg/bin/fake_tool2");
    std::fs::create_dir_all(tool2.parent().unwrap()).unwrap();
    std::fs::copy(target.join("examples").join(format!("fake_tool{EXE}")), &tool2).unwrap();
    let manifest_path = env.paths.packages.join("meduza-np-openvpn-pkg/manifest.json");
    let mut manifest: Value = serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    manifest["providers"][0]["binary"] = json!(tool2.to_string_lossy());
    write_json(&manifest_path, &manifest);
    let reg = registry::scan(&env.paths.packages, &meduza_np_plugin_sdk::Platform::default());
    let third = engine(&env.paths, &env.net);
    let prepared = third.prepare(&raw, &digest, &reg, &Default::default()).await.unwrap();
    third.commit(&prepared, &reg).await.expect("commit after provider switch");
    let st = third.supervisor.status("avahi/a").unwrap();
    assert!(st.pid != Some(pid1) && !st.adopted, "switched binary restarts the daemon");
    assert!(!alive(pid1), "the old daemon was stopped");

    third.supervisor.stop_all().await;
    assert!(third.supervisor.live_recorded_pids().is_empty());
    let _ = std::fs::remove_dir_all(env.paths.etc.parent().unwrap());
}
