//! `meduza-np-planner` — run the node controller (`run`, default) or print the version.

use std::sync::Arc;
use std::time::Duration;

use meduza_np_planner::apply::Engine;
use meduza_np_planner::keystore::KeyStore;
use meduza_np_planner::netbackend::{LinuxBackend, NetBackend, RecordingBackend};
use meduza_np_planner::plugins::PluginRunner;
use meduza_np_planner::supervisor::Supervisor;
use meduza_np_planner::sync::Planner;
use meduza_np_planner::{info, warn};
use meduza_np_runtime::paths::NodePaths;

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| "run".into());
    match arg.as_str() {
        "--version" | "version" => {
            println!("meduza-np-planner {} (planner API {})", meduza_np_planner::VERSION, meduza_np_protocol::PLANNER_API_VERSION);
        }
        "run" => {
            meduza_np_planner::log::init_from_env();
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            rt.block_on(run());
        }
        "stop-orphans" => {
            // systemd ExecStopPost (KillMode=process): after a planned restart (exit 75, D32) the daemons stay for
            // the next planner to adopt; after any other exit nothing may keep running without its supervisor.
            let planned = std::env::var("EXIT_CODE").as_deref() == Ok("exited") && std::env::var("EXIT_STATUS").as_deref() == Ok("75");
            if !planned {
                meduza_np_planner::log::init_from_env();
                let paths = NodePaths::from_env();
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
                rt.block_on(Supervisor::persistent(&paths.run.join("supervisor")).reap_unclaimed());
            }
        }
        other => {
            eprintln!("unknown command {other}; usage: meduza-np-planner [run|--version|stop-orphans]");
            std::process::exit(2);
        }
    }
}

async fn run() {
    let paths = NodePaths::from_env();
    for dir in [&paths.state, &paths.run] {
        if let Err(e) = meduza_np_runtime::fsutil::ensure_dir(dir, 0o755) {
            eprintln!("cannot create {}: {e}", dir.display());
            std::process::exit(1);
        }
    }
    let platform = meduza_np_planner::platform::detect();
    let registry = meduza_np_planner::registry::scan(&paths.packages, &platform);
    info!(
        "meduza-np-planner {} (build {}) starting: {} package(s) registered, platform openwrt={} container={}",
        // the distributed package version (assigned at release) — the crate version only identifies the build
        registry.get("meduza-np-planner").map(|p| p.manifest.version.as_str()).unwrap_or("unpackaged"),
        meduza_np_planner::VERSION,
        registry.registered().count(),
        platform.openwrt,
        platform.container
    );
    let net: Arc<dyn NetBackend> = if std::env::var_os("MEDUZA_NP_DRY_RUN_NETWORK").is_some() || !cfg!(target_os = "linux") {
        warn!("network changes are recorded only (dry run)");
        Arc::new(RecordingBackend::default())
    } else {
        Arc::new(LinuxBackend::default())
    };
    let engine = Engine {
        paths: paths.clone(),
        runner: PluginRunner::new(2, Duration::from_secs(30)),
        // Records in RUN_DIR let a planned restart keep the daemons running (D32).
        supervisor: Supervisor::persistent(&paths.run.join("supervisor")),
        net,
        keystore: KeyStore::new(&paths.etc.join("keys")),
        settle: Duration::from_secs(2),
        settle_deadline: Duration::from_secs(15),
        loaded: Default::default(),
    };
    let planner = Planner::new(paths.clone(), platform, engine, registry);

    // Recovery first: run the last successful configuration before talking to the manager (§7.4, §11.1).
    let pending = meduza_np_runtime::journal::PendingApply::load(&paths.pending_journal()).ok().flatten();
    if let Some(p) = &pending {
        warn!("previous apply of {} was interrupted at {:?}; reconciling to the last good configuration", p.revision, p.stage);
    }
    let reg = planner.registry();
    // RUN_DIR can outlive the processes (a volume in the container deployment). After a power loss a pidfile there
    // may name an unrelated live process, and tincd/dnsmasq then refuse to start ("already running"). Only the
    // daemons a previous planner left running for adoption (D32) own valid pidfiles; every other one is stale.
    let live = planner.engine.supervisor.live_recorded_pids();
    for e in std::fs::read_dir(&paths.run).into_iter().flatten().flatten() {
        if e.path().extension().is_some_and(|x| x == "pid") {
            let pid = std::fs::read_to_string(e.path()).ok().and_then(|t| t.trim().parse::<u32>().ok());
            if !pid.is_some_and(|p| live.contains(&p)) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    // Packages removed while planner was not running (updater deselected a component, manual uninstall): their
    // instances are deactivated by ownership before anything else, so no orphaned interface/route/process stays
    // behind (§14.3.1 "移除组件先生成停用计划").
    let applied = meduza_np_planner::state::AppliedState::load(&paths).unwrap_or_default();
    let mut orphaned: Vec<String> =
        applied.instances.values().map(|a| a.package.clone()).filter(|p| !reg.get(p).map(|x| x.registered()).unwrap_or(false)).collect();
    orphaned.sort();
    orphaned.dedup();
    for pkg in &orphaned {
        warn!("{pkg} is no longer installed/registered; removing the resources and processes its instances owned");
        planner.engine.remove_package_instances(pkg).await;
    }
    planner.restore_last_good(&reg).await;
    // Daemons left running by the previous planner that the restored configuration did not claim.
    planner.engine.supervisor.reap_unclaimed().await;
    let _ = meduza_np_runtime::journal::PendingApply::clear(&paths.pending_journal());

    if let Err(e) = meduza_np_planner::connect_manager(&planner) {
        // Unattended first enrollment (containers, provisioning): one-time token from the environment.
        let url = std::env::var("MEDUZA_NP_ENROLL_URL").ok();
        let token = std::env::var("MEDUZA_NP_ENROLL_TOKEN_FILE")
            .ok()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .map(|t| t.trim().to_string())
            .or_else(|| std::env::var("MEDUZA_NP_ENROLL_TOKEN").ok());
        match (url, token) {
            (Some(url), Some(token)) => {
                let insecure = std::env::var_os("MEDUZA_NP_ENROLL_INSECURE").is_some();
                let ca = std::env::var("MEDUZA_NP_MANAGER_CA_FILE").ok();
                match meduza_np_planner::enroll(&planner, &url, &token, ca, insecure).await {
                    Ok(id) => info!("enrolled from environment as {id}"),
                    Err(e) => meduza_np_planner::error!("enrollment from environment failed: {e}"),
                }
            }
            _ => warn!("manager connection not configured: {e}; enroll with `meduza-np enroll`"),
        }
    }

    tokio::spawn(planner.clone().sync_loop());
    tokio::spawn(planner.clone().probe_loop());
    tokio::spawn(meduza_np_planner::netplan::HealthPorts::run(planner.clone()));
    #[cfg(unix)]
    {
        let p = planner.clone();
        tokio::spawn(async move {
            if let Err(e) = meduza_np_planner::local_api::server::run(p).await {
                meduza_np_planner::error!("local API stopped: {e}");
            }
        });
    }

    wait_for_shutdown().await;
    info!("shutting down; stopping supervised processes");
    planner.engine.supervisor.stop_all().await;
}

#[cfg(unix)]
async fn wait_for_shutdown() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("signal handler");
    let mut int = signal(SignalKind::interrupt()).expect("signal handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
