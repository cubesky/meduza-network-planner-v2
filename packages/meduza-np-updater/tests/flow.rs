//! End-to-end updater flow against a signed `file://` release source and a fake planner that reads the switched
//! view: bootstrap, upgrade with planner restart, broken release rolled back and remembered, metadata attacks
//! (rollback, tampering, expiry, untrusted key) and recovery of an interrupted switch.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use meduza_np_protocol::group::{Artifact, ChannelPointer, Compatibility, GroupManifest, GroupPackage, TrustStore};
use meduza_np_protocol::manifest::ApiRange;
use meduza_np_updater::config::{Backend, Config};
use meduza_np_updater::engine::{Engine, Outcome, Phase};
use meduza_np_updater::openwrt::Runner;
use meduza_np_updater::planner::PlannerApi;
use meduza_np_updater::sig;
use serde_json::{json, Value};

struct Env {
    root: PathBuf,
    repo: PathBuf,
    key: SigningKey,
}

impl Env {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("mnp-updater-flow-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join("packages")).unwrap();
        std::fs::create_dir_all(root.join("trust")).unwrap();
        let key = SigningKey::from_bytes(&[42; 32]);
        let trust = TrustStore { format_version: 1, threshold: 1, keys: vec![sig::trusted_key(&key.verifying_key(), None)] };
        std::fs::write(root.join("trust/release-keys.json"), serde_json::to_vec(&trust).unwrap()).unwrap();
        Self { root, repo, key }
    }

    fn config(&self, components: Option<Vec<&str>>) -> Config {
        Config {
            base_url: format!("file:///{}", self.repo.to_string_lossy().replace('\\', "/").trim_start_matches('/')),
            channel: "stable".into(),
            group_version: None,
            data_dir: self.root.join("data"),
            backend: Backend::Store,
            platform: "linux/amd64".into(),
            components: components.map(|c| c.iter().map(|s| s.to_string()).collect()),
            check_interval: Duration::from_secs(3600),
            auto_apply: true,
            window: None,
            trust_files: vec![self.root.join("trust/release-keys.json")],
            image_updater_version: Some("0.1.0".into()),
            paths: meduza_np_runtime::paths::NodePaths::under(&self.root.join("node")),
        }
    }

    /// Builds a package archive into the repo; `broken` makes the fake planner refuse to register it.
    fn package(&self, name: &str, version: &str, broken: bool) -> GroupPackage {
        let provides = if broken { "broken.test".to_string() } else { format!("{}.test", name.trim_start_matches("meduza-np-")) };
        let conflicts: Vec<&str> = meduza_np_protocol::manifest::CONFLICTS
            .iter()
            .filter_map(|(a, b)| {
                if *a == name {
                    Some(*b)
                } else if *b == name {
                    Some(*a)
                } else {
                    None
                }
            })
            .collect();
        let manifest = json!({
            "manifestVersion": 1, "package": name, "version": version, "role": "cli",
            "plannerApi": {"min": 1, "max": 1}, "requires": [], "conflicts": conflicts, "provides": [provides]
        });
        let file = format!("packages/{name}_{version}{}.tar.gz", if broken { "-broken" } else { "" });
        let path = self.repo.join(&file);
        let f = std::fs::File::create(&path).unwrap();
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(f, flate2::Compression::fast()));
        let entries = [
            (format!("usr/share/meduza-np/packages/{name}/manifest.json"), manifest.to_string(), 0o644),
            (format!("usr/libexec/meduza-np/test/{name}"), format!("{name} {version}"), 0o755),
        ];
        for (p, data, mode) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(mode);
            h.set_cksum();
            b.append_data(&mut h, p, data.as_bytes()).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
        let (size, sha256) = meduza_np_updater::store::sha256_file(&path).unwrap();
        GroupPackage {
            name: name.into(),
            version: version.into(),
            fingerprint: "0".repeat(64),
            reused_from_group: None,
            requires: if name == "meduza-np-planner" || name == "meduza-np-updater" { vec![] } else { vec!["meduza-np-planner".into()] },
            conflicts: vec![],
            artifacts: vec![Artifact { platform: "linux/amd64".into(), format: "tar.gz".into(), url: file, size, sha256, oci: None }],
            upstream: None,
            migration: None,
        }
    }

    fn sign(&self, rel: &str) {
        let bytes = std::fs::read(self.repo.join(rel)).unwrap();
        let s = sig::sign(&bytes, std::slice::from_ref(&self.key));
        std::fs::write(self.repo.join(format!("{rel}.sig")), serde_json::to_vec(&s).unwrap()).unwrap();
    }

    fn publish(&self, version: &str, sequence: u64, packages: Vec<GroupPackage>, expires: &str) {
        let r = ApiRange { min: 1, max: 1 };
        let m = GroupManifest {
            format_version: 1,
            group_version: version.into(),
            sequence,
            source_commit: "abc".into(),
            published_at: "2026-10-01T00:00:00Z".into(),
            expires_at: expires.into(),
            platforms: vec!["linux/amd64".into()],
            compatibility: Compatibility { planner_api: r, manager_api: r, config_schema: r },
            packages,
            notes: None,
        };
        let rel = format!("releases/{version}/manifest.json");
        std::fs::create_dir_all(self.repo.join(format!("releases/{version}"))).unwrap();
        let bytes = serde_json::to_vec_pretty(&m).unwrap();
        std::fs::write(self.repo.join(&rel), &bytes).unwrap();
        self.sign(&rel);
        self.point(version, sequence, &meduza_np_protocol::digest::sha256_hex(&bytes), expires);
    }

    fn point(&self, version: &str, sequence: u64, sha: &str, expires: &str) {
        let p = ChannelPointer {
            format_version: 1,
            channel: "stable".into(),
            group_version: version.into(),
            sequence,
            manifest: format!("releases/{version}/manifest.json"),
            manifest_sha256: sha.into(),
            expires_at: expires.into(),
            published_at: "2026-10-01T00:00:00Z".into(),
        };
        std::fs::create_dir_all(self.repo.join("channels")).unwrap();
        std::fs::write(self.repo.join("channels/stable.json"), serde_json::to_vec(&p).unwrap()).unwrap();
        self.sign("channels/stable.json");
    }
}

/// Planner stand-in: registers whatever the current view contains (like the real registry scan), refuses
/// packages marked `broken`, changes boot id on restart, records every call.
struct FakePlanner {
    store: PathBuf,
    up: Cell<bool>,
    boot: Cell<u32>,
    calls: RefCell<Vec<String>>,
}

impl FakePlanner {
    fn packages(&self) -> Value {
        let store = meduza_np_updater::store::Store::new(self.store.clone());
        let Some(dir) = store.current_path("usr/share/meduza-np/packages") else { return json!({"packages": []}) };
        let mut out = vec![];
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let m: Value = serde_json::from_slice(&std::fs::read(e.path().join("manifest.json")).unwrap()).unwrap();
            let broken = m["provides"][0] == "broken.test";
            out.push(json!({"name": m["package"], "version": m["version"], "registration": if broken {"invalid"} else {"registered"}, "reason": "test"}));
        }
        json!({"packages": out}) // same envelope as planner's local `packages` method
    }
}

impl PlannerApi for FakePlanner {
    fn call(&self, method: &str, _params: Value) -> Result<Value, String> {
        if !self.up.get() {
            return Err("unavailable: not running".into());
        }
        self.calls.borrow_mut().push(method.to_string());
        match method {
            "status" => Ok(json!({"bootId": format!("boot-{}", self.boot.get())})),
            "planner.restart" => {
                self.boot.set(self.boot.get() + 1);
                Ok(json!({"restarting": true}))
            }
            "updater.activate" => {
                meduza_np_updater::handoff::activate(
                    &meduza_np_updater::store::Store::new(self.store.clone()),
                    Backend::Store,
                    &NoRunner,
                    &self.store.join("run"),
                )?;
                Ok(json!({"activated": true}))
            }
            "packages" => Ok(self.packages()),
            "instances" => Ok(json!({"processes": [], "applied": {"wireguard/wg0": {"package": "meduza-np-wireguard", "processes": []}}})),
            _ => Ok(json!({})),
        }
    }
}

struct NoRunner;
impl Runner for NoRunner {
    fn run(&self, program: &str, _args: &[String]) -> Result<String, String> {
        Err(format!("{program} must not run for the store backend"))
    }
}

fn now() -> u64 {
    meduza_np_plugin_sdk::rfc3339_to_unix("2026-10-22T00:00:00Z").unwrap()
}

fn engine<'a>(cfg: &'a Config, planner: &'a dyn PlannerApi, now_fn: &'a dyn Fn() -> u64) -> Engine<'a> {
    Engine { cfg, planner, runner: &NoRunner, now: now_fn, planner_timeout: Duration::from_millis(500), poll: Duration::from_millis(10) }
}

fn current_file(cfg: &Config, rel: &str) -> Option<String> {
    let store = meduza_np_updater::store::Store::new(cfg.store_dir());
    std::fs::read_to_string(store.current_path(rel)?).ok()
}

const FAR: &str = "2099-01-01T00:00:00Z";

#[test]
fn obsolete_downloads_are_collected_on_checks_failures_and_restart_while_rollback_survives() {
    let env = Env::new("automatic-gc");
    let cfg = env.config(Some(vec![]));
    let planner = FakePlanner { store: cfg.store_dir(), up: Cell::new(false), boot: Cell::new(1), calls: RefCell::new(vec![]) };
    let now_fn = now;
    let e = engine(&cfg, &planner, &now_fn);
    let updater = env.package("meduza-np-updater", "0.1.0", false);
    for n in 1..=3 {
        let version = format!("0.{n}.0");
        env.publish(&format!("v{version}"), n, vec![env.package("meduza-np-planner", &version, false), updater.clone()], FAR);
        assert_eq!(e.run_once(true).unwrap().1, Some(Outcome::Applied));
    }
    let store = e.store();
    let objects = store.objects().join("meduza-np-planner");
    let versions =
        || std::fs::read_dir(&objects).unwrap().map(|p| p.unwrap().file_name().to_string_lossy().into_owned()).collect::<Vec<_>>();
    assert_eq!(versions().len(), 2, "only current and one rollback version remain");
    assert!(versions().iter().all(|v| !v.starts_with("0.1.0-")));
    let orphan = objects.join("0.0.0-orphan");
    std::fs::create_dir_all(&orphan).unwrap();
    std::fs::write(orphan.join("binary"), vec![1u8; 8192]).unwrap();
    let partial = store.staging().join("old-download.part");
    std::fs::write(&partial, vec![1u8; 8192]).unwrap();
    let lock = store.operation_lock().unwrap();
    assert!(e.check().is_err(), "another process cannot collect an active download");
    assert!(partial.exists() && orphan.exists());
    drop(lock);
    let calls = planner.calls.borrow().clone();
    e.run_once(false).unwrap();
    assert_eq!(*planner.calls.borrow(), calls, "housekeeping does not restart/reapply services");
    assert!(!partial.exists() && !orphan.exists());
    assert_eq!(versions().len(), 2);

    let mut broken = env.package("meduza-np-planner", "0.4.0", false);
    broken.artifacts[0].url = "missing-package.tar.gz".into();
    env.publish("v0.4.0", 4, vec![broken, updater], FAR);
    assert!(e.run_once(true).is_err());
    assert!(std::fs::read_dir(store.staging()).unwrap().next().is_none(), "failed downloads are removed immediately");
    assert_eq!(versions().len(), 2, "failed attempts must not forget the older valid rollback baseline");
    assert!(versions().iter().any(|v| v.starts_with("0.2.0-")));
    std::fs::write(store.staging().join("interrupted-download"), b"partial").unwrap();
    assert!(e.recover(false).is_none());
    assert!(std::fs::read_dir(store.staging()).unwrap().next().is_none(), "startup also cleans when no journal needs recovery");
    assert_eq!(e.installed().unwrap().packages["meduza-np-planner"].version, "0.3.0");
    std::fs::remove_dir_all(env.root).unwrap();
}

#[test]
fn failed_native_rollback_preserves_repair_downloads_even_if_the_package_database_still_shows_old_versions() {
    struct NativePackages;
    impl Runner for NativePackages {
        fn run(&self, program: &str, _args: &[String]) -> Result<String, String> {
            assert_eq!(program, "opkg");
            Ok("meduza-np-planner - 0.1.0\n".into())
        }
    }
    let env = Env::new("native-repair-gc");
    let mut cfg = env.config(None);
    cfg.backend = Backend::Opkg;
    let planner = FakePlanner { store: cfg.store_dir(), up: Cell::new(false), boot: Cell::new(1), calls: RefCell::new(vec![]) };
    let now_fn = now;
    let native = NativePackages;
    let mut e = engine(&cfg, &planner, &now_fn);
    e.runner = &native;
    let store = e.store();
    store.init().unwrap();
    let current: meduza_np_protocol::group::InstalledSet =
        serde_json::from_value(json!({"formatVersion":1,"generation":7,"backend":"opkg","platform":"openwrt/test","activatedAt":"",
        "packages":{"meduza-np-planner":{"version":"0.1.0","source":"group:stable"}}}))
        .unwrap();
    store.save_installed(&current).unwrap();
    let mut target = current.clone();
    target.packages.get_mut("meduza-np-planner").unwrap().version = "0.2.0".into();
    let mut last = json!({"formatVersion":1,"groupVersion":"v0.2.0","phase":"failed","startedAt":"","updatedAt":"",
        "previousInstalled":current,"newInstalled":target,"repairRequired":true});
    let file = store.root.join("journal.last.json");
    meduza_np_runtime::fsutil::write_json_atomic(&file, &last, 0o600).unwrap();
    let repair = store.staging().join("meduza-np-planner_0.2.0.ipk");
    std::fs::write(&repair, b"repair archive").unwrap();
    e.recover(false);
    assert!(repair.exists(), "a failed install may change files before updating its native package database");
    last["repairRequired"] = json!(false);
    meduza_np_runtime::fsutil::write_json_atomic(&file, &last, 0o600).unwrap();
    e.recover(false);
    assert!(!repair.exists(), "obsolete repair downloads are collected after repair is complete");
    std::fs::remove_dir_all(env.root).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn old_objects_referenced_by_supervised_processes_are_not_collected() {
    let env = Env::new("gc-live-record");
    let cfg = env.config(Some(vec![]));
    let planner = FakePlanner { store: cfg.store_dir(), up: Cell::new(false), boot: Cell::new(1), calls: RefCell::new(vec![]) };
    let now_fn = now;
    let e = engine(&cfg, &planner, &now_fn);
    let store = e.store();
    store.init().unwrap();
    let old = store.objects().join("meduza-np-planner/old-running");
    std::fs::create_dir_all(&old).unwrap();
    let record = cfg.paths.run.join("supervisor/process.json");
    meduza_np_runtime::fsutil::write_json_atomic(
        &record,
        &json!({"program":old.join("usr/bin/old"),
        "bootId":std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap().trim()}),
        0o600,
    )
    .unwrap();
    e.recover(false);
    assert!(old.exists(), "a planner namespace adoption record protects the running version");
    std::fs::remove_file(record).unwrap();
    e.recover(false);
    assert!(!old.exists(), "unreferenced binaries are removed on the next automatic pass");
    std::fs::remove_dir_all(env.root).unwrap();
}

#[test]
fn manager_check_is_read_only_and_upgrade_now_overrides_pause() {
    let env = Env::new("manager-update-controls");
    let cfg = env.config(Some(vec![]));
    let planner = FakePlanner { store: cfg.store_dir(), up: Cell::new(false), boot: Cell::new(1), calls: RefCell::new(vec![]) };
    let now_fn = now;
    let e = engine(&cfg, &planner, &now_fn);
    let updater = env.package("meduza-np-updater", "0.1.0", false);
    env.publish("v0.1.0", 1, vec![env.package("meduza-np-planner", "0.1.0", false), updater.clone()], FAR);
    e.run_once(true).unwrap();
    planner.up.set(true);
    env.publish("v0.2.0", 2, vec![env.package("meduza-np-planner", "0.2.0", false), updater], FAR);
    let (plan, outcome) = meduza_np_updater::daemon::run_pass(&e, Some(false)).unwrap();
    assert!(plan.unwrap().changed());
    assert!(outcome.is_none());
    assert_eq!(
        e.installed().unwrap().packages["meduza-np-planner"].version,
        "0.1.0",
        "checking must not apply even with autoApply enabled"
    );
    meduza_np_runtime::fsutil::write_json_atomic(&e.store().root.join("control.json"), &json!({"paused":true}), 0o644).unwrap();
    assert!(meduza_np_updater::daemon::run_pass(&e, None).unwrap().1.is_none());
    assert_eq!(meduza_np_updater::daemon::run_pass(&e, Some(true)).unwrap().1, Some(Outcome::Applied));
    assert_eq!(e.installed().unwrap().packages["meduza-np-planner"].version, "0.2.0");
    assert!(e.load_control().paused, "one-shot manual upgrade does not change the automatic-update policy");
    std::fs::remove_dir_all(env.root).unwrap();
}

#[test]
fn readonly_planner_helper_failure_falls_back_to_the_writable_updater_owner() {
    struct ReadonlyPlanner<'a>(&'a FakePlanner, u8);
    impl PlannerApi for ReadonlyPlanner<'_> {
        fn call(&self, method: &str, params: Value) -> Result<Value, String> {
            if method == "updater.activate" {
                if self.1 == 1 {
                    // The helper switched successfully but its acknowledgement was lost.
                    self.0.call(method, params)?;
                } else if self.1 == 2 {
                    let file = self.0.store.join("journal.json");
                    let mut journal: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
                    journal["handoff"]["installed"]["packages"]["meduza-np-planner"]["version"] = json!("unverified");
                    meduza_np_runtime::fsutil::write_json_atomic(&file, &journal, 0o600).unwrap();
                }
                return Err("update-failed: updater activation helper failed: exit status: 1: current.new: Read-only file system".into());
            }
            self.0.call(method, params)
        }
    }
    for mode in 0..3 {
        let env = Env::new(&format!("readonly-planner-handoff-{mode}"));
        let cfg = env.config(Some(vec![]));
        let planner = FakePlanner { store: cfg.store_dir(), up: Cell::new(false), boot: Cell::new(1), calls: RefCell::new(vec![]) };
        let readonly = ReadonlyPlanner(&planner, mode);
        let now_fn = now;
        let e = engine(&cfg, &readonly, &now_fn);
        let names = ["meduza-np-planner", "meduza-np-updater"];
        env.publish("v0.1.0", 1, names.iter().map(|name| env.package(name, "0.1.0", false)).collect(), FAR);
        assert_eq!(e.run_once(true).unwrap().1, Some(Outcome::Applied));
        planner.up.set(true);
        env.publish("v0.2.0", 2, names.iter().map(|name| env.package(name, "0.2.0", false)).collect(), FAR);
        let outcome = e.run_once(true).unwrap().1;
        if mode == 2 {
            let Some(Outcome::RolledBack(reason)) = outcome else { panic!("a malformed handoff must roll back") };
            assert!(reason.contains("owner updater activation failed") && reason.contains("preserve every other package"));
            assert_eq!(e.installed().unwrap().packages["meduza-np-planner"].version, "0.1.0");
            assert_eq!(e.installed().unwrap().packages["meduza-np-updater"].version, "0.1.0");
            std::fs::remove_dir_all(env.root).unwrap();
            continue;
        }
        assert_eq!(outcome, Some(Outcome::AppliedRestartSelf));
        let installed = e.installed().unwrap();
        assert_eq!(installed.packages["meduza-np-updater"].version, "0.2.0");
        assert_eq!(installed.packages["meduza-np-planner"].version, "0.1.0", "only updater changes before independent takeover");
        assert!(e.recover(true).unwrap().contains("complete"));
        assert_eq!(e.installed().unwrap().packages["meduza-np-planner"].version, "0.2.0");
        std::fs::remove_dir_all(env.root).unwrap();
    }
}

#[test]
fn planner_activates_updater_then_new_updater_upgrades_planner_without_network() {
    struct ExecutableHelperPlanner<'a> {
        planner: &'a FakePlanner,
        cfg: &'a Config,
    }
    impl PlannerApi for ExecutableHelperPlanner<'_> {
        fn call(&self, method: &str, params: Value) -> Result<Value, String> {
            if method != "updater.activate" {
                return self.planner.call(method, params);
            }
            self.planner.calls.borrow_mut().push(method.into());
            let out = std::process::Command::new(env!("CARGO_BIN_EXE_meduza-np-updater"))
                .arg("activate-updater")
                .env("MEDUZA_NP_DATA_DIR", &self.cfg.data_dir)
                .env("MEDUZA_NP_ETC_DIR", &self.cfg.paths.etc)
                .env("MEDUZA_NP_RUN_DIR", &self.cfg.paths.run)
                .env("MEDUZA_NP_UPDATE_BACKEND", "store")
                .env("MEDUZA_NP_PLATFORM", "linux/amd64")
                .output()
                .map_err(|e| e.to_string())?;
            if !out.status.success() {
                return Err(String::from_utf8_lossy(&out.stderr).into_owned());
            }
            Ok(json!({"activated": true}))
        }
    }
    let env = Env::new("updater-first");
    let cfg = env.config(Some(vec!["meduza-np-wireguard"]));
    let planner = FakePlanner { store: cfg.store_dir(), up: Cell::new(false), boot: Cell::new(1), calls: RefCell::new(vec![]) };
    let rpc = ExecutableHelperPlanner { planner: &planner, cfg: &cfg };
    let now_fn = now;
    let e = engine(&cfg, &rpc, &now_fn);
    let names = ["meduza-np-planner", "meduza-np-updater", "meduza-np-wireguard"];
    env.publish("v0.1.0", 1, names.iter().map(|n| env.package(n, "0.1.0", false)).collect(), FAR);
    assert_eq!(e.run_once(true).unwrap().1, Some(Outcome::Applied));
    planner.up.set(true);
    planner.calls.borrow_mut().clear();
    env.publish("v0.2.0", 2, names.iter().map(|n| env.package(n, "0.2.0", false)).collect(), FAR);
    assert_eq!(e.run_once(true).unwrap().1, Some(Outcome::AppliedRestartSelf));
    let staged: Vec<_> = std::fs::read_dir(e.store().staging()).unwrap().map(|e| e.unwrap().path()).collect();
    assert!(!staged.is_empty());
    e.check().unwrap();
    assert!(staged.iter().all(|p| p.exists()), "checks must preserve downloads needed by the pending updater/planner handoff");
    let halfway = e.installed().unwrap();
    assert_eq!(halfway.group_version.as_deref(), Some("v0.1.0"), "the group is not complete just because updater was activated");
    assert_eq!(halfway.packages["meduza-np-updater"].version, "0.2.0");
    assert_eq!(halfway.packages["meduza-np-planner"].version, "0.1.0");
    assert_eq!(halfway.packages["meduza-np-wireguard"].version, "0.1.0");
    assert!(planner.calls.borrow().iter().any(|m| m == "updater.activate"));
    assert!(!planner.calls.borrow().iter().any(|m| matches!(m.as_str(), "planner.restart" | "registry.rescan" | "drain")));
    assert_eq!(e.load_journal().unwrap().phase, Phase::SelfRestart);
    assert_eq!(meduza_np_runtime::update::snapshot(&cfg.store_dir(), &cfg.paths.run).unwrap()["phase"], "self-restart");

    // The new updater continues the original, fully verified transaction even if the channel is now unreachable.
    std::fs::remove_dir_all(&env.repo).unwrap();
    assert!(e.recover(true).unwrap().contains("new updater upgraded planner"));
    let completed = e.installed().unwrap();
    assert_eq!(completed.group_version.as_deref(), Some("v0.2.0"));
    for name in names {
        assert_eq!(completed.packages[name].version, "0.2.0");
    }
    assert!(planner.calls.borrow().iter().any(|m| m == "planner.restart"));
    assert!(e.load_journal().is_none());
    assert_eq!(e.load_state().last_result.unwrap().outcome, "done");
    std::fs::remove_dir_all(env.root).unwrap();
}

#[test]
fn failed_takeover_and_failed_planner_have_distinct_recovery_baselines() {
    for (tag, new_updater_starts) in
        [("takeover-fails", false), ("before-activation", false), ("before-record", true), ("planner-fails", true)]
    {
        let env = Env::new(tag);
        let cfg = env.config(Some(vec![]));
        let planner = FakePlanner { store: cfg.store_dir(), up: Cell::new(false), boot: Cell::new(1), calls: RefCell::new(vec![]) };
        let now_fn = now;
        let e = engine(&cfg, &planner, &now_fn);
        env.publish(
            "v0.1.0",
            1,
            vec![env.package("meduza-np-planner", "0.1.0", false), env.package("meduza-np-updater", "0.1.0", false)],
            FAR,
        );
        e.run_once(true).unwrap();
        planner.up.set(true);
        planner.calls.borrow_mut().clear();
        env.publish(
            "v0.2.0",
            2,
            vec![env.package("meduza-np-planner", "0.2.0", true), env.package("meduza-np-updater", "0.2.0", false)],
            FAR,
        );
        assert_eq!(e.run_once(true).unwrap().1, Some(Outcome::AppliedRestartSelf));
        if tag == "before-activation" || tag == "before-record" {
            let j = e.load_journal().unwrap();
            if tag == "before-activation" {
                // Crash after journaling but before the helper switched the updater view.
                e.store().switch(j.previous_view.as_deref().unwrap()).unwrap();
            }
            // Crash after switch but before installed.json was persisted is also recoverable.
            e.store().save_installed(j.previous_installed.as_ref().unwrap()).unwrap();
        }
        let msg = e.recover(new_updater_starts).unwrap();
        assert!(msg.contains("rolled back"), "{msg}");
        let installed = e.installed().unwrap();
        assert_eq!(installed.group_version.as_deref(), Some("v0.1.0"));
        assert_eq!(installed.packages["meduza-np-planner"].version, "0.1.0");
        assert_eq!(installed.packages["meduza-np-updater"].version, if new_updater_starts { "0.2.0" } else { "0.1.0" });
        assert!(e.load_journal().is_none());
        assert!(e.load_state().failed_groups.contains(&"v0.2.0".into()));
        if !new_updater_starts {
            assert!(!planner.calls.borrow().iter().any(|m| m == "planner.restart"), "failed updater must never touch planner");
        }
        std::fs::remove_dir_all(env.root).unwrap();
    }
}

#[test]
fn updater_first_bootstrap_and_previous_planner_protocol_are_supported() {
    struct PreviousPlanner<'a>(&'a FakePlanner);
    impl PlannerApi for PreviousPlanner<'_> {
        fn call(&self, method: &str, params: Value) -> Result<Value, String> {
            if method == "updater.activate" {
                Err("not-found: unknown method".into())
            } else {
                self.0.call(method, params)
            }
        }
    }
    for bootstrap in [false, true] {
        let env = Env::new(if bootstrap { "bootstrap-self-first" } else { "previous-planner" });
        let cfg = env.config(Some(vec![]));
        let planner = FakePlanner { store: cfg.store_dir(), up: Cell::new(false), boot: Cell::new(1), calls: RefCell::new(vec![]) };
        let compat = PreviousPlanner(&planner);
        let now_fn = now;
        let e = engine(&cfg, &compat, &now_fn);
        if !bootstrap {
            env.publish(
                "v0.1.0",
                1,
                vec![env.package("meduza-np-planner", "0.1.0", false), env.package("meduza-np-updater", "0.1.0", false)],
                FAR,
            );
            assert_eq!(e.run_once(true).unwrap().1, Some(Outcome::Applied));
            planner.up.set(true);
        }
        env.publish(
            "v0.2.0",
            2,
            vec![env.package("meduza-np-planner", "0.2.0", false), env.package("meduza-np-updater", "0.2.0", false)],
            FAR,
        );
        assert_eq!(e.run_once(true).unwrap().1, Some(Outcome::AppliedRestartSelf));
        assert_eq!(
            e.installed().unwrap().packages.get("meduza-np-planner").map(|p| p.version.as_str()),
            if bootstrap { None } else { Some("0.1.0") }
        );
        assert!(e.recover(true).unwrap().contains("complete"));
        assert_eq!(e.installed().unwrap().packages["meduza-np-planner"].version, "0.2.0");
        std::fs::remove_dir_all(env.root).unwrap();
    }
}

#[test]
fn native_opkg_and_apk_install_updater_before_planner() {
    struct PackageManager {
        backend: Backend,
        installed: RefCell<BTreeMap<String, String>>,
        installs: RefCell<Vec<String>>,
    }
    impl Runner for PackageManager {
        fn run(&self, program: &str, args: &[String]) -> Result<String, String> {
            assert_eq!(program, self.backend.name());
            if args[0] == "list-installed" || args[0] == "list" {
                return Ok(self
                    .installed
                    .borrow()
                    .iter()
                    .map(|(name, version)| {
                        if self.backend == Backend::Opkg {
                            format!("{name} - {version}\n")
                        } else {
                            format!("{name}-{version}-r1 aarch64 {{{name}}} [installed]\n")
                        }
                    })
                    .collect());
            }
            assert!(matches!(args[0].as_str(), "add" | "install"));
            let file = PathBuf::from(args.last().unwrap());
            let name = file.file_name().unwrap().to_str().unwrap().split('_').next().unwrap().to_string();
            if name == "meduza-np-planner" {
                assert_eq!(self.installed.borrow()["meduza-np-updater"], "0.2.0", "native planner install must run under the new updater");
            }
            self.installed.borrow_mut().insert(name.clone(), "0.2.0".into());
            self.installs.borrow_mut().push(name);
            Ok(String::new())
        }
    }
    struct NativePlanner<'a> {
        pm: &'a PackageManager,
        store: PathBuf,
        boot: Cell<u32>,
        calls: RefCell<Vec<String>>,
    }
    impl PlannerApi for NativePlanner<'_> {
        fn call(&self, method: &str, _: Value) -> Result<Value, String> {
            self.calls.borrow_mut().push(method.into());
            match method {
                "status" => Ok(json!({"bootId":self.boot.get().to_string()})),
                "updater.activate" => {
                    meduza_np_updater::handoff::activate(
                        &meduza_np_updater::store::Store::new(self.store.clone()),
                        self.pm.backend,
                        self.pm,
                        &self.store.join("run"),
                    )?;
                    Ok(json!({"activated":true}))
                }
                "planner.restart" => {
                    self.boot.set(self.boot.get() + 1);
                    Ok(json!({}))
                }
                "packages" => Ok(
                    json!({"packages": self.pm.installed.borrow().iter().map(|(n,v)| json!({"name":n,"version":v,"registration":"registered"})).collect::<Vec<_>>()}),
                ),
                _ => Ok(json!({})),
            }
        }
    }
    for backend in [Backend::Opkg, Backend::Apk] {
        let env = Env::new(backend.name());
        let mut cfg = env.config(None);
        cfg.backend = backend;
        cfg.image_updater_version = None;
        let names = ["meduza-np-planner", "meduza-np-updater", "meduza-np-cli"];
        let pm = PackageManager {
            backend,
            installed: RefCell::new(names.iter().map(|n| (n.to_string(), "0.1.0".into())).collect()),
            installs: RefCell::new(vec![]),
        };
        let planner = NativePlanner { pm: &pm, store: cfg.store_dir(), boot: Cell::new(1), calls: RefCell::new(vec![]) };
        let packages = names
            .iter()
            .map(|n| {
                let mut p = env.package(n, "0.2.0", false);
                p.artifacts[0].format = if backend == Backend::Opkg { "ipk" } else { "apk" }.into();
                p
            })
            .collect();
        env.publish("v0.2.0", 2, packages, FAR);
        let e = Engine {
            cfg: &cfg,
            planner: &planner,
            runner: &pm,
            now: &now,
            planner_timeout: Duration::from_millis(500),
            poll: Duration::from_millis(10),
        };
        assert_eq!(e.run_once(true).unwrap().1, Some(Outcome::AppliedRestartSelf));
        assert_eq!(*pm.installs.borrow(), ["meduza-np-updater"]);
        assert_eq!(pm.installed.borrow()["meduza-np-planner"], "0.1.0");
        assert!(!planner.calls.borrow().iter().any(|m| m == "planner.restart"));
        let msg = e.recover(true).unwrap();
        assert!(msg.contains("complete"), "{msg}");
        assert_eq!(*pm.installs.borrow(), ["meduza-np-updater", "meduza-np-planner", "meduza-np-cli"]);
        assert!(planner.calls.borrow().iter().any(|m| m == "planner.restart"));
        assert_eq!(e.installed().unwrap().group_version.as_deref(), Some("v0.2.0"));
        assert!(e.load_journal().is_none());
        std::fs::remove_dir_all(env.root).unwrap();
    }
}

#[test]
fn updater_only_upgrade_does_not_restart_planner() {
    let env = Env::new("updater-only");
    let cfg = env.config(Some(vec![]));
    let planner = FakePlanner { store: cfg.store_dir(), up: Cell::new(false), boot: Cell::new(1), calls: RefCell::new(vec![]) };
    let now_fn = now;
    let e = engine(&cfg, &planner, &now_fn);
    let p = env.package("meduza-np-planner", "0.1.0", false);
    env.publish("v0.1.0", 1, vec![p.clone(), env.package("meduza-np-updater", "0.1.0", false)], FAR);
    e.run_once(true).unwrap();
    planner.up.set(true);
    planner.calls.borrow_mut().clear();
    env.publish("v0.2.0", 2, vec![p, env.package("meduza-np-updater", "0.2.0", false)], FAR);
    assert_eq!(e.run_once(true).unwrap().1, Some(Outcome::AppliedRestartSelf));
    assert!(e.recover(true).unwrap().contains("complete"));
    assert!(!planner.calls.borrow().iter().any(|m| m == "planner.restart"));
    assert_eq!(e.installed().unwrap().packages["meduza-np-planner"].version, "0.1.0");
    std::fs::remove_dir_all(env.root).unwrap();
}

#[test]
fn retries_reuse_only_downloads_matching_signed_hashes() {
    let env = Env::new("verified-download-cache");
    let cfg = env.config(Some(vec!["meduza-np-wireguard"]));
    let planner = FakePlanner { store: cfg.store_dir(), up: Cell::new(false), boot: Cell::new(1), calls: RefCell::new(vec![]) };
    let now_fn = now;
    let e = engine(&cfg, &planner, &now_fn);
    env.publish(
        "v0.1.0",
        1,
        vec![
            env.package("meduza-np-planner", "0.1.0", false),
            env.package("meduza-np-wireguard", "0.1.0", false),
            env.package("meduza-np-updater", "0.1.0", false),
        ],
        FAR,
    );
    let group = e.check().unwrap();
    let plan = e.plan(&group).unwrap();
    for (index, action) in plan.actions.iter().filter(|a| a.downloads()).enumerate() {
        let art = action.artifact.as_ref().unwrap();
        let source = env.repo.join(&art.url);
        let cached = e.store().staging().join(format!("{}_{}.{}", action.package, action.to.as_deref().unwrap(), art.format));
        if index == 0 {
            std::fs::copy(&source, &cached).unwrap();
            std::fs::remove_file(&source).unwrap(); // Success now requires reusing the verified file.
        } else {
            std::fs::write(&cached, b"corrupt cache").unwrap(); // Must be replaced from the valid source.
        }
    }
    assert_eq!(e.apply(&group, &plan, false).unwrap(), Outcome::Applied);
    assert!(e.installed().unwrap().packages.contains_key("meduza-np-wireguard"));
    let _ = std::fs::remove_dir_all(&env.root);
}

#[test]
fn bootstrap_upgrade_rollback_and_attacks() {
    let env = Env::new("main");
    let cfg = env.config(Some(vec!["meduza-np-wireguard"]));
    let planner = FakePlanner { store: cfg.store_dir(), up: Cell::new(false), boot: Cell::new(1), calls: RefCell::new(vec![]) };
    let now_fn = now;
    let e = engine(&cfg, &planner, &now_fn);

    // v0.1.0: container bootstrap (planner not yet running); updater satisfied by the image baseline.
    let planner1 = env.package("meduza-np-planner", "0.1.0", false);
    let wg1 = env.package("meduza-np-wireguard", "0.1.0", false);
    let upd1 = env.package("meduza-np-updater", "0.1.0", false);
    env.publish("v0.1.0", 1, vec![planner1.clone(), wg1.clone(), upd1.clone()], FAR);
    let (plan, outcome) = e.run_once(true).unwrap();
    assert_eq!(outcome, Some(Outcome::Applied), "{plan:?}");
    let inst = e.installed().unwrap();
    assert_eq!(inst.packages.keys().cloned().collect::<Vec<_>>(), vec!["meduza-np-planner", "meduza-np-updater", "meduza-np-wireguard"]);
    assert_eq!(inst.packages["meduza-np-updater"].source, "image");
    assert_eq!(current_file(&cfg, "usr/libexec/meduza-np/test/meduza-np-planner").as_deref(), Some("meduza-np-planner 0.1.0"));

    // v0.2.0: planner changes, wireguard reused (same artifact). Planner is now running.
    planner.up.set(true);
    let planner2 = env.package("meduza-np-planner", "0.2.0", false);
    env.publish("v0.2.0", 2, vec![planner2.clone(), wg1.clone(), upd1.clone()], FAR);
    let (plan, outcome) = e.run_once(true).unwrap();
    let plan = plan.unwrap();
    assert_eq!(outcome, Some(Outcome::Applied));
    assert_eq!(plan.download_bytes, planner2.artifacts[0].size, "only the changed package is downloaded");
    let calls = planner.calls.borrow().clone();
    for m in ["maintenance.lock", "planner.restart", "packages", "maintenance.unlock"] {
        assert!(calls.contains(&m.to_string()), "{m} missing from {calls:?}");
    }
    assert!(!calls.contains(&"drain".to_string()), "a planner restart replaces per-instance drains");
    assert_eq!(current_file(&cfg, "usr/libexec/meduza-np/test/meduza-np-planner").as_deref(), Some("meduza-np-planner 0.2.0"));
    assert_eq!(e.installed().unwrap().generation, 2);
    planner.calls.borrow_mut().clear();

    // v0.3.0: a wireguard build planner refuses → rollback to v0.2.0, remembered as failed.
    let wg3 = env.package("meduza-np-wireguard", "0.3.0", true);
    env.publish("v0.3.0", 3, vec![planner2.clone(), wg3, upd1.clone()], FAR);
    let (_, outcome) = e.run_once(true).unwrap();
    assert!(matches!(outcome, Some(Outcome::RolledBack(ref why)) if why.contains("invalid")), "{outcome:?}");
    let calls = planner.calls.borrow().clone();
    assert!(calls.contains(&"drain".to_string()) && calls.contains(&"instance.restart".to_string()), "{calls:?}");
    assert_eq!(current_file(&cfg, "usr/libexec/meduza-np/test/meduza-np-wireguard").as_deref(), Some("meduza-np-wireguard 0.1.0"));
    let inst = e.installed().unwrap();
    assert_eq!(inst.packages["meduza-np-wireguard"].version, "0.1.0");
    assert_eq!(inst.group_version.as_deref(), Some("v0.2.0"));
    assert!(e.load_state().failed_groups.contains(&"v0.3.0".to_string()));
    assert!(e.load_journal().is_none(), "journal archived");
    let (_, outcome) = e.run_once(true).unwrap();
    assert_eq!(outcome, None, "a rolled-back group is not retried automatically");

    // Rollback attack: an older (validly signed) pointer is refused.
    let old = std::fs::read(env.repo.join("releases/v0.2.0/manifest.json")).unwrap();
    env.point("v0.2.0", 2, &meduza_np_protocol::digest::sha256_hex(&old), FAR);
    assert_eq!(e.check().unwrap_err().code, "rollback-detected");
    // Tampered manifest bytes behind a valid pointer.
    env.publish("v0.4.0", 4, vec![planner2.clone(), wg1.clone(), upd1.clone()], FAR);
    let rel = env.repo.join("releases/v0.4.0/manifest.json");
    let mut bytes = std::fs::read(&rel).unwrap();
    bytes.extend_from_slice(b"\n ");
    std::fs::write(&rel, bytes).unwrap();
    assert_eq!(e.check().unwrap_err().code, "digest-mismatch");
    // Expired metadata (freeze attack).
    env.publish("v0.4.0", 4, vec![planner2.clone(), wg1.clone(), upd1.clone()], "2026-01-01T00:00:00Z");
    assert_eq!(e.check().unwrap_err().code, "metadata-expired");
    // Signed by a key the node does not trust.
    env.publish("v0.4.0", 4, vec![planner2, wg1, upd1], FAR);
    let other = SigningKey::from_bytes(&[9; 32]);
    let bytes = std::fs::read(env.repo.join("channels/stable.json")).unwrap();
    std::fs::write(env.repo.join("channels/stable.json.sig"), serde_json::to_vec(&sig::sign(&bytes, &[other])).unwrap()).unwrap();
    assert_eq!(e.check().unwrap_err().code, "signature-invalid");
    let _ = std::fs::remove_dir_all(&env.root);
}

#[test]
fn interrupted_switch_is_recovered() {
    let env = Env::new("recover");
    let cfg = env.config(Some(vec!["meduza-np-wireguard"]));
    let planner = FakePlanner { store: cfg.store_dir(), up: Cell::new(true), boot: Cell::new(1), calls: RefCell::new(vec![]) };
    let now_fn = now;
    let e = engine(&cfg, &planner, &now_fn);
    let p1 = env.package("meduza-np-planner", "0.1.0", false);
    let w1 = env.package("meduza-np-wireguard", "0.1.0", false);
    let u1 = env.package("meduza-np-updater", "0.1.0", false);
    env.publish("v0.1.0", 1, vec![p1.clone(), w1, u1.clone()], FAR);
    assert_eq!(e.run_once(true).unwrap().1, Some(Outcome::Applied));

    // Apply v0.2.0 with a broken wireguard, then pretend the process died right after switching.
    env.publish("v0.2.0", 2, vec![p1, env.package("meduza-np-wireguard", "0.2.0", true), u1], FAR);
    let g = e.check().unwrap();
    let plan = e.plan(&g).unwrap();
    planner.up.set(false); // apply runs without a planner: no verification, switch only
    assert_eq!(e.apply(&g, &plan, false).unwrap(), Outcome::Applied);
    assert_eq!(current_file(&cfg, "usr/libexec/meduza-np/test/meduza-np-wireguard").as_deref(), Some("meduza-np-wireguard 0.2.0"));
    // Re-create the journal as a crash in phase "verifying" would have left it.
    let mut j: meduza_np_updater::engine::Journal =
        serde_json::from_slice(&std::fs::read(cfg.store_dir().join("journal.last.json")).unwrap()).unwrap();
    j.phase = Phase::Verifying;
    j.planner_was_up = true;
    std::fs::write(cfg.store_dir().join("journal.json"), serde_json::to_vec(&j).unwrap()).unwrap();
    // GC after the "successful" apply kept the previous view and its objects, so a rollback is still possible.
    planner.up.set(true);
    let msg = e.recover(false).unwrap();
    assert!(msg.contains("rolled back"), "{msg}");
    assert_eq!(current_file(&cfg, "usr/libexec/meduza-np/test/meduza-np-wireguard").as_deref(), Some("meduza-np-wireguard 0.1.0"));
    assert!(e.load_journal().is_none());
    let _ = std::fs::remove_dir_all(&env.root);
}

#[test]
fn configured_features_extend_the_selection() {
    use meduza_np_updater::resolve::Selection;
    let env = Env::new("wanted");
    let cfg = env.config(Some(vec!["meduza-np-wireguard"]));
    let planner = FakePlanner { store: cfg.store_dir(), up: Cell::new(false), boot: Cell::new(1), calls: RefCell::new(vec![]) };
    let now_fn = now;
    let e = engine(&cfg, &planner, &now_fn);
    let explicit = |s: Selection| match s {
        Selection::Explicit(l) => l,
        Selection::Installed => panic!("expected an explicit selection"),
    };
    // Nothing wanted yet: exactly the operator's selection (no forced apply because of the union).
    assert_eq!(explicit(e.selection()), vec!["meduza-np-wireguard"]);
    // The planner's configuration enables FRR and Tinc (and something unknown, ignored).
    let path = cfg.paths.wanted_components();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, br#"["frr", "wireguard", "tinc", "no-such-thing"]"#).unwrap();
    assert_eq!(e.wanted_components(), vec!["meduza-np-frr", "meduza-np-tinc", "meduza-np-wireguard"]);
    assert_eq!(explicit(e.selection()), vec!["meduza-np-wireguard", "meduza-np-frr", "meduza-np-tinc"]);
    // Without an operator selection: installed packages are kept, missing wanted ones added.
    let open = env.config(None);
    let e = engine(&open, &planner, &now_fn);
    assert_eq!(explicit(e.selection()), vec!["meduza-np-frr", "meduza-np-tinc", "meduza-np-wireguard"]);
    let _ = std::fs::remove_dir_all(&env.root);
}

#[test]
fn configured_features_install_without_a_new_group() {
    let env = Env::new("wanted-install");
    let cfg = env.config(Some(vec![]));
    let planner = FakePlanner { store: cfg.store_dir(), up: Cell::new(false), boot: Cell::new(1), calls: RefCell::new(vec![]) };
    let now_fn = now;
    let e = engine(&cfg, &planner, &now_fn);
    let names = ["meduza-np-planner", "meduza-np-updater", "meduza-np-mihomo", "meduza-np-mihomo-pkg", "meduza-np-frr", "meduza-np-tinc"];
    env.publish("v0.1.0", 1, names.iter().map(|n| env.package(n, "0.1.0", false)).collect(), FAR);
    assert_eq!(e.run_once(true).unwrap().1, Some(Outcome::Applied));
    assert!(!e.installed().unwrap().packages.contains_key("meduza-np-mihomo"));
    let startup_packages = planner.packages();

    let wanted = cfg.paths.wanted_components();
    std::fs::create_dir_all(wanted.parent().unwrap()).unwrap();
    std::fs::write(&wanted, br#"["frr","mihomo","mihomo-pkg","tinc"]"#).unwrap();
    let (plan, outcome) = e.run_once(true).unwrap();
    assert_eq!(outcome, Some(Outcome::Applied), "{plan:?}");
    let plan = plan.unwrap();
    for name in &names[2..] {
        assert_eq!(plan.action(name).unwrap().kind, meduza_np_updater::resolve::ActionKind::Install);
        assert!(e.installed().unwrap().packages.contains_key(*name));
    }
    assert_eq!(e.installed().unwrap().group_version.as_deref(), Some("v0.1.0"));
    let progress = meduza_np_runtime::update::snapshot(&cfg.store_dir(), &cfg.paths.run).unwrap();
    assert_eq!(progress["phase"], "done");
    assert_eq!(progress["downloadedBytes"], progress["bytesTotal"]);
    assert!(progress["bytesTotal"].as_u64().unwrap() > 0);
    assert!(progress["pendingPackages"].as_array().unwrap().is_empty());
    assert_eq!(e.run_once(true).unwrap().1, Some(Outcome::NoChange), "NoChange only after the requested packages are installed");

    // Planner can scan the old view before its socket starts accepting requests. An install during this gap
    // sees planner as down and skips rescan; later NoChange passes must reconcile the cached registry.
    struct CachedPlanner<'a> {
        inner: &'a FakePlanner,
        packages: RefCell<Value>,
        refresh: Cell<bool>,
    }
    impl PlannerApi for CachedPlanner<'_> {
        fn call(&self, method: &str, params: Value) -> Result<Value, String> {
            if method == "packages" {
                return Ok(self.packages.borrow().clone());
            }
            let result = self.inner.call(method, params)?;
            if method == "registry.rescan" && self.refresh.get() {
                *self.packages.borrow_mut() = self.inner.packages();
            }
            Ok(result)
        }
    }
    planner.up.set(true);
    let cached = CachedPlanner { inner: &planner, packages: RefCell::new(startup_packages.clone()), refresh: Cell::new(true) };
    let e = engine(&cfg, &cached, &now_fn);
    planner.calls.borrow_mut().clear();
    assert_eq!(e.run_once(false).unwrap().1, Some(Outcome::NoChange));
    assert!(!planner.calls.borrow().iter().any(|m| m == "registry.rescan"), "a check-only pass must not re-apply services");
    assert_eq!(e.run_once(true).unwrap().1, Some(Outcome::NoChange));
    assert!(planner.calls.borrow().iter().any(|m| m == "registry.rescan"), "NoChange must repair stale planner registration");
    meduza_np_updater::planner::verify_packages(&cached, &[("meduza-np-mihomo".into(), "0.1.0".into())]).unwrap();
    planner.calls.borrow_mut().clear();
    assert_eq!(e.run_once(true).unwrap().1, Some(Outcome::NoChange));
    assert!(!planner.calls.borrow().iter().any(|m| m == "registry.rescan"), "an up-to-date registry needs no rescan");

    *cached.packages.borrow_mut() = startup_packages;
    cached.refresh.set(false);
    assert!(
        e.run_once(true).unwrap_err().contains("meduza-np-mihomo not found by planner"),
        "an unsuccessful rescan must not report success"
    );
    std::fs::remove_dir_all(&env.root).unwrap();
}
