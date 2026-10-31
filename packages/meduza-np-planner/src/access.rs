//! OpenVPN Access sessions between the auth hook and the connect hook (§9.3, D38).
//!
//! OpenVPN runs `auth-user-pass-verify` and, for the same connection a moment later, `client-connect`. The manager's
//! answer to the first carries the session lifetime; the second turns it into the client's `session-timeout`. Planner
//! keeps that answer in memory only (never a password or token), keyed by instance, username and the client's
//! address, for a short time: a connect hook without a matching entry denies.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use meduza_np_protocol::access::{AccessAuthRequest, AccessAuthResponse, AccessDecision};

/// Time between the auth decision and OpenVPN's client-connect for the same connection.
pub const SESSION_HANDOFF: Duration = Duration::from_secs(120);
const MAX_ENTRIES: usize = 4096;

/// Reasons that mean "no manager decision was obtained", as opposed to a decision against the user.
const UNDECIDED: &[&str] = &["manager-unavailable", "timeout", "rate-limited", "ldap-unavailable"];

/// What the connect hook learns about an authorized connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handoff {
    /// Disconnect after this many seconds (`session-timeout`).
    Lifetime(u32),
    /// A token renegotiation kept without a manager decision: fine for a running session, not for a new connection.
    Unverified,
}

#[derive(Default)]
pub struct Sessions {
    entries: HashMap<(String, String, String), (Handoff, Instant)>,
}

fn key(instance: &str, username: &str, ip: Option<&str>, port: Option<u16>) -> (String, String, String) {
    (instance.to_string(), username.to_string(), format!("{}:{}", ip.unwrap_or("-"), port.unwrap_or(0)))
}

/// Planner's own policy on top of the manager answer: a token-verified renegotiation (`reauth`) that got no decision
/// (manager or directory unreachable, overloaded, too slow) is kept, so an outage does not disconnect every Access user
/// at their next renegotiation; their `session-timeout` still ends them. Explicit denials always apply, and new logins
/// fail closed.
pub fn decide(req: &AccessAuthRequest, resp: AccessAuthResponse) -> AccessAuthResponse {
    if req.reauth && resp.decision == AccessDecision::Deny && UNDECIDED.contains(&resp.reason_code.as_str()) {
        return AccessAuthResponse { decision: AccessDecision::Allow, reason_code: "kept-unverified".into(), ..resp };
    }
    resp
}

impl Sessions {
    /// Records an allowed decision for the connect hook.
    pub fn record(&mut self, req: &AccessAuthRequest, resp: &AccessAuthResponse, now: Instant) {
        if resp.decision != AccessDecision::Allow {
            return;
        }
        self.entries.retain(|_, (_, at)| now.duration_since(*at) < SESSION_HANDOFF);
        if self.entries.len() >= MAX_ENTRIES {
            return;
        }
        let handoff = if resp.reason_code == "kept-unverified" { Handoff::Unverified } else { Handoff::Lifetime(resp.valid_for_seconds) };
        let s = &req.session;
        self.entries.insert(key(&req.instance_id, &req.username, s.untrusted_ip.as_deref(), s.untrusted_port), (handoff, now));
    }

    /// The connect hook's lookup; consumes the entry.
    pub fn take(&mut self, instance: &str, username: &str, ip: Option<&str>, port: Option<u16>, now: Instant) -> Option<Handoff> {
        let (h, at) = self.entries.remove(&key(instance, username, ip, port))?;
        (now.duration_since(at) < SESSION_HANDOFF).then_some(h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meduza_np_protocol::access::AccessSessionMeta;

    fn req(reauth: bool) -> AccessAuthRequest {
        AccessAuthRequest {
            request_id: "r".into(),
            node_id: None,
            instance_id: "access".into(),
            username: "alice".into(),
            password: if reauth { String::new() } else { "pw".into() },
            reauth,
            session: AccessSessionMeta { untrusted_ip: Some("10.50.0.40".into()), untrusted_port: Some(40000), ..Default::default() },
        }
    }

    fn allow(valid: u32) -> AccessAuthResponse {
        AccessAuthResponse {
            decision: AccessDecision::Allow,
            reason_code: "ok".into(),
            valid_for_seconds: valid,
            ..AccessAuthResponse::deny("r", "ok")
        }
    }

    #[test]
    fn undecided_reauth_is_kept_new_logins_and_denials_are_not() {
        assert_eq!(decide(&req(true), AccessAuthResponse::deny("r", "manager-unavailable")).reason_code, "kept-unverified");
        assert_eq!(decide(&req(true), AccessAuthResponse::deny("r", "ldap-unavailable")).decision, AccessDecision::Allow);
        assert_eq!(decide(&req(true), AccessAuthResponse::deny("r", "not-authorized")).decision, AccessDecision::Deny);
        assert_eq!(decide(&req(true), AccessAuthResponse::deny("r", "session-expired")).decision, AccessDecision::Deny);
        assert_eq!(decide(&req(false), AccessAuthResponse::deny("r", "manager-unavailable")).decision, AccessDecision::Deny);
    }

    #[test]
    fn handoff_by_connection_once_and_bounded_in_time() {
        let t0 = Instant::now();
        let mut s = Sessions::default();
        s.record(&req(false), &allow(3600), t0);
        s.record(&req(false), &AccessAuthResponse::deny("r", "bad-credentials"), t0);
        assert_eq!(s.take("access", "alice", Some("10.50.0.40"), Some(40001), t0), None, "other connection");
        assert_eq!(s.take("access", "alice", Some("10.50.0.40"), Some(40000), t0), Some(Handoff::Lifetime(3600)));
        assert_eq!(s.take("access", "alice", Some("10.50.0.40"), Some(40000), t0), None, "consumed");
        s.record(&req(true), &decide(&req(true), AccessAuthResponse::deny("r", "timeout")), t0);
        assert_eq!(s.take("access", "alice", Some("10.50.0.40"), Some(40000), t0), Some(Handoff::Unverified));
        s.record(&req(false), &allow(60), t0);
        assert_eq!(s.take("access", "alice", Some("10.50.0.40"), Some(40000), t0 + SESSION_HANDOFF), None, "too late");
    }
}
