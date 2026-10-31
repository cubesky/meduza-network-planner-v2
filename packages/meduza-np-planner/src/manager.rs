//! Manager API client. Every request passes the shared recovery gate (§7.5.2); this is the only place where
//! HTTP results are classified into success / retryable / overload / credential failures.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use meduza_np_protocol::access::{AccessAuthRequest, AccessAuthResponse};
use meduza_np_protocol::api::{Ack, ChallengeResponse, EnrollRequest, EnrollResponse, Job, JobResult, SessionResponse};
use meduza_np_protocol::report::NodeReport;
use meduza_np_runtime::backoff::{classify_status, parse_retry_after, FailureClass, Gate, Outcome, RequestKind, Wait, WaitReason};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::identity::Identity;
use crate::transport::{HttpResponse, Transport, TransportError};

#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error("manager unavailable: {0}")]
    Unavailable(String),
    #[error("credentials rejected: {0}")]
    Credential(String),
    #[error("request rejected ({status}): {body}")]
    Rejected { status: u16, body: String },
    #[error("invalid response: {0}")]
    Invalid(String),
}

pub enum ConfigFetch {
    NotModified,
    NoConfig,
    Config { raw: Value, digest: String, revision: String },
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportReply {
    pub accepted: bool,
    pub desired_revision: Option<String>,
    pub config_digest: Option<String>,
}

pub struct ManagerClient {
    pub transport: Transport,
    pub gate: Arc<Mutex<Gate>>,
    identity: Arc<Identity>,
    node_id: String,
    token: Mutex<Option<(String, Instant)>>,
    access_slots: Semaphore,
}

const MAX_CONFIG_BODY: usize = 8 * 1024 * 1024;
const MAX_SMALL_BODY: usize = 256 * 1024;

impl ManagerClient {
    pub fn new(transport: Transport, gate: Arc<Mutex<Gate>>, identity: Arc<Identity>, node_id: String) -> Self {
        Self { transport, gate, identity, node_id, token: Mutex::new(None), access_slots: Semaphore::new(2) }
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// Waits for a permit from the shared gate. Credential errors are returned instead of waited on.
    async fn permit(&self, kind: RequestKind) -> Result<meduza_np_runtime::backoff::Permit, CallError> {
        loop {
            let r = self.gate.lock().unwrap().try_acquire(kind, Instant::now());
            match r {
                Ok(p) => return Ok(p),
                Err(Wait { reason: WaitReason::Credential, .. }) => {
                    let why = self.gate.lock().unwrap().credential_error().unwrap_or("credential error").to_string();
                    return Err(CallError::Credential(why));
                }
                Err(Wait { until: Some(t), .. }) => tokio::time::sleep_until(t.into()).await,
                Err(Wait { until: None, .. }) => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
    }

    fn classify(resp: &HttpResponse) -> Option<(FailureClass, Option<Duration>)> {
        classify_status(resp.status).map(|c| (c, resp.header("retry-after").and_then(|v| parse_retry_after(v, SystemTime::now()))))
    }

    /// Runs one request under a permit. `calibrating` marks requests that confirm the desired revision/ACK.
    #[allow(clippy::too_many_arguments)]
    async fn call(
        &self,
        kind: RequestKind,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        extra: &[(&str, String)],
        auth: bool,
        calibrating: bool,
        max: usize,
    ) -> Result<HttpResponse, CallError> {
        let mut headers: Vec<(&str, String)> = extra.to_vec();
        if auth {
            let token = self.session_token().await?;
            headers.push(("authorization", format!("Bearer {token}")));
        }
        self.send(kind, method, path, body, &headers, auth, calibrating, max).await
    }

    /// Sends one request under a gate permit; never fetches tokens itself (no recursion with the session).
    #[allow(clippy::too_many_arguments)]
    async fn send(
        &self,
        kind: RequestKind,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        headers: &[(&str, String)],
        auth: bool,
        calibrating: bool,
        max: usize,
    ) -> Result<HttpResponse, CallError> {
        let permit = self.permit(kind).await?;
        let result = self.transport.request(method, path, headers, body, max).await;
        let now = Instant::now();
        match result {
            Err(e) => {
                self.transport.reset();
                let class = if matches!(e, TransportError::Trust(_)) { FailureClass::Credential } else { FailureClass::Retryable };
                self.gate.lock().unwrap().release(permit, Outcome::Failure { class, retry_after: None, reason: e.to_string() }, now);
                Err(if class == FailureClass::Credential {
                    CallError::Credential(e.to_string())
                } else {
                    CallError::Unavailable(e.to_string())
                })
            }
            Ok(resp) => match Self::classify(&resp) {
                Some((FailureClass::Credential, _)) if auth => {
                    // Expired or revoked session token: drop it; the next call re-authenticates. Repeated
                    // rejection of the session handshake itself parks the gate as a credential error.
                    *self.token.lock().unwrap() = None;
                    self.gate.lock().unwrap().release(permit, Outcome::Neutral, now);
                    Err(CallError::Unavailable(format!("session rejected ({})", resp.status)))
                }
                Some((class, retry_after)) => {
                    let reason = format!("HTTP {} on {path}", resp.status);
                    self.gate.lock().unwrap().release(permit, Outcome::Failure { class, retry_after, reason: reason.clone() }, now);
                    Err(if class == FailureClass::Credential { CallError::Credential(reason) } else { CallError::Unavailable(reason) })
                }
                None => {
                    let ok = resp.status < 400;
                    self.gate.lock().unwrap().release(
                        permit,
                        if ok { Outcome::Success { calibrated: calibrating } } else { Outcome::Neutral },
                        now,
                    );
                    if ok {
                        Ok(resp)
                    } else {
                        Err(CallError::Rejected {
                            status: resp.status,
                            body: String::from_utf8_lossy(&resp.body).chars().take(500).collect(),
                        })
                    }
                }
            },
        }
    }

    fn json<T: DeserializeOwned>(resp: &HttpResponse) -> Result<T, CallError> {
        serde_json::from_slice(&resp.body).map_err(|e| CallError::Invalid(e.to_string()))
    }

    /// Bearer token, renewed a minute before expiry through challenge/response (D3).
    pub async fn session_token(&self) -> Result<String, CallError> {
        if let Some((t, exp)) = self.token.lock().unwrap().clone() {
            if exp > Instant::now() + Duration::from_secs(60) {
                return Ok(t);
            }
        }
        let body = serde_json::to_vec(&serde_json::json!({"nodeId": self.node_id})).unwrap();
        let resp = self
            .send(RequestKind::Short, "POST", "/api/v1/nodes/self/session/challenge", Some(body), &[], false, false, MAX_SMALL_BODY)
            .await?;
        let ch: ChallengeResponse = Self::json(&resp)?;
        let signature = self.identity.sign_b64(&meduza_np_protocol::api::session_signing_input(&self.node_id, &ch.nonce));
        let body = serde_json::to_vec(&serde_json::json!({"nodeId": self.node_id, "nonce": ch.nonce, "signature": signature})).unwrap();
        let resp = match self
            .send(RequestKind::Short, "POST", "/api/v1/nodes/self/session", Some(body), &[], false, false, MAX_SMALL_BODY)
            .await
        {
            Err(CallError::Rejected { status: 401 | 403, body }) => {
                let reason = format!("node identity rejected: {body}");
                self.gate.lock().unwrap().record_failure(FailureClass::Credential, None, reason.clone(), Instant::now());
                return Err(CallError::Credential(reason));
            }
            other => other?,
        };
        let s: SessionResponse = Self::json(&resp)?;
        let expires = Instant::now() + Duration::from_secs(s.expires_in_seconds.into());
        *self.token.lock().unwrap() = Some((s.token.clone(), expires));
        Ok(s.token)
    }

    pub async fn fetch_config(&self, etag: Option<&str>, revision: Option<&str>) -> Result<ConfigFetch, CallError> {
        let path = match revision {
            Some(r) => format!("/api/v1/nodes/self/config?revision={r}"),
            None => "/api/v1/nodes/self/config".to_string(),
        };
        let headers: Vec<(&str, String)> = etag.map(|e| vec![("if-none-match", format!("\"{e}\""))]).unwrap_or_default();
        let resp = self.call(RequestKind::Short, "GET", &path, None, &headers, true, true, MAX_CONFIG_BODY).await?;
        match resp.status {
            304 => Ok(ConfigFetch::NotModified),
            204 => Ok(ConfigFetch::NoConfig),
            _ => {
                let env: Value = Self::json(&resp)?;
                let raw = env.get("config").cloned().ok_or_else(|| CallError::Invalid("missing config".into()))?;
                let digest = env.pointer("/meta/configDigest").and_then(Value::as_str).unwrap_or_default().to_string();
                let revision = env.pointer("/meta/revision").and_then(Value::as_str).unwrap_or_default().to_string();
                Ok(ConfigFetch::Config { raw, digest, revision })
            }
        }
    }

    pub async fn post_report(&self, report: &NodeReport) -> Result<ReportReply, CallError> {
        let body = serde_json::to_vec(report).map_err(|e| CallError::Invalid(e.to_string()))?;
        let resp =
            self.call(RequestKind::Short, "POST", "/api/v1/nodes/self/reports", Some(body), &[], true, false, MAX_SMALL_BODY).await?;
        Self::json(&resp)
    }

    pub async fn post_ack(&self, ack: &Ack) -> Result<(), CallError> {
        let body = serde_json::to_vec(ack).map_err(|e| CallError::Invalid(e.to_string()))?;
        self.call(RequestKind::Short, "POST", "/api/v1/nodes/self/acks", Some(body), &[], true, true, MAX_SMALL_BODY).await.map(|_| ())
    }

    /// Dashboard sign-in through the manager identity (`login` = begin, `exchange` = redeem the one-time code).
    pub async fn dashboard(&self, step: &str, body: &Value) -> Result<Value, CallError> {
        let path = match step {
            "login" => "/api/v1/nodes/self/dashboard/login",
            "exchange" => "/api/v1/nodes/self/dashboard/exchange",
            other => return Err(CallError::Invalid(format!("unknown dashboard step {other}"))),
        };
        let body = serde_json::to_vec(body).map_err(|e| CallError::Invalid(e.to_string()))?;
        let resp = self.call(RequestKind::Short, "POST", path, Some(body), &[], true, false, MAX_SMALL_BODY).await?;
        Self::json(&resp)
    }

    pub async fn jobs(&self) -> Result<Vec<Job>, CallError> {
        let resp = self.call(RequestKind::Short, "GET", "/api/v1/nodes/self/jobs", None, &[], true, false, MAX_SMALL_BODY).await?;
        Self::json(&resp)
    }

    pub async fn job_result(&self, result: &JobResult) -> Result<(), CallError> {
        let body = serde_json::to_vec(result).map_err(|e| CallError::Invalid(e.to_string()))?;
        let path = format!("/api/v1/nodes/self/jobs/{}/result", result.job_id);
        self.call(RequestKind::Short, "POST", &path, Some(body), &[], true, false, MAX_SMALL_BODY).await.map(|_| ())
    }

    /// Access authentication with its own small capacity. Never queues behind the overload gate: if the
    /// manager is in backoff/cooldown, or the deadline would pass, the login is denied fast (§7.5.2, §9.3).
    pub async fn access_auth(&self, req: &AccessAuthRequest, deadline: Duration) -> AccessAuthResponse {
        let deny = |code: &str| AccessAuthResponse::deny(req.request_id.clone(), code);
        if self.gate.lock().unwrap().next_allowed(Instant::now()).is_some() {
            return deny("manager-unavailable");
        }
        let Ok(_slot) = self.access_slots.try_acquire() else { return deny("rate-limited") };
        let work = async {
            let token = self.session_token().await.map_err(|_| "manager-unavailable")?;
            let mut body = serde_json::to_value(req).map_err(|_| "invalid-request")?;
            body["nodeId"] = Value::String(self.node_id.clone());
            let resp = self
                .transport
                .request(
                    "POST",
                    "/api/v1/nodes/self/access-auth",
                    &[("authorization", format!("Bearer {token}"))],
                    Some(serde_json::to_vec(&body).unwrap()),
                    MAX_SMALL_BODY,
                )
                .await;
            match resp {
                Err(e) => {
                    self.gate.lock().unwrap().record_failure(FailureClass::Retryable, None, e.to_string(), Instant::now());
                    Err("manager-unavailable")
                }
                Ok(r) if r.status == 200 => serde_json::from_slice::<AccessAuthResponse>(&r.body).map_err(|_| "manager-unavailable"),
                Ok(r) => {
                    if let Some((class, ra)) = Self::classify(&r) {
                        if class != FailureClass::Credential {
                            self.gate.lock().unwrap().record_failure(class, ra, format!("HTTP {}", r.status), Instant::now());
                        }
                    }
                    Err(if r.status == 429 { "rate-limited" } else { "manager-unavailable" })
                }
            }
        };
        match tokio::time::timeout(deadline, work).await {
            Ok(Ok(resp)) if resp.request_id == req.request_id => resp,
            Ok(Ok(_)) => deny("invalid-request"),
            Ok(Err(code)) => deny(code),
            Err(_) => deny("timeout"),
        }
    }

    /// Enrollment: a user action, not subject to background backoff, but still a single bounded attempt.
    pub async fn enroll(transport: &Transport, req: &EnrollRequest) -> Result<EnrollResponse, CallError> {
        let body = serde_json::to_vec(req).map_err(|e| CallError::Invalid(e.to_string()))?;
        let resp = transport
            .request("POST", "/api/v1/nodes/enroll", &[], Some(body), MAX_SMALL_BODY)
            .await
            .map_err(|e| CallError::Unavailable(e.to_string()))?;
        if resp.status == 200 {
            Self::json(&resp)
        } else {
            Err(CallError::Rejected { status: resp.status, body: String::from_utf8_lossy(&resp.body).chars().take(500).collect() })
        }
    }

    pub fn clear_session(&self) {
        *self.token.lock().unwrap() = None;
        self.transport.reset();
    }
}
