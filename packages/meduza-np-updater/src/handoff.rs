//! Short-lived helper called by planner: activate only the verified updater, leaving planner and services intact.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use meduza_np_protocol::group::InstalledSet;
use serde::{Deserialize, Serialize};

use crate::config::Backend;
use crate::engine::{Journal, Phase};
use crate::openwrt::{self, Runner};
use crate::resolve::Plan;
use crate::store::Store;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handoff {
    pub installed: InstalledSet,
    pub view: Option<String>,
    /// The original verified plan is resumed locally without checking a possibly newer channel pointer.
    pub plan: Plan,
}

pub fn staged_files(store: &Store, plan: &Plan) -> Result<BTreeMap<String, PathBuf>, String> {
    let mut files = BTreeMap::new();
    for a in plan.actions.iter().filter(|a| a.downloads()) {
        let art = a.artifact.as_ref().ok_or("missing staged artifact")?;
        let name = format!("{}_{}.{}", a.package, a.to.as_deref().unwrap_or("x"), art.format);
        if name.contains(['/', '\\']) {
            return Err("invalid staged artifact name".into());
        }
        let file = store.staging().join(name);
        crate::store::verify_file(&file, art.size, &art.sha256)?;
        files.insert(a.package.clone(), file);
    }
    Ok(files)
}

pub fn activate(store: &Store, backend: Backend, runner: &dyn Runner, run: &Path) -> Result<(), String> {
    let j: Journal = meduza_np_runtime::fsutil::read_json(&store.journal_path(), 4 * 1024 * 1024)
        .map_err(|e| e.to_string())?
        .ok_or("no pending updater handoff")?;
    if j.phase != Phase::SelfRestart {
        return Err("updater activation is only allowed during self-restart".into());
    }
    let h = j.handoff.as_ref().ok_or("no pending updater handoff")?;
    let previous = j.previous_installed.as_ref().ok_or("handoff has no previous installed set")?;
    let mut expected = previous.clone();
    expected.generation = expected.generation.checked_add(1).ok_or("generation overflow")?;
    let updater = h.installed.packages.get("meduza-np-updater").ok_or("handoff has no updater")?;
    if j.new_installed.as_ref().and_then(|s| s.packages.get("meduza-np-updater")) != Some(updater) {
        return Err("handoff updater differs from the staged target".into());
    }
    expected.packages.insert("meduza-np-updater".into(), updater.clone());
    if expected != h.installed || h.installed.backend != backend.name() {
        return Err("updater handoff must preserve every other package".into());
    }
    if j.native.is_some()
        && backend != Backend::Store
        && store.load_installed()?.as_ref() == Some(&h.installed)
        && openwrt::installed(runner, backend)?.get("meduza-np-updater") == Some(&updater.version)
    {
        // The previous helper may have installed and recorded the package before its acknowledgement was lost.
        return Ok(());
    }
    match backend {
        Backend::Store => {
            let view = h.view.as_deref().ok_or("missing updater handoff view")?;
            if view != format!("g{}", h.installed.generation) {
                return Err("invalid updater handoff view".into());
            }
            if store.current_view() != j.previous_view && store.current_view().as_deref() != Some(view) {
                return Err("the current store view changed during updater handoff".into());
            }
            store.switch(view)?;
        }
        _ => {
            let files = if j.native.is_none() {
                staged_files(store, &h.plan)?
            } else {
                let action = h.plan.action("meduza-np-updater").ok_or("missing updater action")?;
                let artifact = action.artifact.as_ref().ok_or("missing updater artifact")?;
                let file = openwrt::archive(store, "meduza-np-updater", &updater.version, &artifact.format)?;
                crate::store::verify_file(&file, artifact.size, &artifact.sha256)?;
                openwrt::check_install_space(store, artifact.size)?;
                [("meduza-np-updater".into(), file)].into()
            };
            let file = files.get("meduza-np-updater").ok_or("missing updater archive")?;
            let _marker = openwrt::InstallMarker::hold(run);
            let (prog, args) = openwrt::install_args(backend, file);
            runner.run(prog, &args)?;
            if j.native.is_none() {
                let cache = openwrt::Cache::new(&store.root, backend);
                std::fs::create_dir_all(&cache.dir).map_err(|e| e.to_string())?;
                std::fs::copy(file, cache.file("meduza-np-updater", &updater.version)).map_err(|e| e.to_string())?;
            }
        }
    }
    store.save_installed(&h.installed)?;
    if j.native.is_some() && backend != Backend::Store {
        let action = h.plan.action("meduza-np-updater").ok_or("missing updater action")?;
        let artifact = action.artifact.as_ref().ok_or("missing updater artifact")?;
        let _ = std::fs::remove_file(openwrt::archive(store, "meduza-np-updater", &updater.version, &artifact.format)?);
    }
    Ok(())
}
