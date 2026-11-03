//! `meduza-np-updater` — see `lib.rs`. Subcommands:
//!
//! ```text
//! run                     daemon: recover, then scheduled check/plan/apply; serves updater.sock
//! check | plan [--json]   one verified metadata check / resolved plan for this node
//! apply [--force]         apply now (ignores window/auto; --force retries a rolled-back group)
//! status                  installed set, journal, state
//! recover                 finish or roll back an interrupted update, then exit
//! system-links [ROOT]     link /usr/{bin,libexec/meduza-np,share/meduza-np} under ROOT to <store>/current
//! keygen KEYFILE [COMMENT]  new Ed25519 release key; prints its trust-store entry   (release tooling)
//! sign FILE KEYFILE...    writes FILE.sig                                          (release tooling)
//! verify FILE TRUSTFILE   checks FILE.sig against a trust store
//! trust-check TRUSTFILE   validates a trust store (image build gate)
//! installed               installed package names, one per line
//! base-image [--json]     base image the verified group pins for this platform (host helper base-image.sh)
//! store-check [--json]    can this (image) updater take over the store? read-only; exit 3 when it cannot
//! planner-health          exits 0 when planner answers on planner.sock (container health check)
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use meduza_np_updater::config::{Backend, Config};
use meduza_np_updater::engine::{Engine, Outcome, EXIT_SELF_RESTART};
use meduza_np_updater::image;
use meduza_np_updater::openwrt::SystemRunner;
use meduza_np_updater::planner::UdsPlanner;
use meduza_np_updater::sig;
use serde_json::json;

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("meduza-np-updater: {msg}");
    std::process::exit(1)
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn print(v: &serde_json::Value, json_out: bool) {
    if json_out {
        println!("{v}");
    } else {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    }
}

/// True when this process is the version the journal is waiting for after a self-update.
pub fn running_new_updater(cfg: &Config, engine: &Engine<'_>) -> bool {
    if std::env::var("MEDUZA_NP_UPDATER_SOURCE").as_deref() == Ok("image") {
        return false; // the entrypoint fell back to the image baseline
    }
    let Some(j) = engine.load_journal() else { return false };
    let Some(next) = j.new_installed.as_ref().and_then(|s| s.packages.get("meduza-np-updater")) else { return false };
    match cfg.backend {
        Backend::Store => {
            let exe = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok()).unwrap_or_default();
            let want = engine.store().object_dir("meduza-np-updater", next.object.as_deref().unwrap_or("-"));
            want.canonicalize().map(|w| exe.starts_with(w)).unwrap_or(false)
        }
        _ => meduza_np_updater::openwrt::installed(&SystemRunner, cfg.backend)
            .map(|m| m.get("meduza-np-updater") == Some(&next.version))
            .unwrap_or(false),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    let json_out = args.iter().any(|a| a == "--json");
    match cmd {
        "--version" | "version" => {
            println!("meduza-np-updater {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        "keygen" => return keygen(&args[1..]),
        "sign" => return sign_cmd(&args[1..]),
        "verify" => return verify_cmd(&args[1..]),
        "trust-check" => return trust_check(&args[1..]),
        "help" | "--help" | "-h" => {
            println!(
                "{}",
                include_str!("main.rs")
                    .lines()
                    .take_while(|l| l.starts_with("//!"))
                    .map(|l| l.trim_start_matches("//!").trim_start_matches(' '))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            return;
        }
        _ => {}
    }
    let cfg = Config::from_env().unwrap_or_else(|e| fail(e));
    let planner = UdsPlanner { socket: cfg.paths.planner_socket(), timeout: Duration::from_secs(30) };
    let runner = SystemRunner;
    let now_fn = now;
    let engine = Engine {
        cfg: &cfg,
        planner: &planner,
        runner: &runner,
        now: &now_fn,
        planner_timeout: Duration::from_secs(180),
        poll: Duration::from_secs(2),
    };
    match cmd {
        "activate-updater" => engine.activate_updater().unwrap_or_else(|e| fail(e)),
        "check" => match engine.check() {
            Ok(g) => print(
                &json!({"groupVersion": g.manifest.group_version, "sequence": g.manifest.sequence, "expiresAt": g.manifest.expires_at, "signedBy": g.signed_by}),
                json_out,
            ),
            Err(e) => fail(e),
        },
        "plan" => {
            let g = engine.check().unwrap_or_else(|e| fail(e));
            let plan = engine.plan(&g).unwrap_or_else(|e| fail(e));
            print(&serde_json::to_value(&plan).unwrap_or_default(), json_out);
        }
        "apply" => {
            if let Some(msg) = engine.recover(running_new_updater(&cfg, &engine)) {
                eprintln!("{msg}");
            }
            let g = engine.check().unwrap_or_else(|e| fail(e));
            let plan = engine.plan(&g).unwrap_or_else(|e| fail(e));
            match engine.apply(&g, &plan, args.iter().any(|a| a == "--force")) {
                Ok(Outcome::NoChange) => println!("{}: nothing to change", plan.group_version),
                Ok(Outcome::Applied) => println!("{}: applied", plan.group_version),
                Ok(Outcome::AppliedRestartSelf) => {
                    println!("{}: applied; updater restarts to take over", plan.group_version);
                    std::process::exit(EXIT_SELF_RESTART);
                }
                Ok(Outcome::RolledBack(why)) => fail(format!("{}: rolled back: {why}", plan.group_version)),
                Err(e) => fail(e),
            }
        }
        "status" => print(&status(&engine), json_out),
        "recover" => match engine.recover(running_new_updater(&cfg, &engine)) {
            Some(msg) => println!("{msg}"),
            None => println!("no interrupted update"),
        },
        "installed" => {
            for name in engine.installed().unwrap_or_else(|e| fail(e)).packages.keys() {
                println!("{name}");
            }
        }
        "base-image" => {
            let g = engine.check().unwrap_or_else(|e| fail(e));
            let t = image::base_image_target(&g.manifest, &cfg.platform).unwrap_or_else(|e| fail(e));
            if json_out {
                print(&serde_json::to_value(&t).unwrap_or_default(), true);
            } else {
                // key=value lines for the POSIX host helper (no JSON parser on the host)
                println!(
                    "group={}
version={}
reference={}
platform={}
platformDigest={}",
                    t.group_version, t.version, t.reference, t.platform, t.platform_digest
                );
                println!("updaterVersion={}", t.updater_version.as_deref().unwrap_or(""));
            }
        }
        "store-check" => {
            let c = image::store_check(&engine.store(), &cfg.platform, cfg.image_updater_version.as_deref());
            if json_out {
                print(&serde_json::to_value(&c).unwrap_or_default(), true);
            } else {
                println!("compatible={}", if c.compatible { "yes" } else { "no" });
                println!("updaterVersion={}", c.updater_version.as_deref().unwrap_or(""));
                println!("storeUpdaterVersion={}", c.store_updater_version.as_deref().unwrap_or(""));
                println!("group={}", c.group_version.as_deref().unwrap_or(""));
                println!("packages={}", c.packages.iter().map(|(k, v)| format!("{k}@{v}")).collect::<Vec<_>>().join(","));
                for p in &c.problems {
                    println!("problem={p}");
                }
                for w in &c.warnings {
                    println!("warning={w}");
                }
            }
            if !c.compatible {
                std::process::exit(3);
            }
        }
        "planner-health" => match meduza_np_updater::planner::boot_id(&planner) {
            Some(b) => println!("planner up (boot {b})"),
            None => fail("planner is not answering on planner.sock"),
        },
        "system-links" => system_links(&cfg, args.get(1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))),
        #[cfg(unix)]
        "run" => {
            let running_new = running_new_updater(&cfg, &engine);
            meduza_np_updater::daemon::run(&engine, running_new)
        }
        other => fail(format!("unknown command {other} (see `meduza-np-updater help`)")),
    }
}

pub fn status(engine: &Engine<'_>) -> serde_json::Value {
    let cfg = engine.cfg;
    json!({
        "backend": cfg.backend.name(),
        "platform": cfg.platform,
        "channel": cfg.channel,
        "pinnedGroupVersion": cfg.group_version,
        "source": cfg.base_url,
        "autoApply": cfg.auto_apply,
        "components": cfg.components,
        "installed": engine.installed().ok(),
        "journal": engine.load_journal(),
        "state": engine.load_state(),
        "control": engine.load_control(),
    })
}

fn system_links(cfg: &Config, root: PathBuf) {
    if cfg.backend != Backend::Store {
        fail("system links only apply to the store backend; OpenWrt packages install into the system directly");
    }
    let current = cfg.store_dir().join("current");
    let links = [
        ("usr/libexec/meduza-np", "usr/libexec/meduza-np"),
        ("usr/share/meduza-np/packages", "usr/share/meduza-np/packages"),
        ("usr/lib/meduza-np/providers", "usr/lib/meduza-np/providers"),
        ("usr/lib/meduza-np/updater-run", "usr/lib/meduza-np/updater-run"),
        // service units and init scripts shipped by planner/updater (docs/operations.md §2.2)
        ("usr/share/meduza-np/packaging", "usr/share/meduza-np/packaging"),
        ("usr/share/meduza-np/dashboard", "usr/share/meduza-np/dashboard"),
    ];
    for (at, target) in links {
        link(&root.join(at), &current.join(target));
    }
    // binaries only for installed packages; a stale link of ours for a removed package is dropped
    for bin in ["meduza-np-planner", "meduza-np", "meduza-np-dashboard"] {
        let at = root.join("usr/bin").join(bin);
        let target = current.join("usr/bin").join(bin);
        if target.exists() {
            link(&at, &target);
        } else if std::fs::symlink_metadata(&at).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            let _ = std::fs::remove_file(&at);
        }
    }
}

fn link(at: &Path, target: &Path) {
    if let Ok(meta) = std::fs::symlink_metadata(at) {
        if !meta.file_type().is_symlink() {
            fail(format!("{} exists and is not a symlink; refusing to replace it", at.display()));
        }
        let _ = std::fs::remove_file(at);
    }
    if let Some(p) = at.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, at).unwrap_or_else(|e| fail(format!("{}: {e}", at.display())));
        println!("{} -> {}", at.display(), target.display());
    }
    #[cfg(not(unix))]
    fail(format!("symlinks unsupported here ({} -> {})", at.display(), target.display()));
}

fn keygen(args: &[String]) {
    let Some(out) = args.first() else { fail("usage: keygen KEYFILE [COMMENT]") };
    if Path::new(out).exists() {
        fail(format!("{out} exists; refusing to overwrite a key"));
    }
    let k = sig::generate_key().unwrap_or_else(|e| fail(e));
    meduza_np_runtime::fsutil::write_atomic(Path::new(out), sig::encode_secret(&k).as_bytes(), 0o600).unwrap_or_else(|e| fail(e));
    let entry = sig::trusted_key(&k.verifying_key(), args.get(1).cloned());
    println!("{}", serde_json::to_string_pretty(&entry).unwrap_or_default());
}

fn sign_cmd(args: &[String]) {
    if args.len() < 2 {
        fail("usage: sign FILE KEYFILE...");
    }
    let bytes = std::fs::read(&args[0]).unwrap_or_else(|e| fail(format!("{}: {e}", args[0])));
    let keys: Vec<_> = args[1..]
        .iter()
        .map(|f| sig::decode_secret(&std::fs::read_to_string(f).unwrap_or_else(|e| fail(format!("{f}: {e}")))).unwrap_or_else(|e| fail(e)))
        .collect();
    let sf = sig::sign(&bytes, &keys);
    let out = format!("{}.sig", args[0]);
    let mut text = serde_json::to_vec_pretty(&sf).unwrap_or_default();
    text.push(b'\n');
    meduza_np_runtime::fsutil::write_atomic(Path::new(&out), &text, 0o644).unwrap_or_else(|e| fail(e));
    println!("{out}");
}

fn verify_cmd(args: &[String]) {
    if args.len() < 2 {
        fail("usage: verify FILE TRUSTFILE");
    }
    let bytes = std::fs::read(&args[0]).unwrap_or_else(|e| fail(format!("{}: {e}", args[0])));
    let sigf = std::fs::read(format!("{}.sig", args[0])).unwrap_or_else(|e| fail(format!("{}.sig: {e}", args[0])));
    let trust = meduza_np_runtime::fsutil::read_json(Path::new(&args[1]), 256 * 1024)
        .unwrap_or_else(|e| fail(e))
        .unwrap_or_else(|| fail(format!("{} not found", args[1])));
    match sig::verify(&bytes, &sigf, &trust, now()) {
        Ok(ids) => println!("ok: signed by {}", ids.join(", ")),
        Err(e) => fail(e),
    }
}

fn trust_check(args: &[String]) {
    let Some(f) = args.first() else { fail("usage: trust-check TRUSTFILE") };
    let t: meduza_np_protocol::group::TrustStore = meduza_np_runtime::fsutil::read_json(Path::new(f), 256 * 1024)
        .unwrap_or_else(|e| fail(format!("{f}: {e}")))
        .unwrap_or_else(|| fail(format!("{f} not found")));
    let usable: Vec<&str> = t
        .keys
        .iter()
        .filter(|k| !k.revoked && k.algorithm == "ed25519")
        .filter(|k| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(&k.public_key)
                .ok()
                .filter(|raw| raw.len() == 32 && meduza_np_protocol::group::key_id(raw) == k.key_id)
                .is_some()
        })
        .map(|k| k.key_id.as_str())
        .collect();
    if usable.len() < t.threshold.max(1) as usize {
        fail(format!("{f}: {} usable key(s), threshold {}", usable.len(), t.threshold));
    }
    println!("ok: {} usable release key(s): {}", usable.len(), usable.join(", "));
}
