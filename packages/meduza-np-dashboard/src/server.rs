//! HTTP routing: static UI, `/auth/*`, `/api/*` (read: viewer; change: operator + CSRF). The API is a fixed map onto
//! planner/updater local methods — never a generic proxy — so the dashboard cannot reach methods the UI does not use.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{HeaderValue, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_TYPE, COOKIE, LOCATION, SET_COOKIE};
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{json, Value};

use crate::auth::{self, Role, Session, Sessions, LOGIN_COOKIE, SESSION_COOKIE};

pub type Resp = Response<Full<Bytes>>;

pub struct App {
    pub www: PathBuf,
    pub planner_socket: PathBuf,
    pub updater_socket: PathBuf,
    pub run_dir: PathBuf,
    /// `https://host:port` the browser uses; required for manager sign-in.
    pub public_url: Option<String>,
    pub secure_cookies: bool,
    pub sessions: Mutex<Sessions>,
}

fn json_resp(status: StatusCode, v: &Value) -> Resp {
    let mut r = Response::new(Full::new(Bytes::from(serde_json::to_vec(v).unwrap_or_default())));
    *r.status_mut() = status;
    r.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn error(status: StatusCode, code: &str, message: &str) -> Resp {
    json_resp(status, &json!({"error": code, "message": message}))
}

fn redirect(to: &str, cookies: &[String]) -> Resp {
    let mut r = Response::new(Full::new(Bytes::new()));
    *r.status_mut() = StatusCode::FOUND;
    if let Ok(v) = HeaderValue::from_str(to) {
        r.headers_mut().insert(LOCATION, v);
    }
    for c in cookies {
        if let Ok(v) = HeaderValue::from_str(c) {
            r.headers_mut().append(SET_COOKIE, v);
        }
    }
    r
}

fn security_headers(r: &mut Resp) {
    let h = r.headers_mut();
    h.insert("content-security-policy", HeaderValue::from_static("default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    // the callback URL carries a one-time code: never leak it through Referer
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
}

/// Read API: path → (socket, method). `None` socket = planner.
const READS: &[(&str, bool, &str)] = &[
    ("/api/status", false, "status"),
    ("/api/features", false, "features"),
    ("/api/packages", false, "packages"),
    ("/api/instances", false, "instances"),
    ("/api/config", false, "config.current"),
    ("/api/doctor", false, "doctor"),
    ("/api/support-bundle", false, "support.bundle"),
    ("/api/maintenance", false, "maintenance.status"),
    ("/api/update/status", true, "update.status"),
];

/// Change API: path → (socket, method, minimum role).
const WRITES: &[(&str, bool, &str, Role)] = &[
    ("/api/sync", false, "sync.now", Role::Operator),
    ("/api/instances/start", false, "instance.start", Role::Operator),
    ("/api/instances/stop", false, "instance.stop", Role::Operator),
    ("/api/instances/restart", false, "instance.restart", Role::Operator),
    ("/api/maintenance/start", false, "maintenance.start", Role::Operator),
    ("/api/maintenance/stop", false, "maintenance.stop", Role::Operator),
    ("/api/update/check", true, "update.check", Role::Operator),
    ("/api/update/apply", true, "update.apply", Role::Admin),
    ("/api/update/pause", true, "update.pause", Role::Operator),
    ("/api/update/resume", true, "update.resume", Role::Operator),
    ("/api/update/defer", true, "update.defer", Role::Operator),
];

impl App {
    fn session(&self, req: &Request<Incoming>) -> Option<(String, Session)> {
        let c = auth::cookies(req.headers().get(COOKIE).and_then(|v| v.to_str().ok()));
        let tok = c.get(SESSION_COOKIE)?.clone();
        self.sessions.lock().unwrap().get(&tok).map(|s| (tok, s))
    }

    async fn local(&self, updater: bool, method: &str, params: Value) -> Result<Value, (String, String)> {
        let sock = if updater { &self.updater_socket } else { &self.planner_socket };
        crate::local::call(sock, method, params, Duration::from_secs(if method.starts_with("dashboard.") { 15 } else { 10 })).await
    }

    fn query(req: &Request<Incoming>, key: &str) -> Option<String> {
        req.uri().query().unwrap_or_default().split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| {
            // values we accept are hex/base64url tokens: no percent-decoding needed, anything else is rejected
            v.to_string()
        })
    }

    pub async fn handle(self: Arc<Self>, req: Request<Incoming>) -> Resp {
        let mut r = self.route(req).await;
        security_headers(&mut r);
        r
    }

    async fn route(&self, req: Request<Incoming>) -> Resp {
        let path = req.uri().path().to_string();
        match (req.method().clone(), path.as_str()) {
            (Method::GET, "/auth/login") => self.login().await,
            (Method::GET, "/auth/callback") => self.callback(&req).await,
            (Method::GET, "/auth/local") => self.local_login(&req),
            (Method::POST, "/auth/logout") => {
                if let Some((tok, _)) = self.session(&req) {
                    self.sessions.lock().unwrap().remove(&tok);
                }
                let mut r = json_resp(StatusCode::OK, &json!({"ok": true}));
                if let Ok(v) = HeaderValue::from_str(&auth::set_cookie(SESSION_COOKIE, "", 0, self.secure_cookies)) {
                    r.headers_mut().insert(SET_COOKIE, v);
                }
                r
            }
            (Method::GET, "/api/session") => match self.session(&req) {
                Some((_, s)) => json_resp(
                    StatusCode::OK,
                    &json!({"authenticated": true, "user": s.user, "role": s.role.name(), "csrf": s.csrf, "via": s.via,
                            "managerLogin": self.public_url.is_some()}),
                ),
                None => json_resp(StatusCode::OK, &json!({"authenticated": false, "managerLogin": self.public_url.is_some()})),
            },
            // Periodic availability polling excludes the controller credential.
            (Method::GET, "/api/mihomo/panel/status") => self.reply(self.local(false, "mihomo.panel.status", Value::Null).await),
            // Explicitly public native-panel launch. This custom header requires a same-origin browser request;
            // no CORS permission is granted to cross-origin pages. Other control APIs still require a session.
            (Method::POST, "/api/mihomo/panel") => {
                if req.headers().get("x-meduza-panel").and_then(|v| v.to_str().ok()) != Some("open") {
                    return error(StatusCode::FORBIDDEN, "invalid-panel-request", "use the Open Mihomo panel button");
                }
                self.reply(self.local(false, "mihomo.panel", Value::Null).await)
            }
            (m, p) if p.starts_with("/api/") => self.api(m, p, req).await,
            (Method::GET, _) => crate::assets::serve(&self.www, &path, req.headers()),
            _ => error(StatusCode::METHOD_NOT_ALLOWED, "method-not-allowed", "method not allowed"),
        }
    }

    async fn api(&self, method: Method, path: &str, req: Request<Incoming>) -> Resp {
        let Some((_, s)) = self.session(&req) else { return error(StatusCode::UNAUTHORIZED, "unauthorized", "sign in first") };
        if method == Method::GET {
            if path == "/api/logs" {
                let lines = Self::query(&req, "lines").and_then(|v| v.parse::<u64>().ok()).unwrap_or(200).min(500);
                return self.reply(self.local(false, "logs", json!({"lines": lines})).await);
            }
            return match READS.iter().find(|(p, _, _)| *p == path) {
                Some((_, updater, m)) => {
                    let r = self.local(*updater, m, Value::Null).await;
                    if *updater && matches!(&r, Err((c, _)) if c == "unavailable") {
                        // a node without the updater package: say so instead of failing (§13.1)
                        return json_resp(StatusCode::OK, &json!({"available": false}));
                    }
                    self.reply(r)
                }
                None => error(StatusCode::NOT_FOUND, "not-found", "unknown endpoint"),
            };
        }
        if method != Method::POST {
            return error(StatusCode::METHOD_NOT_ALLOWED, "method-not-allowed", "method not allowed");
        }
        let Some((_, updater, m, need)) = WRITES.iter().find(|(p, ..)| *p == path) else {
            return error(StatusCode::NOT_FOUND, "not-found", "unknown endpoint");
        };
        if req.headers().get("x-csrf-token").and_then(|v| v.to_str().ok()) != Some(s.csrf.as_str()) {
            return error(StatusCode::FORBIDDEN, "bad-csrf", "missing or invalid CSRF token");
        }
        if s.role < *need {
            return error(StatusCode::FORBIDDEN, "forbidden", &format!("{} role required", need.name()));
        }
        let body = match Limited::new(req.into_body(), 64 * 1024).collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "too-large", "request body too large"),
        };
        let mut params: Value = if body.is_empty() { json!({}) } else { serde_json::from_slice(&body).unwrap_or(json!({})) };
        if *m == "maintenance.start" {
            // attribute local maintenance to the signed-in user
            let reason = params.get("reason").and_then(Value::as_str).unwrap_or("dashboard").to_string();
            params["reason"] = Value::String(format!("{reason} (by {} via dashboard)", s.user));
        }
        self.reply(self.local(*updater, m, params).await)
    }

    fn reply(&self, r: Result<Value, (String, String)>) -> Resp {
        match r {
            Ok(v) => json_resp(StatusCode::OK, &v),
            Err((code, msg)) => {
                let status = match code.as_str() {
                    "unavailable" => StatusCode::SERVICE_UNAVAILABLE,
                    "forbidden" => StatusCode::FORBIDDEN,
                    "not-found" => StatusCode::NOT_FOUND,
                    "busy" | "maintenance" => StatusCode::CONFLICT,
                    "invalid-params" => StatusCode::BAD_REQUEST,
                    _ => StatusCode::BAD_GATEWAY,
                };
                error(status, &code, &msg)
            }
        }
    }

    async fn login(&self) -> Resp {
        let Some(public) = &self.public_url else {
            return error(
                StatusCode::CONFLICT,
                "manager-login-unconfigured",
                "set MEDUZA_NP_DASHBOARD_PUBLIC_URL to sign in with the manager, or use `meduza-np dashboard-link` on the node",
            );
        };
        let nonce = auth::random_token();
        let r = self
            .local(false, "dashboard.login", json!({"nonceHash": auth::sha256_hex(&nonce), "returnUrl": format!("{public}/auth/callback")}))
            .await;
        match r {
            Ok(v) => {
                let (Some(id), Some(url)) = (v.get("id").and_then(Value::as_str), v.get("authorizeUrl").and_then(Value::as_str)) else {
                    return error(StatusCode::BAD_GATEWAY, "bad-manager-reply", "manager returned no authorize URL");
                };
                redirect(url, &[auth::set_cookie(LOGIN_COOKIE, &format!("{id}.{nonce}"), 300, self.secure_cookies)])
            }
            Err((code, msg)) => error(StatusCode::SERVICE_UNAVAILABLE, &code, &msg),
        }
    }

    async fn callback(&self, req: &Request<Incoming>) -> Resp {
        let c = auth::cookies(req.headers().get(COOKIE).and_then(|v| v.to_str().ok()));
        let pending = c.get(LOGIN_COOKIE).and_then(|v| v.split_once('.')).map(|(a, b)| (a.to_string(), b.to_string()));
        let (Some((id, nonce)), Some(code), Some(state)) = (pending, Self::query(req, "code"), Self::query(req, "state")) else {
            return error(StatusCode::BAD_REQUEST, "invalid-callback", "sign-in was not started from this browser");
        };
        if state != id {
            return error(StatusCode::BAD_REQUEST, "invalid-callback", "sign-in state mismatch");
        }
        let clear = auth::set_cookie(LOGIN_COOKIE, "", 0, self.secure_cookies);
        match self.local(false, "dashboard.exchange", json!({"id": id, "code": code, "nonce": nonce})).await {
            Ok(v) => {
                let user = v.get("user").and_then(Value::as_str).unwrap_or_default().to_string();
                let Some(role) = v.get("role").and_then(Value::as_str).and_then(Role::parse) else {
                    return error(StatusCode::BAD_GATEWAY, "bad-manager-reply", "no role");
                };
                let ttl = v
                    .get("expiresAt")
                    .and_then(Value::as_str)
                    .and_then(iso_to_unix)
                    .map(|t| Duration::from_secs(t.saturating_sub(now_unix())))
                    .unwrap_or(Duration::from_secs(3600));
                let (tok, _) = self.sessions.lock().unwrap().create(user, role, ttl, "manager");
                redirect("/", &[clear, auth::set_cookie(SESSION_COOKIE, &tok, ttl.as_secs(), self.secure_cookies)])
            }
            Err((code, msg)) => {
                let mut r = error(StatusCode::FORBIDDEN, &code, &msg);
                if let Ok(v) = HeaderValue::from_str(&clear) {
                    r.headers_mut().insert(SET_COOKIE, v);
                }
                r
            }
        }
    }

    fn local_login(&self, req: &Request<Incoming>) -> Resp {
        let token = Self::query(req, "token").unwrap_or_default();
        if !auth::consume_local_token(&self.run_dir, &token) {
            return error(StatusCode::FORBIDDEN, "invalid-token", "login link invalid, used or older than 10 minutes");
        }
        let ttl = auth::local_session_ttl();
        let (tok, _) = self.sessions.lock().unwrap().create("local-root".into(), Role::Admin, ttl, "local");
        redirect("/", &[auth::set_cookie(SESSION_COOKIE, &tok, ttl.as_secs(), self.secure_cookies)])
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// RFC 3339 (`2026-10-23T10:00:00.000Z`) → unix seconds, enough for the manager's `toISOString()`.
fn iso_to_unix(s: &str) -> Option<u64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let t = time.trim_end_matches('Z');
    let t = t.split('.').next()?;
    let mut hms = t.split(':').map(|x| x.parse::<i64>().ok());
    let (hh, mm, ss) = (hms.next()??, hms.next()??, hms.next()??);
    // days from civil (Howard Hinnant)
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3600 + mm * 60 + ss).ok()
}

pub fn content_type_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "json" => "application/json",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

pub fn file_response(bytes: Vec<u8>, ctype: &'static str, gz: bool, immutable: bool) -> Resp {
    let mut r = Response::new(Full::new(Bytes::from(bytes)));
    r.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static(ctype));
    if gz {
        r.headers_mut().insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        r.headers_mut().insert("vary", HeaderValue::from_static("Accept-Encoding"));
    }
    r.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static(if immutable { "public, max-age=31536000, immutable" } else { "no-cache" }));
    r
}

pub fn not_found() -> Resp {
    error(StatusCode::NOT_FOUND, "not-found", "not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_times() {
        assert_eq!(iso_to_unix("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(iso_to_unix("2026-10-23T07:32:41.123Z"), Some(1_792_740_761));
        assert_eq!(iso_to_unix("garbage"), None);
    }

    #[test]
    fn api_tables_are_consistent_with_the_local_protocol() {
        use meduza_np_protocol::local::{method_is_mutating, PLANNER_METHODS, UPDATER_METHODS};
        for (_, updater, m) in READS {
            let t = if *updater { UPDATER_METHODS } else { PLANNER_METHODS };
            assert_eq!(method_is_mutating(t, m), Some(false), "{m} must be a read");
        }
        for (_, updater, m, _) in WRITES {
            let t = if *updater { UPDATER_METHODS } else { PLANNER_METHODS };
            assert_eq!(method_is_mutating(t, m), Some(true), "{m} must exist and be mutating");
        }
    }
}
