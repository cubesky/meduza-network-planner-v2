//! Push event envelope shared by the WebSocket and HTTP/2 event stream channels (§7.3).

use serde::{Deserialize, Serialize};

use crate::revision::RevisionId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncEvent {
    pub event_version: u32,
    /// Monotonic per node; doubles as the resume cursor. A cursor only says how far notifications were
    /// delivered — never that a configuration was applied.
    pub event_id: String,
    #[serde(rename = "type")]
    pub kind: EventKind,
    pub node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<RevisionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// For `server.cooldown`: milliseconds clients should stay quiet (plus their own jitter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum EventKind {
    /// A new desired revision exists; fetch the full snapshot with GET /nodes/self/config.
    #[serde(rename = "config.available")]
    ConfigAvailable,
    /// One or more jobs are waiting at GET /nodes/self/jobs.
    #[serde(rename = "job.available")]
    JobAvailable,
    /// The resume cursor is unknown or a gap was detected: fetch the current config over HTTPS.
    #[serde(rename = "cursor.reset")]
    CursorReset,
    /// Manager is shedding load.
    #[serde(rename = "server.cooldown")]
    ServerCooldown,
    /// The node's `behavior` changed; renegotiate the channel.
    #[serde(rename = "node.behavior")]
    BehaviorChanged,
}

/// First frame the planner sends after the WebSocket upgrade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StreamHello {
    pub hello_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_successful_revision: Option<RevisionId>,
}

/// Manager's reply to the hello frame, sent before any events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StreamWelcome {
    pub desired_revision: Option<RevisionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_digest: Option<String>,
    /// False when the cursor could not be resumed; the client must GET the full config.
    pub cursor_valid: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spec_event_parses() {
        let e: SyncEvent = serde_json::from_value(json!({
          "eventVersion": 1, "eventId": "event-123", "type": "config.available",
          "nodeId": "gw-a", "revision": "rev-000043", "configDigest": "abc"
        }))
        .unwrap();
        assert_eq!(e.kind, EventKind::ConfigAvailable);
        assert_eq!(serde_json::to_value(&e).unwrap()["type"], json!("config.available"));
    }
}
