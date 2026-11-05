//! Update orchestration (§13.3):
//!
//! ```text
//! check signed metadata → resolve subset → preflight space → download + verify into staging (no lock held)
//! → stage objects/view → maintenance.lock → drain affected → switch (store) / install (opkg, apk)
//! → restart via the actual owner (planner.restart / instance.restart) → verify planner + packages
//! → record success, or roll back to the previous generation → unlock → GC (keep one previous generation)
//! ```
//!
//! Every phase is written to `journal.json` before it starts, so an interrupted run is completed or rolled back
//! by [`Engine::recover`] on the next start. The updater never replaces itself in place: its own new version is
//! activated first by planner's short-lived helper and taken over by its service manager after exit 75. The new
//! updater resumes the staged transaction immediately, then upgrades planner and the remaining packages.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use meduza_np_protocol::group::{InstalledPackage, InstalledSet, TrustStore};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{Backend, Config};
use crate::meta::{self, MetaError, Source, VerifiedGroup};
use crate::openwrt::{self, Runner};
use crate::planner::{self, PlannerApi};
use crate::resolve::{self, ActionKind, Plan, Selection};
use crate::store::Store;

pub const EXIT_SELF_RESTART: i32 = 75;

/// Alpine packages the base container installs for components when planner starts (base-container image).
const IMAGE_DEPS: &str = "/usr/lib/meduza-np/container/apk-deps";

/// Packages the image installs system software for (`<package> <apk names...>` lines), from `path`. Empty outside
/// the base container.
pub fn image_dependencies(path: &std::path::Path) -> BTreeSet<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("skip-if"))
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .collect()
}

/// planner must restart for this change: it is upgraded itself, or a newly added package needs system software that
/// the base container installs only when planner starts (otherwise e.g. tinc would wait for a manual restart).
fn needs_planner_restart(changed: &[String], previous: &InstalledSet, image_deps: &BTreeSet<String>) -> bool {
    changed.iter().any(|p| p == "meduza-np-planner" || (!previous.packages.contains_key(p) && image_deps.contains(p)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Downloading,
    Staged,
    Locked,
    Switched,
    Verifying,
    SelfRestart,
    Done,
    RolledBack,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Journal {
    pub format_version: u32,
    pub group_version: String,
    pub phase: Phase,
    pub started_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub previous_view: Option<String>,
    #[serde(default)]
    pub new_view: Option<String>,
    #[serde(default)]
    pub previous_installed: Option<InstalledSet>,
    #[serde(default)]
    pub new_installed: Option<InstalledSet>,
    /// Intermediate generation: only updater changes. Absent in journals written by older releases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<crate::handoff::Handoff>,
    /// Packages whose bits change (install/upgrade/downgrade/reinstall/remove).
    #[serde(default)]
    pub changed: Vec<String>,
    #[serde(default)]
    pub planner_restart: bool,
    #[serde(default)]
    pub planner_was_up: bool,
    #[serde(default)]
    pub planner_boot_id: Option<String>,
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub error: Option<String>,
    /// Local package installation could not be rolled back; retain both sides and native repair downloads.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub repair_required: bool,
    #[serde(default)]
    pub history: Vec<(Phase, String)>,
}

/// Survives failed attempts overwriting journal.last.json; keep only the last accepted rollback baseline.
#[derive(Debug, Serialize, Deserialize)]
struct RollbackPoint {
    generation: u64,
    packages: BTreeMap<String, InstalledPackage>,
    installed: InstalledSet,
    view: Option<String>,
}

fn same_package_files(a: &BTreeMap<String, InstalledPackage>, b: &BTreeMap<String, InstalledPackage>) -> bool {
    a.len() == b.len() && a.iter().all(|(name, p)| b.get(name).is_some_and(|other| p.version == other.version && p.object == other.object))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdaterState {
    /// Highest accepted channel sequence per channel (rollback protection).
    pub last_sequence: BTreeMap<String, u64>,
    /// Groups whose apply failed and was rolled back; automatic runs skip them until a newer group appears.
    pub failed_groups: Vec<String>,
    pub last_check: Option<String>,
    pub last_check_error: Option<String>,
    pub available: Option<Available>,
    pub last_result: Option<RunResult>,
    pub next_check_unix: Option<u64>,
}

/// Operator controls (`control.json`), written only by the socket handler.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Control {
    pub paused: bool,
    pub defer_until: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Available {
    pub group_version: String,
    pub sequence: u64,
    pub expires_at: String,
    pub signed_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResult {
    pub group_version: String,
    pub outcome: String,
    pub at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    NoChange,
    Applied,
    /// Applied, and this updater must exit with [`EXIT_SELF_RESTART`] so its owner starts the new version.
    AppliedRestartSelf,
    RolledBack(String),
}

pub struct Engine<'a> {
    pub cfg: &'a Config,
    pub planner: &'a dyn PlannerApi,
    pub runner: &'a dyn Runner,
    pub now: &'a dyn Fn() -> u64,
    /// Planner restart/verification timeouts (shortened in tests).
    pub planner_timeout: Duration,
    pub poll: Duration,
}

fn now_text(now: u64) -> String {
    meduza_np_plugin_sdk::unix_to_rfc3339(now)
}

impl<'a> Engine<'a> {
    pub fn store(&self) -> Store {
        Store::new(self.cfg.store_dir())
    }

    pub fn load_state(&self) -> UpdaterState {
        meduza_np_runtime::fsutil::read_json(&self.store().state_path(), 256 * 1024).ok().flatten().unwrap_or_default()
    }

    pub fn save_state(&self, s: &UpdaterState) {
        if let Err(e) = meduza_np_runtime::fsutil::write_json_atomic(&self.store().state_path(), s, 0o644) {
            eprintln!("warning: cannot save updater state: {e}");
        }
    }

    pub fn load_control(&self) -> Control {
        meduza_np_runtime::fsutil::read_json(&self.store().root.join("control.json"), 64 * 1024).ok().flatten().unwrap_or_default()
    }

    pub fn load_journal(&self) -> Option<Journal> {
        meduza_np_runtime::fsutil::read_json(&self.store().journal_path(), 4 * 1024 * 1024).ok().flatten()
    }

    fn save_journal(&self, j: &mut Journal, phase: Phase) -> Result<(), String> {
        let now = now_text((self.now)());
        j.phase = phase;
        j.updated_at = now.clone();
        j.history.push((phase, now));
        if j.history.len() > 32 {
            j.history.remove(0);
        }
        meduza_np_runtime::fsutil::write_json_atomic(&self.store().journal_path(), j, 0o600).map_err(|e| format!("journal: {e}"))
    }

    fn finish_journal(&self, j: &mut Journal, phase: Phase, detail: Option<String>) {
        let _ = self.save_journal(j, phase);
        let mut st = self.load_state();
        st.last_result = Some(RunResult {
            group_version: j.group_version.clone(),
            outcome: format!("{phase:?}").to_lowercase(),
            at: now_text((self.now)()),
            detail,
        });
        if phase == Phase::RolledBack && !st.failed_groups.contains(&j.group_version) {
            st.failed_groups.push(j.group_version.clone());
            if st.failed_groups.len() > 16 {
                st.failed_groups.remove(0);
            }
        }
        self.save_state(&st);
        if phase == Phase::Done {
            if let (Some(current), Some(previous)) = (&j.new_installed, &j.previous_installed) {
                let point = RollbackPoint {
                    generation: current.generation,
                    packages: current.packages.clone(),
                    installed: previous.clone(),
                    view: j.previous_view.clone(),
                };
                if let Err(e) = meduza_np_runtime::fsutil::write_json_atomic(&self.store().root.join("rollback.json"), &point, 0o600) {
                    eprintln!("warning: cannot save rollback retention: {e}");
                }
            }
        }
        let _ = std::fs::rename(self.store().journal_path(), self.store().root.join("journal.last.json"));
        self.cleanup_idle();
    }

    pub fn trust(&self) -> Result<TrustStore, String> {
        for f in &self.cfg.trust_files {
            if let Some(t) =
                meduza_np_runtime::fsutil::read_json::<TrustStore>(f, 256 * 1024).map_err(|e| format!("{}: {e}", f.display()))?
            {
                if t.keys.is_empty() {
                    return Err(format!("{}: no trusted release keys", f.display()));
                }
                return Ok(t);
            }
        }
        Err("no release trust store installed (trust/release-keys.json)".into())
    }

    /// Fetches and verifies metadata; advances the accepted sequence only after full verification.
    pub fn check(&self) -> Result<VerifiedGroup, MetaError> {
        let store = self.store();
        let _operation = store.operation_lock().map_err(|e| MetaError { code: "fetch-failed", message: e })?;
        self.cleanup_idle();
        let mut st = self.load_state();
        let now = (self.now)();
        st.last_check = Some(now_text(now));
        let result = (|| {
            let trust = self.trust().map_err(|e| MetaError { code: "signature-invalid", message: e })?;
            let source = Source::new(&self.cfg.base_url)?;
            meta::check(&meta::CheckInput {
                source: &source,
                trust: &trust,
                channel: &self.cfg.channel,
                pinned: self.cfg.group_version.as_deref(),
                last_sequence: st.last_sequence.get(&self.cfg.channel).copied(),
                now_unix: now,
                tmp_dir: &store.staging(),
            })
        })();
        match &result {
            Ok(g) => {
                let installed = self.installed().ok();
                let keep = g
                    .manifest
                    .packages
                    .iter()
                    .flat_map(|p| p.artifacts.iter())
                    .filter(|a| {
                        !a.chunks.is_empty()
                            && (a.platform == self.cfg.platform || a.platform == "any")
                            && !installed.as_ref().is_some_and(|s| s.packages.values().any(|p| p.sha256.eq_ignore_ascii_case(&a.sha256)))
                    })
                    .map(|a| a.sha256.to_ascii_lowercase())
                    .collect();
                if let Err(e) = crate::chunks::retain(&store.root.join("downloads"), &keep) {
                    eprintln!("download cache cleanup: {e}");
                }
                if let Some(p) = &g.pointer {
                    st.last_sequence.insert(self.cfg.channel.clone(), p.sequence);
                }
                st.available = Some(Available {
                    group_version: g.manifest.group_version.clone(),
                    sequence: g.manifest.sequence,
                    expires_at: g.manifest.expires_at.clone(),
                    signed_by: g.signed_by.clone(),
                });
                st.last_check_error = None;
            }
            Err(e) => st.last_check_error = Some(e.to_string()),
        }
        self.save_state(&st);
        result
    }

    /// The installed record, reconciled with the package manager on OpenWrt.
    pub fn installed(&self) -> Result<InstalledSet, String> {
        let store = self.store();
        let mut s = store.load_installed()?.unwrap_or_else(|| InstalledSet {
            format_version: 1,
            backend: self.cfg.backend.name().into(),
            platform: self.cfg.platform.clone(),
            activated_at: String::new(),
            ..Default::default()
        });
        if self.cfg.backend != Backend::Store {
            let real = openwrt::installed(self.runner, self.cfg.backend)?;
            s.packages.retain(|name, _| real.contains_key(name));
            for (name, version) in real {
                let keep = s.packages.get(&name).map(|p| p.version == version).unwrap_or(false);
                if !keep {
                    s.packages.insert(name, InstalledPackage { version, sha256: String::new(), source: "system".into(), object: None });
                }
            }
        }
        Ok(s)
    }

    /// Components the planner's configuration needs (D20): features configured in the manager are installed here
    /// without editing `MEDUZA_NP_COMPONENTS`. Unknown names are ignored.
    pub fn wanted_components(&self) -> Vec<String> {
        let raw = std::fs::read(self.cfg.paths.wanted_components()).unwrap_or_default();
        let names: Vec<String> = serde_json::from_slice(&raw).unwrap_or_default();
        let mut out: Vec<String> = names.iter().filter_map(|n| crate::config::parse_components(n).ok()).flatten().collect();
        out.sort();
        out.dedup();
        out
    }

    /// The operator's selection plus what the configuration needs. Without new wanted components the selection is
    /// exactly the operator's, so nothing is re-applied outside the update window because of this union.
    pub fn selection(&self) -> Selection {
        let wanted = self.wanted_components();
        match &self.cfg.components {
            Some(list) => {
                let mut all = list.clone();
                all.extend(wanted.into_iter().filter(|w| !list.contains(w)));
                Selection::Explicit(all)
            }
            None => {
                // full package names like the parsed selection; planner and updater are always fixed
                let installed: Vec<String> = self
                    .installed()
                    .map(|s| {
                        s.packages.keys().filter(|p| !matches!(p.as_str(), "meduza-np-planner" | "meduza-np-updater")).cloned().collect()
                    })
                    .unwrap_or_default();
                if wanted.iter().all(|w| installed.contains(w)) {
                    Selection::Installed
                } else {
                    let mut all = installed;
                    all.extend(wanted.into_iter().filter(|w| !all.contains(w)).collect::<Vec<_>>());
                    Selection::Explicit(all)
                }
            }
        }
    }

    pub fn plan(&self, group: &VerifiedGroup) -> Result<Plan, String> {
        let installed = self.installed()?;
        let selection = self.selection();
        let mut plan = resolve::resolve(&resolve::ResolveInput {
            group: &group.manifest,
            installed: &installed,
            selection: &selection,
            platform: &self.cfg.platform,
            formats: self.cfg.backend.formats(),
            image_updater_version: self.cfg.image_updater_version.as_deref(),
        });
        if plan.changed() {
            let store = self.store();
            store.init()?;
            let free = meduza_np_runtime::fsutil::free_space(&store.root).unwrap_or(u64::MAX);
            // archive + extracted copy + headroom; never fill the volume (§13.3)
            let need = plan.download_bytes.saturating_mul(4) + 16 * 1024 * 1024;
            if free < need {
                plan.blocked.push(format!("insufficient space in {}: {free} bytes free, {need} needed", store.root.display()));
            }
            if self.cfg.backend != Backend::Store {
                let cache = openwrt::Cache::new(&store.root, self.cfg.backend);
                for a in plan.actions.iter().filter(|a| a.changes()) {
                    if let Some(from) = &a.from {
                        if cache.find(&a.package, from).is_none() {
                            plan.warnings.push(format!("{} {from}: no cached package, rollback unavailable", a.package));
                        }
                    }
                }
            }
        }
        Ok(plan)
    }

    fn download(&self, plan: &Plan, started_at: &str) -> Result<BTreeMap<String, PathBuf>, String> {
        let store = self.store();
        let source = Source::new(&self.cfg.base_url).map_err(|e| e.to_string())?;
        let mut files = BTreeMap::new();
        let downloads: Vec<_> = plan.actions.iter().filter(|a| a.downloads()).collect();
        let packages: Vec<_> = downloads.iter().map(|a| &a.package).collect();
        let total: u64 = downloads.iter().filter_map(|a| a.artifact.as_ref()).map(|a| a.size).sum();
        let mut completed_bytes = 0u64;
        for (index, a) in downloads.iter().enumerate() {
            let art = a.artifact.as_ref().ok_or_else(|| format!("{}: no artifact", a.package))?;
            if art.size > crate::store::MAX_ARTIFACT_BYTES {
                return Err(format!("{}: artifact larger than {} bytes", a.package, crate::store::MAX_ARTIFACT_BYTES));
            }
            let dest = store.staging().join(format!("{}_{}.{}", a.package, a.to.as_deref().unwrap_or("x"), art.format));
            let progress = |done: bool| {
                let value = json!({"startedAt":started_at, "packages":packages,
                    "currentPackage":if done { None } else { Some(&a.package) },
                    "file":if done { None } else { dest.file_name().and_then(|n| n.to_str()) },
                    "packageBytesTotal":if done { 0 } else { art.size }, "bytesTotal":total,
                    "completedBytes":completed_bytes + if done { art.size } else { 0 },
                    "completedPackages":index + usize::from(done), "totalPackages":downloads.len()});
                let _ = meduza_np_runtime::fsutil::write_json_atomic(&store.root.join("download-progress.json"), &value, 0o644);
            };
            progress(false);
            // A later automatic pass may reuse only a complete file matching the signed size AND digest.
            if crate::store::verify_file(&dest, art.size, &art.sha256).is_err() {
                let _ = std::fs::remove_file(&dest);
                eprintln!("downloading {} ({}/{}, {} bytes)", a.package, index + 1, downloads.len(), art.size);
                let url = source.resolve(&art.url).map_err(|e| e.to_string())?;
                let notify = |attempt: crate::retry::Attempt| {
                    progress(false);
                    let path = store.root.join("download-progress.json");
                    if let Ok(Some(mut value)) = meduza_np_runtime::fsutil::read_json::<Value>(&path, 256 * 1024) {
                        value["attempt"] = json!(attempt.number);
                        value["maxAttempts"] = json!(attempt.maximum);
                        value["retryError"] = json!(attempt.error);
                        value["retryAtUnix"] = attempt
                            .retry_after
                            .map(|d| {
                                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                json!(now + d.as_secs() + u64::from(d.subsec_nanos() > 0))
                            })
                            .unwrap_or(Value::Null);
                        let _ = meduza_np_runtime::fsutil::write_json_atomic(&path, &value, 0o644);
                    }
                };
                if self.cfg.backend == Backend::Store && !art.chunks.is_empty() {
                    crate::chunks::download(&source, art, &dest, &store.root.join("downloads"), notify)?;
                } else {
                    source.fetch_to_with_progress(&url, &dest, art.size, 900, notify).map_err(|e| e.to_string())?;
                }
            } else {
                eprintln!("reusing verified download: {}", a.package);
            }
            crate::store::verify_file(&dest, art.size, &art.sha256).map_err(|e| format!("{}: {e}", a.package))?;
            progress(true);
            completed_bytes += art.size;
            files.insert(a.package.clone(), dest);
        }
        Ok(files)
    }

    /// Applies a resolved plan. `force` ignores the failed-group memory (explicit operator request).
    pub fn apply(&self, group: &VerifiedGroup, plan: &Plan, force: bool) -> Result<Outcome, String> {
        let _operation = self.store().operation_lock()?;
        if plan.is_blocked() {
            return Err(format!("plan blocked: {}", plan.blocked.join("; ")));
        }
        if let Some(j) = self.load_journal() {
            return Err(format!("an update of {} is still recorded in phase {:?}; run recovery first", j.group_version, j.phase));
        }
        let st = self.load_state();
        if !force && st.failed_groups.contains(&plan.group_version) {
            return Err(format!("{} was rolled back before; apply it explicitly to retry", plan.group_version));
        }
        let store = self.store();
        store.init()?;
        let previous = self.installed()?;
        if !plan.changed() {
            self.reconcile_planner(plan)?;
            // Record the group as current (reused packages) without changing package files.
            if previous.group_version.as_deref() != Some(&plan.group_version) && !previous.packages.is_empty() {
                let mut s = previous.clone();
                s.group_version = Some(plan.group_version.clone());
                s.group_sequence = Some(plan.group_sequence);
                s.channel = Some(self.cfg.channel.clone());
                store.save_installed(&s)?;
            }
            return Ok(Outcome::NoChange);
        }
        let self_update = plan.action("meduza-np-updater").is_some_and(|a| a.changes() && !a.from_image);
        let now = now_text((self.now)());
        let changed: Vec<String> = plan.actions.iter().filter(|a| a.changes()).map(|a| a.package.clone()).collect();
        let mut j = Journal {
            format_version: 1,
            group_version: plan.group_version.clone(),
            phase: Phase::Downloading,
            started_at: now.clone(),
            updated_at: now,
            previous_view: store.current_view(),
            new_view: None,
            previous_installed: Some(previous.clone()),
            new_installed: None,
            handoff: None,
            changed: changed.clone(),
            planner_restart: needs_planner_restart(
                &changed,
                &previous,
                &image_dependencies(std::path::Path::new(&std::env::var("MEDUZA_NP_IMAGE_DEPS").unwrap_or_else(|_| IMAGE_DEPS.into()))),
            ),
            planner_was_up: false,
            planner_boot_id: None,
            locked: false,
            error: None,
            repair_required: false,
            history: vec![],
        };
        let _ = std::fs::remove_file(store.root.join("download-progress.json"));
        self.save_journal(&mut j, Phase::Downloading)?;

        // 1. download + verify, no lock held
        let files = match self.download(plan, &j.started_at) {
            Ok(f) => f,
            Err(e) => {
                j.error = Some(e.clone());
                self.finish_journal(&mut j, Phase::Failed, Some(e.clone()));
                return Err(e);
            }
        };

        // 2. stage
        let mut next = InstalledSet {
            format_version: 1,
            generation: previous.generation + if self_update { 2 } else { 1 },
            group_version: Some(group.manifest.group_version.clone()),
            group_sequence: Some(group.manifest.sequence),
            channel: if self.cfg.group_version.is_some() { None } else { Some(self.cfg.channel.clone()) },
            backend: self.cfg.backend.name().into(),
            platform: self.cfg.platform.clone(),
            packages: BTreeMap::new(),
            selected: match self.selection() {
                Selection::Explicit(l) => l,
                Selection::Installed => previous.selected.clone(),
            },
            activated_at: now_text((self.now)()),
        };
        let staged = (|| -> Result<(), String> {
            for a in plan.target() {
                let version = a.to.clone().unwrap_or_default();
                if a.from_image {
                    next.packages.insert(
                        a.package.clone(),
                        InstalledPackage { version, sha256: String::new(), source: "image".into(), object: None },
                    );
                    continue;
                }
                if a.kind == ActionKind::Keep {
                    let cur =
                        previous.packages.get(&a.package).cloned().ok_or_else(|| format!("{} missing from installed set", a.package))?;
                    if self.cfg.backend == Backend::Store {
                        let obj = cur.object.clone().unwrap_or_default();
                        if obj.is_empty() || !store.object_dir(&a.package, &obj).join("usr").is_dir() {
                            return Err(format!("{} {}: installed files are missing from the store", a.package, cur.version));
                        }
                    }
                    next.packages.insert(a.package.clone(), cur);
                    continue;
                }
                let art = a.artifact.as_ref().ok_or_else(|| format!("{}: no artifact", a.package))?;
                let progress_path = store.root.join("download-progress.json");
                if let Ok(Some(mut progress)) = meduza_np_runtime::fsutil::read_json::<Value>(&progress_path, 256 * 1024) {
                    progress["step"] = json!("installing");
                    progress["currentPackage"] = json!(a.package);
                    progress["file"] = Value::Null;
                    let _ = meduza_np_runtime::fsutil::write_json_atomic(&progress_path, &progress, 0o644);
                }
                let object = if self.cfg.backend == Backend::Store {
                    Some(store.install_object(&a.package, &version, art, &files[&a.package])?)
                } else {
                    None
                };
                next.packages.insert(
                    a.package.clone(),
                    InstalledPackage {
                        version,
                        sha256: art.sha256.to_ascii_lowercase(),
                        source: format!("group:{}", plan.group_version),
                        object,
                    },
                );
            }
            if self.cfg.backend == Backend::Store {
                let objects: BTreeMap<String, String> =
                    next.packages.iter().filter_map(|(n, p)| p.object.clone().map(|o| (n.clone(), o))).collect();
                j.new_view = Some(store.build_view(next.generation, &objects)?);
            }
            Ok(())
        })();
        if let Err(e) = staged {
            j.error = Some(e.clone());
            self.finish_journal(&mut j, Phase::Failed, Some(e.clone()));
            return Err(e);
        }
        j.new_installed = Some(next.clone());
        self.save_journal(&mut j, Phase::Staged)?;

        if self_update {
            let result = self.start_handoff(&mut j, plan);
            if let Err(e) = &result {
                if j.phase == Phase::Staged {
                    self.finish_journal(&mut j, Phase::Failed, Some(e.clone()));
                }
            }
            return result;
        }

        self.activate_packages(&mut j, plan, &files, next)
    }

    fn start_handoff(&self, j: &mut Journal, plan: &Plan) -> Result<Outcome, String> {
        let store = self.store();
        let mut installed = j.previous_installed.clone().ok_or("missing previous installed set")?;
        installed.generation += 1;
        installed.packages.insert(
            "meduza-np-updater".into(),
            j.new_installed.as_ref().and_then(|s| s.packages.get("meduza-np-updater")).cloned().ok_or("missing new updater")?,
        );
        let view = if self.cfg.backend == Backend::Store {
            let objects = installed.packages.iter().filter_map(|(n, p)| p.object.clone().map(|o| (n.clone(), o))).collect();
            Some(store.build_view(installed.generation, &objects)?)
        } else {
            None
        };
        j.handoff = Some(crate::handoff::Handoff { installed, view, plan: plan.clone() });
        // Persist BEFORE activation. Power loss on either side of the switch can be recovered by either updater.
        self.save_journal(j, Phase::SelfRestart)?;
        let result = if planner::boot_id(self.planner).is_some() {
            match self.planner.call("updater.activate", Value::Null) {
                Ok(v) if v["activated"] == true => Ok(()),
                Ok(_) => Err("planner did not confirm updater activation".into()),
                // Rolling compatibility: the last release's planner cannot implement this new method yet.
                Err(e) if e.starts_with("not-found:") => {
                    eprintln!("previous planner has no updater.activate; using the independent takeover helper");
                    self.activate_updater()
                }
                Err(e) => {
                    // Planner's store mount can be read-only. Repeat the same constrained activation in
                    // the writable owner; activate_updater revalidates the journal and updater-only scope.
                    eprintln!("planner updater activation failed ({e}); retrying the verified handoff in its writable owner");
                    self.activate_updater().map_err(|owner| format!("{e}; owner updater activation failed: {owner}"))
                }
            }
        } else {
            // Bootstrap / broken planner must never prevent updater from repairing the node.
            self.activate_updater()
        };
        if let Err(e) = result {
            j.error = Some(e.clone());
            return self.rollback(j, &[], e);
        }
        let h = j.handoff.as_ref().expect("handoff was staged");
        let recorded = match store.load_installed() {
            Ok(s) => s,
            Err(e) => return self.rollback(j, &[], e),
        };
        if recorded.as_ref() != Some(&h.installed) || (self.cfg.backend == Backend::Store && store.current_view() != h.view) {
            return self.rollback(j, &[], "planner did not activate the expected updater generation".into());
        }
        Ok(Outcome::AppliedRestartSelf)
    }

    /// Called only by planner's bounded activation helper (or bootstrap/old-planner compatibility path).
    pub fn activate_updater(&self) -> Result<(), String> {
        crate::handoff::activate(&self.store(), self.cfg.backend, self.runner, &self.cfg.paths.run)
    }

    fn activate_packages(
        &self,
        j: &mut Journal,
        plan: &Plan,
        files: &BTreeMap<String, PathBuf>,
        mut next: InstalledSet,
    ) -> Result<Outcome, String> {
        let store = self.store();
        let previous = j.previous_installed.clone().ok_or("missing previous installed set")?;

        // 3. maintenance lock + drain (only when planner runs)
        j.planner_boot_id = planner::boot_id(self.planner);
        j.planner_was_up = j.planner_boot_id.is_some();
        let affected = self.affected_instances(&j.changed);
        if j.planner_was_up {
            match self.planner.call("maintenance.lock", planner::lock_params(&format!("update to {}", plan.group_version))) {
                Ok(_) => j.locked = true,
                Err(e) => eprintln!("warning: maintenance lock failed: {e}"),
            }
            if !j.planner_restart && !affected.is_empty() {
                let _ = self.planner.call("drain", json!({"instances": affected}));
            }
        }
        self.save_journal(j, Phase::Locked)?;

        // 4. switch / install
        let switched = match self.cfg.backend {
            Backend::Store => store.switch(j.new_view.as_deref().unwrap_or_default()).map(|_| ()).and_then(|_| store.save_installed(&next)),
            _ => self.opkg_apply(plan, files, &previous, &mut next),
        };
        if let Err(e) = switched {
            j.error = Some(e.clone());
            return self.rollback(j, &affected, e);
        }
        self.save_journal(j, Phase::Switched)?;

        // 5. restart via owners + verify
        self.save_journal(j, Phase::Verifying)?;
        if let Err(e) = self.restart_and_verify(j, &next, &affected) {
            j.error = Some(e.clone());
            return self.rollback(j, &affected, e);
        }
        self.unlock(j);
        self.finish_journal(j, Phase::Done, None);
        Ok(Outcome::Applied)
    }

    fn affected_instances(&self, changed: &[String]) -> Vec<String> {
        let mut out: Vec<String> =
            changed.iter().filter(|p| *p != "meduza-np-planner").flat_map(|p| planner::instances_of(self.planner, p)).collect();
        out.sort();
        out.dedup();
        out
    }

    /// Installs changed packages with opkg/apk in plan order (updater was already taken over).
    fn opkg_apply(
        &self,
        plan: &Plan,
        files: &BTreeMap<String, PathBuf>,
        previous: &InstalledSet,
        next: &mut InstalledSet,
    ) -> Result<(), String> {
        let store = self.store();
        let cache = openwrt::Cache::new(&store.root, self.cfg.backend);
        meduza_np_runtime::fsutil::ensure_dir(&cache.dir, 0o755).map_err(|e| e.to_string())?;
        let _marker = openwrt::InstallMarker::hold(&self.cfg.paths.run);
        for a in plan.actions.iter().filter(|a| a.changes() && a.package != "meduza-np-updater") {
            let (prog, args) = match a.kind {
                ActionKind::Remove => openwrt::remove_args(self.cfg.backend, &a.package),
                _ => openwrt::install_args(self.cfg.backend, &files[&a.package]),
            };
            self.runner.run(prog, &args).map_err(|e| format!("{}: {e}", a.package))?;
            if let (Some(v), Some(f)) = (&a.to, files.get(&a.package)) {
                let _ = std::fs::copy(f, cache.file(&a.package, v));
            }
            // Record progress so a crash in the middle leaves an accurate installed set.
            let mut partial = previous.clone();
            for done in plan.actions.iter().take_while(|x| x.package != a.package).chain(std::iter::once(a)) {
                match (done.kind, next.packages.get(&done.package)) {
                    (ActionKind::Remove, _) => {
                        partial.packages.remove(&done.package);
                    }
                    (_, Some(p)) => {
                        partial.packages.insert(done.package.clone(), p.clone());
                    }
                    _ => {}
                }
            }
            store.save_installed(&partial)?;
        }
        if let Some(u) = previous.packages.get("meduza-np-updater") {
            // The updater already running must stay recorded throughout the remaining native installation.
            next.packages.insert("meduza-np-updater".into(), u.clone());
        }
        store.save_installed(next)?;
        Ok(())
    }

    fn reconcile_planner(&self, plan: &Plan) -> Result<(), String> {
        if self.cfg.backend != Backend::Store || planner::boot_id(self.planner).is_none() {
            return Ok(());
        }
        let expected: Vec<(String, String)> = plan
            .target()
            .filter(|a| a.package != "meduza-np-updater")
            .filter_map(|a| a.to.clone().map(|v| (a.package.clone(), v)))
            .collect();
        if planner::verify_packages(self.planner, &expected).is_ok() {
            return Ok(());
        }
        // Planner may have scanned the previous store view before opening its socket. An install in that
        // startup gap saw it as down and skipped rescan; unchanged package bits still need registration.
        eprintln!("installed packages are not reflected in planner; rescanning its registry");
        self.planner.call("registry.rescan", Value::Null)?;
        planner::verify_packages(self.planner, &expected).map_err(|e| format!("installed package verification failed after rescan: {e}"))
    }

    fn restart_and_verify(&self, j: &Journal, next: &InstalledSet, affected: &[String]) -> Result<(), String> {
        if !j.planner_was_up {
            // Bootstrap / planner down: its owner starts it on the new set; nothing to verify against.
            return Ok(());
        }
        if j.planner_restart {
            self.planner.call("planner.restart", json!({"reason": format!("update to {}", j.group_version)}))?;
            planner::wait_for_planner(self.planner, j.planner_boot_id.as_deref(), self.planner_timeout, self.poll)?;
        } else {
            self.planner.call("registry.rescan", Value::Null)?;
            for inst in affected {
                if let Err(e) = self.planner.call("instance.restart", json!({"instance": inst})) {
                    // an instance removed by the rescan (package removed) is not an error
                    if !e.starts_with("not-found") {
                        return Err(format!("restart {inst}: {e}"));
                    }
                }
            }
        }
        let expected: Vec<(String, String)> = j
            .changed
            .iter()
            .filter(|p| *p != "meduza-np-updater")
            .filter_map(|p| next.packages.get(p).map(|x| (p.clone(), x.version.clone())))
            .collect();
        planner::verify_packages(self.planner, &expected)
    }

    fn unlock(&self, j: &mut Journal) {
        if j.locked {
            if let Err(e) = self.planner.call("maintenance.unlock", Value::Null) {
                eprintln!("warning: maintenance unlock failed (expires by itself): {e}");
            }
            j.locked = false;
        }
    }

    fn rollback(&self, j: &mut Journal, affected: &[String], cause: String) -> Result<Outcome, String> {
        eprintln!("update to {} failed: {cause}; rolling back", j.group_version);
        let store = self.store();
        let prev = j.previous_installed.clone().unwrap_or_default();
        let result = match self.cfg.backend {
            Backend::Store => match &j.previous_view {
                Some(v) => store.switch(v).map(|_| ()).and_then(|_| store.save_installed(&prev)),
                None => {
                    // first install: nothing to go back to; leave the new set in place but report failure
                    Err("no previous generation to roll back to".into())
                }
            },
            _ => self.opkg_rollback(j, &prev),
        };
        if j.planner_was_up {
            let mut jj = j.clone();
            jj.changed = j.changed.clone();
            if j.planner_restart {
                jj.planner_boot_id = planner::boot_id(self.planner).or(jj.planner_boot_id);
            }
            if let Err(e) = self.restart_and_verify(&jj, &prev, affected) {
                eprintln!("warning: after rollback: {e}");
            }
        }
        self.unlock(j);
        match result {
            Ok(()) => {
                self.finish_journal(j, Phase::RolledBack, Some(cause.clone()));
                Ok(Outcome::RolledBack(cause))
            }
            Err(e) => {
                let msg = format!("{cause}; rollback failed: {e}");
                j.repair_required = true;
                self.finish_journal(j, Phase::Failed, Some(msg.clone()));
                Err(msg)
            }
        }
    }

    fn opkg_rollback(&self, j: &Journal, prev: &InstalledSet) -> Result<(), String> {
        let cache = openwrt::Cache::new(&self.store().root, self.cfg.backend);
        let _marker = openwrt::InstallMarker::hold(&self.cfg.paths.run);
        let mut problems = vec![];
        for name in j.changed.iter().rev().filter(|p| {
            // Before takeover no other package was touched; reinstalling planner here would violate that boundary.
            if j.handoff.is_some() {
                *p == "meduza-np-updater"
            } else {
                *p != "meduza-np-updater"
            }
        }) {
            match prev.packages.get(name) {
                Some(p) => match cache.find(name, &p.version) {
                    Some(f) => {
                        let (prog, args) = openwrt::install_args(self.cfg.backend, &f);
                        if let Err(e) = self.runner.run(prog, &args) {
                            problems.push(e);
                        }
                    }
                    None => problems.push(format!("{name} {}: no cached package to reinstall", p.version)),
                },
                None => {
                    let (prog, args) = openwrt::remove_args(self.cfg.backend, name);
                    let _ = self.runner.run(prog, &args);
                }
            }
        }
        self.store().save_installed(prev)?;
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    /// Called only while holding operation.lock. Pending recovery/handoff data is never collected.
    fn cleanup_idle(&self) {
        if let Err(error) = self.cleanup_idle_inner() {
            eprintln!("warning: obsolete package cleanup skipped: {error}");
        }
    }

    fn cleanup_idle_inner(&self) -> Result<(), String> {
        let store = self.store();
        if store.journal_path().exists() {
            return Ok(());
        }
        let current = self.installed()?;
        crate::chunks::discard_installed(
            &store.root.join("downloads"),
            &current.packages.values().map(|p| p.sha256.to_ascii_lowercase()).collect(),
        )?;
        if store.current_view().is_some() && !store.installed_path().is_file() {
            return Err("current view has no installed record".into());
        }
        let mut views: Vec<_> = store.current_view().into_iter().collect();
        let mut sets = vec![current.clone()];
        let point = meduza_np_runtime::fsutil::read_json::<RollbackPoint>(&store.root.join("rollback.json"), 4 * 1024 * 1024)
            .map_err(|e| e.to_string())?;
        if let Some(point) = point.filter(|p| p.generation == current.generation && same_package_files(&p.packages, &current.packages)) {
            views.extend(point.view);
            sets.push(point.installed);
        }
        let last = meduza_np_runtime::fsutil::read_json::<Journal>(&store.root.join("journal.last.json"), 4 * 1024 * 1024)
            .map_err(|e| e.to_string())?;
        let mut repair_pending = false;
        if let Some(last) = last {
            // Legacy journals have no rollback.json. A failed rollback can still need both sides of the transaction.
            let done = last.phase == Phase::Done
                && last
                    .new_installed
                    .as_ref()
                    .is_some_and(|n| n.generation == current.generation && same_package_files(&n.packages, &current.packages));
            let repair = last.phase == Phase::Failed
                && (last.repair_required
                    || !last.previous_installed.as_ref().is_some_and(|p| same_package_files(&p.packages, &current.packages))
                    || (self.cfg.backend == Backend::Store && store.current_view() != last.previous_view));
            repair_pending = repair;
            if done || repair {
                views.extend(last.previous_view);
                sets.extend(last.previous_installed);
            }
            if repair {
                views.extend(last.new_view);
                sets.extend(last.new_installed);
            }
        }
        if self.cfg.backend != Backend::Store {
            let mut keep: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for set in &sets {
                for (name, package) in &set.packages {
                    keep.entry(name.clone()).or_default().push(package.version.clone());
                }
            }
            openwrt::Cache::new(&store.root, self.cfg.backend).prune(&keep);
            if !repair_pending {
                store.clear_staging();
            }
            return Ok(());
        }
        let mut objects = BTreeSet::new();
        for s in &sets {
            for (n, p) in &s.packages {
                if let Some(o) = &p.object {
                    objects.insert((n.clone(), o.clone()));
                }
            }
        }
        retain_running_objects(&store, &self.cfg.paths.run, &mut objects)?;
        store.gc(&views, &objects);
        Ok(())
    }

    /// Completes or rolls back an update interrupted by a crash/power loss. Returns a description of what it did.
    pub fn recover(&self, running_new_updater: bool) -> Option<String> {
        let _operation = match self.store().operation_lock() {
            Ok(lock) => lock,
            Err(error) => {
                eprintln!("recovery deferred: {error}");
                return None;
            }
        };
        if self.cfg.backend != Backend::Store {
            openwrt::InstallMarker::clear_stale(&self.cfg.paths.run);
        }
        let Some(mut j) = self.load_journal() else {
            self.cleanup_idle();
            return None;
        };
        let affected = self.affected_instances(&j.changed);
        let msg = match j.phase {
            Phase::Downloading | Phase::Staged | Phase::Locked => {
                // Nothing was switched yet.
                self.unlock(&mut j);
                self.finish_journal(&mut j, Phase::Failed, Some("interrupted before switching; nothing changed".into()));
                format!("update to {} was interrupted before switching; previous set unchanged", j.group_version)
            }
            Phase::Switched | Phase::Verifying => {
                let next = j.new_installed.clone().unwrap_or_default();
                j.planner_boot_id = None; // whatever runs now is fine as long as it is healthy
                let verified = if planner::boot_id(self.planner).is_some() {
                    let mut jj = j.clone();
                    jj.planner_restart = false;
                    self.restart_and_verify(&jj, &next, &affected)
                } else {
                    Err("planner is not answering".into())
                };
                match verified {
                    Ok(()) => {
                        self.unlock(&mut j);
                        self.finish_journal(&mut j, Phase::Done, Some("completed after restart".into()));
                        format!("update to {} verified after restart", j.group_version)
                    }
                    Err(e) => match self.rollback(&mut j, &affected, format!("interrupted update not healthy: {e}")) {
                        Ok(_) => format!("update to {} rolled back after an interruption: {e}", j.group_version),
                        Err(e2) => e2,
                    },
                }
            }
            Phase::SelfRestart => {
                if running_new_updater && j.handoff.is_some() {
                    return Some(self.resume_after_handoff(&mut j));
                }
                if running_new_updater {
                    if self.cfg.backend != Backend::Store {
                        // opkg/apk: the self-install ran after the installed set was saved and after planner's rescan
                        if let Some(new) = j.new_installed.as_ref().and_then(|n| n.packages.get("meduza-np-updater")).cloned() {
                            let store = self.store();
                            if let Ok(Some(mut cur)) = store.load_installed() {
                                cur.packages.insert("meduza-np-updater".into(), new);
                                if let Err(e) = store.save_installed(&cur) {
                                    eprintln!("warning: recording the new updater: {e}");
                                }
                            }
                        }
                        if let Err(e) = self.planner.call("registry.rescan", Value::Null) {
                            eprintln!("warning: planner rescan after the updater self-update: {e}");
                        }
                    }
                    self.finish_journal(&mut j, Phase::Done, Some("updater self-update taken over".into()));
                    format!("update to {} complete (new updater running)", j.group_version)
                } else {
                    match self.rollback(&mut j, &affected, "the new updater did not start; using the previous one".into()) {
                        Ok(_) => format!("update to {} rolled back: new updater failed to start", j.group_version),
                        Err(e) => e,
                    }
                }
            }
            Phase::Done | Phase::RolledBack | Phase::Failed => {
                let phase = j.phase;
                self.finish_journal(&mut j, phase, None);
                return None;
            }
        };
        Some(msg)
    }

    fn resume_after_handoff(&self, j: &mut Journal) -> String {
        let h = j.handoff.take().expect("handoff checked by caller");
        // The verified new updater is now the recovery baseline. A failed planner upgrade must retain it.
        j.previous_installed = Some(h.installed);
        j.previous_view = h.view;
        j.changed.retain(|p| p != "meduza-np-updater");
        let result = (|| {
            self.save_journal(j, Phase::Staged)?;
            let next = j.new_installed.clone().ok_or("handoff has no staged target")?;
            let files =
                if self.cfg.backend == Backend::Store { BTreeMap::new() } else { crate::handoff::staged_files(&self.store(), &h.plan)? };
            self.activate_packages(j, &h.plan, &files, next)
        })();
        match result {
            Ok(Outcome::Applied) => format!("update to {} complete: new updater upgraded planner and packages", j.group_version),
            Ok(Outcome::RolledBack(e)) => format!("update to {} rolled back after updater takeover: {e}", j.group_version),
            Err(e) if self.load_journal().is_none() => e,
            Err(e) => match self.rollback(j, &self.affected_instances(&j.changed), e) {
                Ok(_) => format!("update to {} rolled back after updater takeover", j.group_version),
                Err(e) => e,
            },
            Ok(other) => format!("unexpected handoff outcome: {other:?}"),
        }
    }

    /// One scheduled pass: check, plan and (when allowed) apply.
    pub fn run_once(&self, allow_apply: bool) -> Result<(Option<Plan>, Option<Outcome>), String> {
        let group = self.check().map_err(|e| e.to_string())?;
        let plan = self.plan(&group)?;
        if !allow_apply || plan.is_blocked() || !plan.changed() {
            let outcome = if !plan.changed() && !plan.is_blocked() {
                // A paused/window-deferred check may inspect the plan, but must not rescan/re-apply services.
                Some(if allow_apply { self.apply(&group, &plan, false)? } else { Outcome::NoChange })
            } else {
                None
            };
            return Ok((Some(plan), outcome));
        }
        let st = self.load_state();
        if st.failed_groups.contains(&plan.group_version) {
            return Ok((Some(plan), None));
        }
        let outcome = self.apply(&group, &plan, false)?;
        Ok((Some(plan), Some(outcome)))
    }
}

/// Keep executables still in use (including adoption records from planner's separate PID namespace).
fn retain_running_objects(store: &Store, run: &std::path::Path, keep: &mut BTreeSet<(String, String)>) -> Result<(), String> {
    let root = std::fs::canonicalize(store.objects()).map_err(|e| e.to_string())?;
    let mut retain = |path: &std::path::Path| {
        if let Ok(relative) = path.strip_prefix(&root) {
            let parts: Vec<_> = relative.components().collect();
            if let [std::path::Component::Normal(package), std::path::Component::Normal(object), ..] = parts.as_slice() {
                keep.insert((package.to_string_lossy().into_owned(), object.to_string_lossy().into_owned()));
            }
        }
    };
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for (index, entry) in entries.enumerate() {
            if index > 16_384 {
                return Err("process scan limit reached".into());
            }
            let Ok(entry) = entry else { continue };
            if entry.file_name().to_str().is_some_and(|s| s.parse::<u32>().is_ok()) {
                if let Ok(exe) = std::fs::read_link(entry.path().join("exe")) {
                    retain(&exe);
                }
            }
        }
    }
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok();
    if let Ok(entries) = std::fs::read_dir(run.join("supervisor")) {
        for (index, entry) in entries.enumerate() {
            if index > 4096 {
                return Err("supervisor record limit reached".into());
            }
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.path().extension().is_some_and(|e| e == "json") {
                let value = meduza_np_runtime::fsutil::read_json::<Value>(&entry.path(), 64 * 1024).map_err(|e| e.to_string())?;
                if let Some(value) = value.filter(|v| boot.as_ref().is_some_and(|boot| v["bootId"].as_str() == Some(boot.trim()))) {
                    if let Some(path) = value["program"].as_str() {
                        retain(std::path::Path::new(path));
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod image_deps_tests {
    use super::*;

    #[test]
    fn added_packages_with_image_dependencies_restart_planner() {
        let dir = std::env::temp_dir().join(format!("mnp-deps-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("apk-deps");
        std::fs::write(&f, "# comment\nmeduza-np-tinc tinc\nmeduza-np-frr frr\nskip-if meduza-np-openvpn-pkg openvpn\n").unwrap();
        let deps = image_dependencies(&f);
        assert_eq!(deps.iter().map(String::as_str).collect::<Vec<_>>(), ["meduza-np-frr", "meduza-np-tinc"]);
        let mut prev: InstalledSet = serde_json::from_value(json!({
            "formatVersion": 1, "generation": 1, "backend": "store", "platform": "linux/amd64", "packages": {}, "activatedAt": "2026-10-24T00:00:00Z"
        }))
        .unwrap();
        let frr: InstalledPackage =
            serde_json::from_value(json!({"version": "1", "object": "o", "sha256": "0".repeat(64), "source": "group:v1"})).unwrap();
        prev.packages.insert("meduza-np-frr".into(), frr);
        let ch = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(needs_planner_restart(&ch(&["meduza-np-tinc"]), &prev, &deps), "new tinc needs its apk");
        assert!(!needs_planner_restart(&ch(&["meduza-np-frr"]), &prev, &deps), "an upgraded frr has its apk already");
        assert!(!needs_planner_restart(&ch(&["meduza-np-cli"]), &prev, &deps), "no system software");
        assert!(needs_planner_restart(&ch(&["meduza-np-planner"]), &prev, &deps));
        assert!(image_dependencies(&dir.join("missing")).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
