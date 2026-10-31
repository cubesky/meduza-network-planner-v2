//! The planner core: desired-state handling, apply serialization, and the manager sync loops (§7, §11.1).
//!
//! - static nodes: one outbound WebSocket for notifications, HTTPS for snapshots, periodic calibration;
//!   degrade to HTTPS polling when the channel is unsupported, and keep trying to restore it under backoff.
//! - roaming nodes: periodic HTTPS polling merged with the status report; no long-lived subscription.
//! - every request goes through the shared gate; disconnects never stop running services.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use meduza_np_plugin_sdk::Platform;
use meduza_np_protocol::api::{Ack, AckStage, InstanceOutcome};
use meduza_np_protocol::config::{NodeBehavior, NodeConfig};
use meduza_np_protocol::digest::canonical_digest;
use meduza_np_protocol::events::{EventKind, StreamHello, StreamWelcome, SyncEvent};
use meduza_np_protocol::report::{InstanceStatus, MaintenanceStatus, SyncMode, SyncState, SyncTransport};
use meduza_np_protocol::revision::{RevisionId, RevisionPointers};
use meduza_np_runtime::backoff::{FailureClass, Outcome, RequestKind, Wait};
use meduza_np_runtime::jitter::{jittered_period, Jitter, OsJitter};
use meduza_np_runtime::paths::NodePaths;
use serde_json::Value;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;

use crate::apply::{service_enabled, Engine};
use crate::manager::{CallError, ConfigFetch, ManagerClient};
use crate::registry::{self, Registry, PLANNER_SERVICE};
use crate::state::{load_last_good, SyncPersisted};

/// Mutable runtime status shown in reports, CLI and dashboards.
#[derive(Debug, Clone)]
pub struct RuntimeStatus {
    pub pointers: RevisionPointers,
    pub applied_digest: Option<String>,
    pub state: SyncState,
    pub mode: SyncMode,
    pub transport: SyncTransport,
    pub behavior: NodeBehavior,
    pub degraded_reason: Option<String>,
    pub event_cursor: Option<String>,
    pub apply_stage: Option<String>,
    pub last_error: Option<String>,
    pub failed: Option<(String, String)>,
    pub maintenance: Option<MaintenanceStatus>,
    pub maintenance_until: Option<Instant>,
    pub probes: Vec<InstanceStatus>,
    pub warnings: Vec<String>,
}

impl Default for RuntimeStatus {
    fn default() -> Self {
        Self {
            pointers: RevisionPointers::default(),
            applied_digest: None,
            state: SyncState::Unsynced,
            mode: SyncMode::Poll,
            transport: SyncTransport::Https,
            behavior: NodeBehavior::Static,
            degraded_reason: None,
            event_cursor: None,
            apply_stage: None,
            last_error: None,
            failed: None,
            maintenance: None,
            maintenance_until: None,
            probes: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

pub struct Planner {
    pub paths: NodePaths,
    pub platform: Platform,
    pub engine: Engine,
    pub registry: Mutex<Arc<Registry>>,
    pub status: Mutex<RuntimeStatus>,
    pub client: Mutex<Option<Arc<ManagerClient>>>,
    pub boot_id: String,
    pub sequence: AtomicU64,
    /// Wakes the sync loop (sync now, enrollment, maintenance end).
    pub wake: Notify,
    apply_lock: tokio::sync::Mutex<()>,
    /// Latest target received while an apply was running (bounded queue of one, latest wins, §7.4).
    queued: Mutex<Option<(Value, String, String)>>,
    acks: Mutex<VecDeque<Ack>>,
    jitter: Mutex<OsJitter>,
    /// Access auth decisions handed to OpenVPN's connect hook (D38).
    pub access_sessions: Mutex<crate::access::Sessions>,
}

const MAX_QUEUED_ACKS: usize = 16;

impl Planner {
    pub fn new(paths: NodePaths, platform: Platform, engine: Engine, registry: Registry) -> Arc<Self> {
        let persisted = SyncPersisted::load(&paths.sync_state());
        let mut status = RuntimeStatus {
            pointers: persisted.pointers.clone(),
            // A failed revision is retried once by every new planner process: an upgrade (new planner or package
            // code) or a host fix applied while it was stopped may be exactly what makes it applicable. Within one
            // process the memory still prevents a retry loop.
            failed: None,
            event_cursor: persisted.event_cursor.clone(),
            ..Default::default()
        };
        if let Ok(Some(m)) = meduza_np_runtime::fsutil::read_json::<MaintenanceStatus>(&paths.maintenance(), 64 * 1024) {
            if m.active {
                status.maintenance = Some(m);
            }
        }
        Arc::new(Self {
            paths,
            platform,
            engine,
            registry: Mutex::new(Arc::new(registry)),
            status: Mutex::new(status),
            client: Mutex::new(None),
            boot_id: crate::platform::new_boot_id(),
            sequence: AtomicU64::new(0),
            wake: Notify::new(),
            apply_lock: tokio::sync::Mutex::new(()),
            queued: Mutex::new(None),
            acks: Mutex::new(VecDeque::new()),
            jitter: Mutex::new(OsJitter::new()),
            access_sessions: Mutex::new(Default::default()),
        })
    }

    pub fn registry(&self) -> Arc<Registry> {
        self.registry.lock().unwrap().clone()
    }

    pub fn client(&self) -> Option<Arc<ManagerClient>> {
        self.client.lock().unwrap().clone()
    }

    fn persist(&self) {
        let st = self.status.lock().unwrap().clone();
        let level = self.client().map(|c| c.gate.lock().unwrap().level()).unwrap_or(0);
        let p = SyncPersisted { pointers: st.pointers, event_cursor: st.event_cursor, backoff_level: level, failed: st.failed };
        if let Err(e) = p.save(&self.paths.sync_state()) {
            warn!("cannot persist sync state: {e}");
        }
    }

    fn jittered(&self, base: Duration) -> Duration {
        jittered_period(&mut *self.jitter.lock().unwrap(), base, 0.1)
    }

    /// Re-scans installed packages; packages that disappeared get their instances cleaned up (§3.4).
    pub async fn rescan(&self) {
        let new = registry::scan(&self.paths.packages, &self.platform);
        let old = self.registry();
        for p in old.registered() {
            if !new.get(&p.manifest.package).map(|n| n.registered()).unwrap_or(false) {
                info!("{} no longer registered; removing its instances", p.manifest.package);
                self.engine.remove_package_instances(&p.manifest.package).await;
            }
        }
        // Upgraded (or newly registered) packages: their plugins may now render different plans for the same
        // configuration, so the last good revision is applied again. Commit only reloads/restarts the plans whose
        // output actually changed; without this a plugin upgrade would take effect only at the next publish.
        let changed: Vec<String> = new
            .registered()
            .filter(|p| old.get(&p.manifest.package).map(|o| !o.registered() || o.manifest_digest != p.manifest_digest).unwrap_or(true))
            .map(|p| p.manifest.package.clone())
            .chain(
                // a provider switch (system ↔ -pkg, or a re-prepared download provider) migrates its instances (§3.1)
                new.providers
                    .iter()
                    .filter(|(slot, rp)| old.providers.get(*slot).map(|o| o.path != rp.path || o.source != rp.source).unwrap_or(true))
                    .map(|(slot, _)| format!("provider {slot}")),
            )
            .collect();
        let new = Arc::new(new);
        *self.registry.lock().unwrap() = new.clone();
        // A capability change may make a previously rejected revision applicable.
        self.status.lock().unwrap().failed = None;
        if !changed.is_empty() {
            info!("packages changed ({}); re-applying the last good configuration", changed.join(", "));
            let _guard = self.apply_lock.lock().await;
            self.restore_last_good(&new).await;
        }
        self.wake.notify_one();
    }

    // ---- applying -------------------------------------------------------------------------------------------

    fn queue_ack(&self, ack: Ack) {
        let mut q = self.acks.lock().unwrap();
        q.retain(|a| !(a.revision == ack.revision && a.stage == ack.stage));
        if q.len() >= MAX_QUEUED_ACKS {
            q.pop_front();
        }
        q.push_back(ack);
    }

    pub async fn flush_acks(&self) {
        let Some(client) = self.client() else { return };
        loop {
            let next = self.acks.lock().unwrap().front().cloned();
            let Some(ack) = next else { return };
            match client.post_ack(&ack).await {
                Ok(()) | Err(CallError::Rejected { .. }) => {
                    self.acks.lock().unwrap().pop_front();
                }
                Err(e) => {
                    debug!("ack {} {:?} deferred: {e}", ack.revision, ack.stage);
                    return;
                }
            }
        }
    }

    fn ack(&self, revision: &str, digest: &str, stage: AckStage, error: Option<(&str, &str)>, instances: Vec<InstanceOutcome>) {
        let Ok(rev) = RevisionId::parse(revision) else { return };
        self.queue_ack(Ack {
            revision: rev,
            config_digest: digest.into(),
            stage,
            boot_id: self.boot_id.clone(),
            error_code: error.map(|e| e.0.to_string()),
            message: error.map(|e| e.1.chars().take(1500).collect()),
            instances,
        });
    }

    /// Handles a desired configuration from the manager. Serialized; a newer target received meanwhile is
    /// applied right after, at a transaction boundary (never interrupting an apply).
    pub async fn offer(self: &Arc<Self>, raw: Value, digest: String, revision: String) {
        if self.apply_lock.try_lock().is_err() {
            *self.queued.lock().unwrap() = Some((raw, digest, revision));
            return;
        }
        let mut next = Some((raw, digest, revision));
        while let Some((raw, digest, revision)) = next.take() {
            self.apply_target(raw, digest, revision).await;
            next = self.queued.lock().unwrap().take();
        }
        self.flush_acks().await;
    }

    async fn apply_target(self: &Arc<Self>, raw: Value, digest: String, revision: String) {
        let _guard = self.apply_lock.lock().await;
        let actual = canonical_digest(&raw).unwrap_or_default();
        if actual != digest {
            warn!("{revision}: digest mismatch (got {actual}, expected {digest}); ignoring snapshot");
            self.status.lock().unwrap().last_error = Some("config digest mismatch".into());
            return;
        }
        {
            let mut st = self.status.lock().unwrap();
            if let Ok(r) = RevisionId::parse(&revision) {
                st.pointers.desired_revision = Some(r.clone());
                st.pointers.received_revision = Some(r);
            }
            if st.pointers.applied_revision.as_ref().map(|r| r.as_str()) == Some(revision.as_str())
                && st.applied_digest.as_deref() == Some(digest.as_str())
            {
                st.state = SyncState::InSync;
                return;
            }
            if st.failed.as_ref() == Some(&(revision.clone(), digest.clone())) {
                st.state = SyncState::ApplyFailed;
                return; // already failed; wait for a new revision or a package change
            }
            if let Some(m) = &st.maintenance {
                if m.apply_paused {
                    st.state = SyncState::Behind;
                    st.apply_stage = Some("paused-by-maintenance".into());
                    drop(st);
                    *self.queued.lock().unwrap() = Some((raw, digest, revision));
                    return;
                }
            }
            st.state = SyncState::Applying;
            st.apply_stage = Some("validating".into());
        }
        self.ack(&revision, &digest, AckStage::Received, None, vec![]);
        let registry = self.registry();
        // the desired state, even when this revision is rejected or fails below (D20, D46)
        self.engine.publish_wanted_components(&raw, false);
        let prepared = match self.engine.prepare(&raw, &digest, &registry, &self.platform).await {
            Ok(p) => p,
            Err(rej) => {
                let msg = rej.summary();
                warn!("{revision}: rejected: {msg} ({})", rej.code);
                self.ack(&revision, &digest, AckStage::Rejected, Some((&rej.code, &msg)), vec![]);
                let mut st = self.status.lock().unwrap();
                st.state = SyncState::ApplyFailed;
                st.apply_stage = None;
                st.last_error = Some(msg);
                st.failed = Some((revision, digest));
                drop(st);
                self.persist();
                return;
            }
        };
        {
            let mut st = self.status.lock().unwrap();
            st.pointers.validated_revision = RevisionId::parse(&revision).ok();
            st.apply_stage = Some("applying".into());
            st.warnings = prepared.warnings.clone();
        }
        self.ack(&revision, &digest, AckStage::Validated, None, vec![]);
        match self.engine.commit(&prepared, &registry).await {
            Ok(outcomes) => {
                info!("{revision}: applied ({} instance(s))", outcomes.len());
                let behavior = serde_json::from_value::<NodeConfig>(raw.clone()).map(|c| c.node.behavior).unwrap_or_default();
                {
                    let mut st = self.status.lock().unwrap();
                    let r = RevisionId::parse(&revision).ok();
                    st.pointers.applied_revision = r.clone();
                    st.pointers.observed_revision = r.clone();
                    st.pointers.last_successful_revision = r;
                    st.applied_digest = Some(digest.clone());
                    st.state = SyncState::InSync;
                    st.apply_stage = None;
                    st.last_error = None;
                    st.failed = None;
                    st.behavior = behavior;
                }
                self.ack(&revision, &digest, AckStage::Applied, None, outcomes);
                self.persist();
                self.wake.notify_one(); // behavior may have changed
            }
            Err(failure) => {
                error!("{revision}: apply failed: {} ({})", failure.message, failure.code);
                self.ack(&revision, &digest, AckStage::Failed, Some((&failure.code, &failure.message)), failure.instances);
                {
                    let mut st = self.status.lock().unwrap();
                    st.state = SyncState::ApplyFailed;
                    st.apply_stage = Some("restoring-last-good".into());
                    st.last_error = Some(failure.message.clone());
                    st.failed = Some((revision.clone(), digest.clone()));
                }
                self.persist();
                self.restore_last_good(&registry).await;
            }
        }
    }

    /// Re-applies the last successful configuration (startup recovery, failed apply compensation).
    /// A key generated on this node may complete a feature that waited for it (D28): the last good configuration is
    /// applied again, now with the key.
    pub async fn reapply_after_key(&self, secret_id: &str) {
        info!("key {secret_id} generated; re-applying the last good configuration");
        let _guard = self.apply_lock.lock().await;
        let registry = self.registry();
        self.restore_last_good(&registry).await;
        self.wake.notify_one();
    }

    pub async fn restore_last_good(&self, registry: &Registry) {
        match load_last_good(&self.paths) {
            Ok(Some(lg)) => {
                let revision = lg.config.revision.to_string();
                self.engine.publish_wanted_components(&lg.raw, true);
                match self.engine.prepare_last_good(&lg, registry, &self.platform).await {
                    Ok((prepared, held_back_rejected)) => match self.engine.commit(&prepared, registry).await {
                        Ok(_) => {
                            let mut st = self.status.lock().unwrap();
                            if let Some(r) = &held_back_rejected {
                                warn!(
                                    "restored last good configuration {revision} without {} (rejected now that it can be validated: {})",
                                    lg.held_back.join(", "),
                                    r.summary()
                                );
                            } else {
                                info!("restored last good configuration {revision}");
                            }
                            let r = RevisionId::parse(&revision).ok();
                            st.pointers.applied_revision = r.clone();
                            st.pointers.observed_revision = r.clone();
                            st.pointers.last_successful_revision = r;
                            st.applied_digest = Some(lg.digest.clone());
                            st.behavior = lg.config.node.behavior;
                            st.apply_stage = held_back_rejected.as_ref().map(|_| "degraded: held-back-rejected".to_string());
                            if let Some(r) = held_back_rejected {
                                st.last_error = Some(format!("{}: {}", lg.held_back.join(", "), r.summary()));
                            }
                        }
                        Err(f) => {
                            error!("restoring {revision} failed: {} — running with partial state, see diagnostics", f.message);
                            self.status.lock().unwrap().apply_stage = Some(format!("degraded: {}", f.code));
                        }
                    },
                    Err(r) => {
                        error!("last good configuration {revision} no longer valid here: {}", r.summary());
                        self.status.lock().unwrap().apply_stage = Some(format!("degraded: {}", r.code));
                    }
                }
            }
            Ok(None) => {}
            Err(e) => error!("cannot read last good configuration: {e}"),
        }
    }

    /// Instances currently known (for reports/features).
    pub fn running_packages(&self) -> (BTreeSet<String>, BTreeSet<String>) {
        let applied = crate::state::AppliedState::load(&self.paths).unwrap_or_default();
        let statuses = self.engine.supervisor.statuses();
        let mut running = BTreeSet::new();
        let mut healthy = BTreeSet::new();
        for (inst, a) in &applied.instances {
            let short = meduza_np_protocol::manifest::short_name(&a.package).to_string();
            let procs_ok =
                a.processes.keys().all(|p| statuses.iter().any(|s| &s.id == p && s.state == crate::supervisor::ProcState::Running));
            let probe_ok = self
                .status
                .lock()
                .unwrap()
                .probes
                .iter()
                .filter(|p| p.id.starts_with(inst.as_str()))
                .all(|p| p.state == meduza_np_protocol::report::ServiceState::Up);
            // An instance without processes (a kernel WireGuard interface, nft tables) runs once applied while
            // its probes see it up; with processes, running means every one of them runs.
            if procs_ok && (!a.processes.is_empty() || probe_ok) {
                running.insert(short.clone());
            }
            if procs_ok && probe_ok {
                healthy.insert(short);
            }
        }
        (running, healthy)
    }

    /// Configuration of every service the last good revision enables, by service key. Planner's own features
    /// (mappings, port forwards, health ports) are switched by the node's `networks`, entered under `planner`.
    pub fn enabled_services(&self) -> BTreeMap<String, Value> {
        let Some(lg) = load_last_good(&self.paths).ok().flatten() else {
            return BTreeMap::new();
        };
        let networks = serde_json::to_value(&lg.config.networks).unwrap_or_default();
        let mut out: BTreeMap<String, Value> = lg.config.services.into_iter().filter(|(_, v)| service_enabled(v)).collect();
        out.insert(PLANNER_SERVICE.into(), networks);
        out
    }

    pub fn next_sequence(&self) -> u64 {
        self.sequence.fetch_add(1, Ordering::Relaxed) + 1
    }

    // ---- manager sync ---------------------------------------------------------------------------------------

    async fn pull(self: &Arc<Self>, client: &ManagerClient) -> Result<(), CallError> {
        let etag = self.status.lock().unwrap().applied_digest.clone();
        match client.fetch_config(etag.as_deref(), None).await? {
            ConfigFetch::Config { raw, digest, revision } => {
                let me = self.clone();
                me.offer(raw, digest, revision).await;
            }
            ConfigFetch::NotModified => {
                let mut st = self.status.lock().unwrap();
                if st.state != SyncState::ApplyFailed {
                    st.state = SyncState::InSync;
                }
            }
            ConfigFetch::NoConfig => {
                let mut st = self.status.lock().unwrap();
                if st.pointers.applied_revision.is_none() {
                    st.state = SyncState::Unsynced;
                }
            }
        }
        self.flush_acks().await;
        Ok(())
    }

    async fn send_report(self: &Arc<Self>, client: &ManagerClient) -> Result<(), CallError> {
        let report = crate::reporter::build(self);
        let reply = client.post_report(&report).await?;
        // The reply carries the desired revision: a missed notification is detected here (§7.3).
        if let Some(desired) = reply.desired_revision {
            let applied = self.status.lock().unwrap().pointers.applied_revision.as_ref().map(|r| r.to_string());
            let failed = self.status.lock().unwrap().failed.clone();
            let behind = applied.as_deref() != Some(desired.as_str()) && failed.map(|f| f.0 != desired).unwrap_or(true);
            if behind {
                self.status.lock().unwrap().state = SyncState::Behind;
                self.pull(client).await?;
            }
        }
        Ok(())
    }

    fn set_link(&self, mode: SyncMode, transport: SyncTransport, degraded: Option<String>) {
        let mut st = self.status.lock().unwrap();
        st.mode = mode;
        st.transport = transport;
        st.degraded_reason = degraded;
    }

    fn mark_error(&self, e: &CallError) {
        let mut st = self.status.lock().unwrap();
        st.last_error = Some(e.to_string());
        st.state = match e {
            CallError::Credential(_) => SyncState::CredentialError,
            CallError::Unavailable(_) if st.state != SyncState::ApplyFailed => SyncState::Offline,
            _ => st.state,
        };
    }

    /// Runs forever. Exits only if the planner is shutting down.
    pub async fn sync_loop(self: Arc<Self>) {
        let mut first_round = true;
        let mut ws_unsupported_until: Option<Instant> = None;
        loop {
            let Some(client) = self.client() else {
                self.wake.notified().await;
                continue;
            };
            if first_round {
                // Cold start stagger, coalesced with any network notifications (§7.5.1).
                client.gate.lock().unwrap().note_reconnect_round(Instant::now());
                first_round = false;
            }
            if let Err(e) = self.pull(&client).await {
                self.mark_error(&e);
                if matches!(e, CallError::Credential(_)) {
                    warn!("manager rejected node credentials: {e}; waiting for re-enrollment");
                    let _ = tokio::time::timeout(Duration::from_secs(600), self.wake.notified()).await;
                }
                continue; // the gate decides when the next attempt may start
            }
            let behavior = self.status.lock().unwrap().behavior;
            let ws_allowed = ws_unsupported_until.map(|t| Instant::now() >= t).unwrap_or(true);
            if behavior == NodeBehavior::Roaming {
                self.set_link(SyncMode::Poll, SyncTransport::Https, None);
                self.roaming_round(&client).await;
                continue;
            }
            if !ws_allowed {
                self.set_link(SyncMode::Poll, SyncTransport::Https, Some("push channel unsupported by manager/proxy; polling".into()));
                self.roaming_round(&client).await;
                continue;
            }
            match self.push_session(&client).await {
                PushEnd::Unsupported(reason) => {
                    warn!("push channel unavailable: {reason}; falling back to HTTPS polling");
                    ws_unsupported_until = Some(Instant::now() + Duration::from_secs(1800));
                }
                PushEnd::BehaviorChanged => {}
                PushEnd::Lost(reason) => {
                    debug!("push channel ended: {reason}");
                    self.set_link(SyncMode::Push, SyncTransport::Websocket, Some(reason));
                }
            }
        }
    }

    /// One poll round for roaming nodes (or degraded static nodes): report + config + jobs, then wait.
    async fn roaming_round(self: &Arc<Self>, client: &Arc<ManagerClient>) {
        let poll = self.poll_interval();
        if let Err(e) = self.send_report(client).await {
            self.mark_error(&e);
        }
        if let Err(e) = crate::jobs::run_pending(self, client).await {
            self.mark_error(&e);
        }
        tokio::select! {
            _ = tokio::time::sleep(self.jittered(poll)) => {}
            _ = self.wake.notified() => {}
        }
    }

    fn poll_interval(&self) -> Duration {
        let s = load_last_good(&self.paths).ok().flatten().map(|lg| lg.config.node.sync).unwrap_or_default();
        Duration::from_secs(s.roaming_poll_seconds.max(15).into())
    }

    fn intervals(&self) -> (Duration, Duration, Duration) {
        let s = load_last_good(&self.paths).ok().flatten().map(|lg| lg.config.node.sync).unwrap_or_default();
        (
            Duration::from_secs(s.heartbeat_seconds.max(5).into()),
            Duration::from_secs(s.push_keepalive_seconds.max(10).into()),
            Duration::from_secs(s.calibration_seconds.max(60).into()),
        )
    }

    async fn push_session(self: &Arc<Self>, client: &Arc<ManagerClient>) -> PushEnd {
        let token = match client.session_token().await {
            Ok(t) => t,
            Err(e) => {
                self.mark_error(&e);
                return PushEnd::Lost(e.to_string());
            }
        };
        let permit = loop {
            let r = client.gate.lock().unwrap().try_acquire(RequestKind::Connect, Instant::now());
            match r {
                Ok(p) => break p,
                Err(Wait { until: Some(t), .. }) => tokio::time::sleep_until(t.into()).await,
                Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        };
        let ws = client.transport.websocket("/api/v1/nodes/self/stream", &token).await;
        let mut ws = match ws {
            Ok(ws) => {
                client.gate.lock().unwrap().release(permit, Outcome::Success { calibrated: false }, Instant::now());
                ws
            }
            Err((e, status, retry)) => {
                let retry_after = retry.and_then(|r| meduza_np_runtime::backoff::parse_retry_after(&r, std::time::SystemTime::now()));
                let (class, unsupported) = match status {
                    Some(401) | Some(403) => {
                        client.clear_session();
                        (None, false)
                    }
                    Some(429) | Some(503) => (Some(FailureClass::Overloaded), false),
                    Some(s) if (400..500).contains(&s) => (None, true), // proxy/manager does not support the upgrade
                    _ => (Some(FailureClass::Retryable), false),
                };
                let outcome = match class {
                    Some(c) => Outcome::Failure { class: c, retry_after, reason: e.to_string() },
                    None => Outcome::Neutral,
                };
                client.gate.lock().unwrap().release(permit, outcome, Instant::now());
                return if unsupported { PushEnd::Unsupported(e.to_string()) } else { PushEnd::Lost(e.to_string()) };
            }
        };
        let (cursor, last_ok) = {
            let st = self.status.lock().unwrap();
            (st.event_cursor.clone(), st.pointers.last_successful_revision.clone())
        };
        let hello = StreamHello { hello_version: 1, last_event_id: cursor, last_successful_revision: last_ok };
        if ws.send(Message::Text(serde_json::to_string(&hello).unwrap().into())).await.is_err() {
            client.gate.lock().unwrap().note_channel_lost("hello failed".into(), Instant::now());
            return PushEnd::Lost("hello failed".into());
        }
        let welcome: StreamWelcome = match tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => match serde_json::from_str(&t) {
                Ok(w) => w,
                Err(e) => return PushEnd::Lost(format!("bad welcome: {e}")),
            },
            _ => {
                client.gate.lock().unwrap().note_channel_lost("no welcome".into(), Instant::now());
                return PushEnd::Lost("no welcome".into());
            }
        };
        self.set_link(SyncMode::Push, SyncTransport::Websocket, None);
        info!("push channel established");
        // Calibrate right after subscribing to close the gap between the initial GET and the subscription.
        let applied = self.status.lock().unwrap().pointers.applied_revision.clone();
        if !welcome.cursor_valid || welcome.desired_revision != applied {
            if let Err(e) = self.pull(client).await {
                self.mark_error(&e);
            }
        } else {
            client.gate.lock().unwrap().note_channel_healthy(true, Instant::now());
        }

        let (heartbeat, keepalive, calibration) = self.intervals();
        let start_behavior = self.status.lock().unwrap().behavior;
        let mut next_report = tokio::time::Instant::now() + {
            let mut j = self.jitter.lock().unwrap();
            j.uniform(heartbeat)
        };
        let mut next_ping = tokio::time::Instant::now() + keepalive;
        let mut next_calibration = tokio::time::Instant::now() + self.jittered(calibration);
        let mut last_seen = Instant::now();
        loop {
            tokio::select! {
                msg = ws.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            last_seen = Instant::now();
                            client.gate.lock().unwrap().note_channel_healthy(false, Instant::now());
                            match serde_json::from_str::<SyncEvent>(&text) {
                                Ok(ev) => self.handle_event(client, ev).await,
                                Err(e) => warn!("ignoring malformed event: {e}"),
                            }
                        }
                        Some(Ok(Message::Pong(_))) | Some(Ok(Message::Ping(_))) => {
                            last_seen = Instant::now();
                            client.gate.lock().unwrap().note_channel_healthy(false, Instant::now());
                        }
                        Some(Ok(Message::Close(frame))) => {
                            let reason = frame.map(|f| format!("closed {}: {}", f.code, f.reason)).unwrap_or_else(|| "closed".into());
                            client.gate.lock().unwrap().note_channel_lost(reason.clone(), Instant::now());
                            return PushEnd::Lost(reason);
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            client.gate.lock().unwrap().note_channel_lost(e.to_string(), Instant::now());
                            return PushEnd::Lost(e.to_string());
                        }
                        None => {
                            client.gate.lock().unwrap().note_channel_lost("stream ended".into(), Instant::now());
                            return PushEnd::Lost("stream ended".into());
                        }
                    }
                }
                _ = tokio::time::sleep_until(next_ping) => {
                    if last_seen.elapsed() > keepalive * 2 + Duration::from_secs(5) {
                        client.gate.lock().unwrap().note_channel_lost("keepalive timeout".into(), Instant::now());
                        return PushEnd::Lost("keepalive timeout".into());
                    }
                    let _ = ws.send(Message::Ping(Vec::new().into())).await;
                    next_ping = tokio::time::Instant::now() + keepalive;
                }
                _ = tokio::time::sleep_until(next_report) => {
                    if let Err(e) = self.send_report(client).await { self.mark_error(&e) }
                    next_report = tokio::time::Instant::now() + self.jittered(heartbeat);
                }
                _ = tokio::time::sleep_until(next_calibration) => {
                    match self.pull(client).await {
                        Ok(()) => client.gate.lock().unwrap().note_channel_healthy(true, Instant::now()),
                        Err(e) => self.mark_error(&e),
                    }
                    next_calibration = tokio::time::Instant::now() + self.jittered(calibration);
                }
                _ = self.wake.notified() => {
                    if self.status.lock().unwrap().behavior != start_behavior {
                        let _ = ws.close(None).await;
                        return PushEnd::BehaviorChanged; // static → roaming: leave the subscription (§7.4)
                    }
                    if let Err(e) = self.pull(client).await { self.mark_error(&e) }
                    let _ = crate::jobs::run_pending(self, client).await;
                }
            }
        }
    }

    async fn handle_event(self: &Arc<Self>, client: &Arc<ManagerClient>, ev: SyncEvent) {
        if ev.node_id != client.node_id() {
            return;
        }
        if ev.event_id.parse::<u64>().is_ok() {
            self.status.lock().unwrap().event_cursor = Some(ev.event_id.clone());
        }
        match ev.kind {
            EventKind::ConfigAvailable | EventKind::CursorReset => {
                let applied = self.status.lock().unwrap().pointers.applied_revision.clone();
                // Duplicate or stale notifications never trigger work (§7.3).
                if ev.kind == EventKind::ConfigAvailable && ev.revision.is_some() && applied.as_ref() >= ev.revision.as_ref() {
                    return;
                }
                if let Err(e) = self.pull(client).await {
                    self.mark_error(&e);
                }
            }
            EventKind::JobAvailable => {
                if let Err(e) = crate::jobs::run_pending(self, client).await {
                    self.mark_error(&e);
                }
            }
            EventKind::ServerCooldown => {
                let wait = Duration::from_millis(ev.retry_after_ms.unwrap_or(30_000));
                client.gate.lock().unwrap().record_failure(FailureClass::Overloaded, Some(wait), "server cooldown".into(), Instant::now());
            }
            EventKind::BehaviorChanged => {
                if let Err(e) = self.pull(client).await {
                    self.mark_error(&e);
                }
            }
        }
    }

    /// Periodic plugin probes; never blocks sync or the local API (own task, bounded plugin concurrency).
    pub async fn probe_loop(self: Arc<Self>) {
        tokio::time::sleep(Duration::from_secs(5)).await;
        loop {
            let registry = self.registry();
            let mut results: Vec<InstanceStatus> = Vec::new();
            if let Ok(Some(lg)) = load_last_good(&self.paths) {
                for pkg in registry.registered().filter(|p| p.manifest.operations.iter().any(|o| o == "probe")) {
                    let short = pkg.short().to_string();
                    let Some(svc) = lg.config.services.get(&short) else { continue };
                    if !service_enabled(svc) {
                        continue;
                    }
                    let Some(program) = registry.plugin_path(pkg) else { continue };
                    let mut providers = std::collections::BTreeMap::new();
                    if let Some(slot) = &pkg.manifest.provider_slot {
                        if let Some(p) = registry.providers.get(slot) {
                            providers.insert(slot.clone(), p.clone());
                        }
                    }
                    let req = meduza_np_plugin_sdk::PluginRequest {
                        protocol_version: meduza_np_protocol::PLUGIN_PROTOCOL_VERSION,
                        request_id: format!("probe-{short}-{}", self.next_sequence()),
                        operation: "probe".into(),
                        config: lg.config.clone(),
                        instance: None,
                        providers,
                        context: meduza_np_plugin_sdk::PluginContext {
                            generation_root: self.paths.current_generation_link().to_string_lossy().into_owned(),
                            state_dir: self.paths.state.to_string_lossy().into_owned(),
                            run_dir: self.paths.run.to_string_lossy().into_owned(),
                            libexec_dir: self.paths.libexec.to_string_lossy().into_owned(),
                            platform: self.platform.clone(),
                            capabilities: registry.capabilities(),
                        },
                        params: Value::Null,
                    };
                    match self.engine.runner.call(&program, &req).await {
                        Ok(v) => match serde_json::from_value::<meduza_np_plugin_sdk::ProbeResult>(v) {
                            Ok(r) => results.extend(r.instances),
                            Err(e) => warn!("{short} probe: bad result: {e}"),
                        },
                        Err(e) => results.push(InstanceStatus {
                            id: format!("{short}/probe"),
                            kind: "probe".into(),
                            state: meduza_np_protocol::report::ServiceState::Unknown,
                            observed_at: Some(meduza_np_plugin_sdk::now_rfc3339()),
                            reason: Some(e.to_string()),
                            detail: Default::default(),
                        }),
                    }
                }
            }
            self.status.lock().unwrap().probes = results;
            self.expire_maintenance();
            tokio::time::sleep(self.jittered(Duration::from_secs(30))).await;
        }
    }

    /// Ends an expired maintenance window and resumes queued applies (§12.3).
    pub fn expire_maintenance(self: &Arc<Self>) {
        let expired = {
            let st = self.status.lock().unwrap();
            st.maintenance.is_some() && st.maintenance_until.map(|t| Instant::now() >= t).unwrap_or(false)
        };
        if expired {
            info!("maintenance window expired; resuming automatic reconcile");
            self.end_maintenance();
        }
    }

    pub fn end_maintenance(self: &Arc<Self>) {
        {
            let mut st = self.status.lock().unwrap();
            st.maintenance = None;
            st.maintenance_until = None;
        }
        let _ = std::fs::remove_file(self.paths.maintenance());
        if let Some((raw, digest, revision)) = self.queued.lock().unwrap().take() {
            let me = self.clone();
            tokio::spawn(async move { me.offer(raw, digest, revision).await });
        }
        self.wake.notify_one();
    }
}

enum PushEnd {
    Unsupported(String),
    Lost(String),
    BehaviorChanged,
}
