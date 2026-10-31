//! OpenVPN Access authentication (§9.3): helper → planner (UDS) → manager → OpenLDAP.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Sent by the `openvpn-auth` helper to planner, and by planner to manager (nodeId added from identity).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccessAuthRequest {
    pub request_id: String,
    /// Filled by planner from its machine identity; helpers' values are ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    pub instance_id: String,
    pub username: String,
    /// Empty for `reauth`.
    #[serde(default)]
    pub password: String,
    /// Renegotiation of a session OpenVPN already verified with its HMAC auth token (`auth-gen-token
    /// external-auth`): no password; the manager re-checks the account, groups, policy and session lifetime.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reauth: bool,
    #[serde(default)]
    pub session: AccessSessionMeta,
}

impl fmt::Debug for AccessAuthRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessAuthRequest")
            .field("request_id", &self.request_id)
            .field("node_id", &self.node_id)
            .field("instance_id", &self.instance_id)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("reauth", &self.reauth)
            .finish()
    }
}

/// Non-secret session metadata OpenVPN exposes to the hook (env vars).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccessSessionMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub untrusted_ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub untrusted_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub common_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AccessDecision {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccessAuthResponse {
    pub request_id: String,
    pub decision: AccessDecision,
    /// Machine readable: `ok`, `ok-reauth`, `bad-credentials`, `not-authorized`, `user-disabled`,
    /// `ldap-unavailable`, `manager-unavailable`, `timeout`, `rate-limited`, `instance-not-allowed`,
    /// `invalid-request`, `no-session`, `session-expired`; planner answers a `reauth` it could not get a manager
    /// decision for with allow + `kept-unverified` (the session keeps its `session-timeout`).
    pub reason_code: String,
    /// Remaining session lifetime: OpenVPN disconnects the client after it (`session-timeout`).
    #[serde(default)]
    pub valid_for_seconds: u32,
    /// Authorization attributes (groups that matched, policy id). Never LDAP credentials.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
}

impl AccessAuthResponse {
    pub fn deny(request_id: impl Into<String>, reason_code: &str) -> Self {
        Self {
            request_id: request_id.into(),
            decision: AccessDecision::Deny,
            reason_code: reason_code.into(),
            valid_for_seconds: 0,
            groups: Vec::new(),
            policy_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_not_in_debug() {
        let r = AccessAuthRequest {
            request_id: "r".into(),
            node_id: None,
            instance_id: "access-main".into(),
            username: "alice".into(),
            password: "s3cret".into(),
            reauth: false,
            session: Default::default(),
        };
        assert!(!format!("{r:?}").contains("s3cret"));
    }
}
