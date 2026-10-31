//! Meduza NP planner: the single node controller (AGENT.md Â§2.1, Â§11).
//!
//! Library crate so the engine can be tested without a running system; `main.rs` wires it to the OS.

#[macro_use]
pub mod log;

pub mod access;
pub mod apply;
pub mod identity;
pub mod jobs;
pub mod keystore;
pub mod local_api;
pub mod manager;
pub mod netbackend;
pub mod netplan;
pub mod platform;
pub mod plugins;
pub mod registry;
pub mod reporter;
pub mod settings;
pub mod state;
pub mod supervisor;
pub mod support;
pub mod sync;
pub mod transport;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

use std::sync::{Arc, Mutex};
use std::time::Instant;

use meduza_np_runtime::backoff::{BackoffPolicy, Gate};
use meduza_np_runtime::jitter::OsJitter;

/// Builds the manager client from `/etc/meduza-np/manager.json` and the node identity.
pub fn connect_manager(p: &Arc<sync::Planner>) -> Result<(), String> {
    let settings = settings::ManagerSettings::load(&p.paths.manager_settings()).map_err(|e| e.to_string())?.ok_or("not enrolled")?;
    settings.validate()?;
    let identity = identity::Identity::load_or_create(&p.paths.identity_dir()).map_err(|e| e.to_string())?;
    let transport = transport::Transport::new(settings.base(), settings.ca_file.as_deref()).map_err(|e| e.to_string())?;
    let mut policy = BackoffPolicy::default();
    // Initial values are configurable for fleet sizing and tests (§7.5.1).
    if let Some(s) = std::env::var("MEDUZA_NP_BACKOFF_SPREAD_SECONDS").ok().and_then(|v| v.parse().ok()) {
        policy.startup_spread = std::time::Duration::from_secs(s);
    }
    if let Some(s) = std::env::var("MEDUZA_NP_BACKOFF_CAP_SECONDS").ok().and_then(|v| v.parse().ok()) {
        policy.cap = std::time::Duration::from_secs(s);
    }
    let mut gate = Gate::new(policy, Box::new(OsJitter::new()));
    // A restart must not reset the failure level and hammer the manager (§7.5.3).
    gate.restore(state::SyncPersisted::load(&p.paths.sync_state()).backoff_level, Instant::now());
    let client = manager::ManagerClient::new(transport, Arc::new(Mutex::new(gate)), Arc::new(identity), settings.node_id.clone());
    p.status.lock().unwrap().behavior = settings.behavior;
    *p.client.lock().unwrap() = Some(Arc::new(client));
    p.wake.notify_one();
    Ok(())
}

/// Enrolls with a one-time token: new identity key, `POST /nodes/enroll`, persist settings, connect (§7.1).
pub async fn enroll(
    p: &Arc<sync::Planner>,
    url: &str,
    token: &str,
    ca_file: Option<String>,
    allow_insecure: bool,
) -> Result<String, String> {
    let draft = settings::ManagerSettings {
        manager_url: url.trim_end_matches('/').to_string(),
        node_id: String::new(),
        ca_file: ca_file.clone(),
        behavior: Default::default(),
        allow_insecure,
    };
    draft.validate()?;
    let transport = transport::Transport::new(draft.base(), ca_file.as_deref()).map_err(|e| e.to_string())?;
    let identity = identity::Identity::regenerate(&p.paths.identity_dir()).map_err(|e| e.to_string())?;
    let req = meduza_np_protocol::api::EnrollRequest {
        enrollment_token: token.to_string(),
        public_key: identity.public_key_b64(),
        hostname: std::fs::read_to_string("/proc/sys/kernel/hostname").map(|h| h.trim().to_string()).unwrap_or_default(),
        arch: std::env::consts::ARCH.to_string(),
        planner_version: VERSION.to_string(),
        planner_api: meduza_np_protocol::PLANNER_API_VERSION,
    };
    let resp = manager::ManagerClient::enroll(&transport, &req).await.map_err(|e| e.to_string())?;
    let saved = settings::ManagerSettings { node_id: resp.node_id.clone(), behavior: resp.behavior, ..draft };
    saved.save(&p.paths.manager_settings()).map_err(|e| e.to_string())?;
    // A fresh identity starts with a clean credential state but keeps the persisted backoff level.
    connect_manager(p)?;
    info!("enrolled as {} at {}", resp.node_id, saved.manager_url);
    Ok(resp.node_id)
}
