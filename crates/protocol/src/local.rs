//! Local control protocol over Unix domain sockets (§12.3, §13.1).
//!
//! Framing: one JSON object per line (`\n`-terminated), at most [`MAX_FRAME`] bytes. A connection may carry
//! several request/response pairs; responses carry the request `id`.
//!
//! Sockets (under `/run/meduza-np/`):
//! - `planner.sock`   — CLI/dashboard. Authorized by peer credentials: root or the `meduza-np` group may
//!   call mutating methods; other local users get read-only methods only if the socket mode allows.
//! - `access.sock`    — only `access.auth`, connectable by the OpenVPN runtime user (§9.3).
//! - `updater.sock`   — served by the updater; planner and CLI call `update.*` methods there, and the
//!   updater calls planner's `maintenance.*` methods on `planner.sock` (versioned by `protocol`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const LOCAL_PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LocalRequest {
    pub protocol: u32,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LocalResponse {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub result: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<LocalError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LocalError {
    /// `forbidden`, `not-found`, `invalid-params`, `unavailable`, `busy`, `maintenance`, `internal`.
    pub code: String,
    pub message: String,
}

impl LocalResponse {
    pub fn ok(id: u64, result: Value) -> Self {
        Self { id, ok: true, result, error: None }
    }

    pub fn err(id: u64, code: &str, message: impl Into<String>) -> Self {
        Self { id, ok: false, result: Value::Null, error: Some(LocalError { code: code.into(), message: message.into() }) }
    }
}

/// Planner methods and whether they mutate state. Mutating methods need an authorized peer.
pub const PLANNER_METHODS: &[(&str, bool)] = &[
    ("status", false),
    ("features", false),
    ("packages", false),
    ("instances", false),
    ("config.current", false),
    ("logs", false),
    ("doctor", false),
    ("plan.preview", false),
    ("validate", false),
    // redacted, size-bounded support bundle (diagnostics export)
    ("support.bundle", false),
    ("enroll", true),
    ("sync.now", true),
    ("instance.start", true),
    ("instance.stop", true),
    ("instance.restart", true),
    ("maintenance.status", false),
    ("maintenance.start", true),
    ("maintenance.stop", true),
    ("action.run", true),
    // updater ↔ planner maintenance protocol
    ("maintenance.lock", true),
    ("maintenance.unlock", true),
    ("apply.pause", true),
    ("apply.resume", true),
    ("drain", true),
    ("health", false),
    ("registry.rescan", true),
    // Graceful exit with code 75 so the owning service manager (procd/systemd/Podman) starts the new binary.
    ("planner.restart", true),
    // Activate only the staged updater via a bounded helper; its independent owner starts the new daemon.
    ("updater.activate", true),
    // meduza-np-dashboard sign-in relay to the manager
    ("dashboard.login", true),
    ("dashboard.exchange", true),
];

pub const UPDATER_METHODS: &[(&str, bool)] = &[
    ("update.status", false),
    ("update.check", true),
    ("update.plan", false),
    ("update.apply", true),
    ("update.pause", true),
    ("update.resume", true),
    ("update.defer", true),
];

pub fn method_is_mutating(table: &[(&str, bool)], method: &str) -> Option<bool> {
    table.iter().find(|(m, _)| *m == method).map(|(_, mutating)| *mutating)
}

/// Parameters of `maintenance.start` (CLI/dashboard) and `maintenance.lock` (updater).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceRequest {
    pub reason: String,
    /// Seconds until automatic expiry (max 24 h).
    pub duration_seconds: u32,
    #[serde(default)]
    pub pause_apply: bool,
    /// Instances to stop for the duration (empty = none).
    #[serde(default)]
    pub stop_instances: Vec<String>,
}
