//! manager ↔ planner HTTPS API bodies (§7.3, docs/DECISIONS.md D3).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{NodeBehavior, NodeConfig};
use crate::revision::RevisionMeta;

/// `POST /api/v1/nodes/enroll`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EnrollRequest {
    /// One-time, expiring token created in the manager UI.
    pub enrollment_token: String,
    /// Base64 (standard, padded) raw 32-byte Ed25519 public key.
    pub public_key: String,
    pub hostname: String,
    pub arch: String,
    #[serde(default)]
    pub planner_version: String,
    #[serde(default)]
    pub planner_api: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EnrollResponse {
    pub node_id: String,
    pub behavior: NodeBehavior,
    /// Manager's protocol range, to negotiate features.
    pub manager_api: crate::manifest::ApiRange,
}

/// `POST /api/v1/nodes/self/session/challenge`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeRequest {
    pub node_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeResponse {
    /// Base64 random nonce, valid once for `expiresInSeconds`.
    pub nonce: String,
    pub expires_in_seconds: u32,
}

/// `POST /api/v1/nodes/self/session`. The signature covers [`session_signing_input`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionRequest {
    pub node_id: String,
    pub nonce: String,
    /// Base64 Ed25519 signature.
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionResponse {
    pub token: String,
    pub expires_in_seconds: u32,
}

/// Domain-separated bytes the node signs to obtain a session.
pub fn session_signing_input(node_id: &str, nonce: &str) -> Vec<u8> {
    format!("meduza-np-node-session-v1\n{node_id}\n{nonce}").into_bytes()
}

/// `GET /api/v1/nodes/self/config` body (200). 304 when `after` / `If-None-Match` already matches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigEnvelope {
    pub meta: RevisionMeta,
    pub config: NodeConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AckStage {
    Received,
    Validated,
    Applied,
    Failed,
    Rejected,
}

/// `POST /api/v1/nodes/self/acks` — idempotent on (revision, stage, bootId).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Ack {
    pub revision: crate::revision::RevisionId,
    pub config_digest: String,
    pub stage: AckStage,
    pub boot_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Per-instance outcome for the manager's per-node result view.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instances: Vec<InstanceOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceOutcome {
    pub instance: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// `GET /api/v1/nodes/self/jobs` item. Jobs are durable on the manager and idempotent by id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub job_id: String,
    /// `generate-keypair`, `restart-instance`, `collect-diagnostics`, `run-action`, ...
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(default)]
    pub params: Value,
    /// RFC 3339; jobs past this are reported as expired instead of executed.
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobResult {
    pub job_id: String,
    pub ok: bool,
    #[serde(default)]
    pub result: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Standard error body for all API errors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}
