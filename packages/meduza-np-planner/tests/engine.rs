//! Apply engine integration test with real plugin executables (the WireGuard plugin and a test plugin from
//! `examples/`), a fake native tool, the real supervisor and a recording network backend.
//! First run `cargo build --locked -p meduza-np-wireguard -p meduza-np-planner --bins --examples`,
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

    let log = root.join("tool.log");
    let net = RecordingBackend::default();
    let engine = engine(&paths, &net);
    Some(Env { paths, engine, net, log, have_wg })
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

    for failure in ["spawn", "syntax", "resource", "wireguard-resource"] {
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
    env.engine.supervisor.stop_all().await;
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
