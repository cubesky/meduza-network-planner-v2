//! Private-network, package-gated public diagnostics; only the actual TCP peer is authoritative.
use crate::server::{App, Resp};
use http_body_util::{BodyExt, Limited};
use hyper::{Method, Request, StatusCode};
use serde_json::{json, Value};
use std::{net::IpAddr, time::Duration};

fn private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_private(),
        IpAddr::V6(ip) => ip.to_ipv4_mapped().map(|ip| ip.is_private()).unwrap_or(ip.segments()[0] & 0xfe00 == 0xfc00),
    }
}

pub async fn route<B>(app: &App, req: Request<B>) -> Resp
where
    B: hyper::body::Body<Data = bytes::Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    if !req.extensions().get::<IpAddr>().copied().is_some_and(private) {
        return app.reply(Err(("forbidden".into(), "LookingGlass requires an RFC1918 or RFC4193 source address".into())));
    }
    let status = crate::local::call(&app.planner_socket, "lookingglass.status", Value::Null, Duration::from_secs(3)).await;
    if !status.is_ok_and(|v| v.get("available").and_then(Value::as_bool) == Some(true)) {
        return app.reply(Err(("not-found".into(), "install and register meduza-np-lookingglass first".into())));
    }
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/ilg" | "/ilg/") => crate::assets::serve(&app.www, "/", req.headers()),
        (&Method::GET, "/api/ilg/status") => app.reply(Ok(json!({"available":true}))),
        (&Method::POST, "/api/ilg/query") => {
            if req.headers().get("x-meduza-lookingglass").and_then(|v| v.to_str().ok()) != Some("query") {
                return app.reply(Err(("forbidden".into(), "use the LookingGlass page".into())));
            }
            let body = match tokio::time::timeout(Duration::from_secs(3), Limited::new(req.into_body(), 1024).collect()).await {
                Ok(Ok(body)) => body.to_bytes(),
                Ok(Err(_)) => return crate::server::json_resp(StatusCode::PAYLOAD_TOO_LARGE, &json!({"error":"too-large"})),
                Err(_) => return crate::server::json_resp(StatusCode::REQUEST_TIMEOUT, &json!({"error":"request-timeout"})),
            };
            let params = match serde_json::from_slice::<Value>(&body) {
                Ok(v) if v.is_object() => v,
                _ => return app.reply(Err(("invalid-params".into(), "expected JSON query".into()))),
            };
            app.reply(crate::local::call(&app.planner_socket, "lookingglass.query", params, Duration::from_secs(28)).await)
        }
        _ => crate::server::not_found(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_acl_uses_exact_private_ranges_including_mapped_v4() {
        for ip in ["10.0.0.1", "172.16.0.1", "172.31.255.254", "192.168.1.1", "fc00::1", "fd12::1", "::ffff:10.0.0.1"] {
            assert!(private(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "127.0.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "172.15.0.1",
            "172.32.0.1",
            "192.0.2.1",
            "::1",
            "fe80::1",
            "2001:db8::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!private(ip.parse().unwrap()), "{ip}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn page_and_api_require_private_peer_and_live_package_without_requiring_login() {
        use bytes::Bytes;
        use http_body_util::Full;
        use std::sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc, Mutex,
        };
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let root = std::env::temp_dir().join(format!("mnp-ilg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("index.html"), "<html>LookingGlass UI</html>").unwrap();
        let app = App {
            www: root.clone(),
            planner_socket: root.join("planner.sock"),
            updater_socket: root.join("updater.sock"),
            run_dir: root.clone(),
            public_url: None,
            secure_cookies: false,
            sessions: Mutex::new(crate::auth::Sessions::default()),
        };
        let socket = tokio::net::UnixListener::bind(&app.planner_socket).unwrap();
        let installed = Arc::new(AtomicBool::new(false));
        let queries = Arc::new(AtomicUsize::new(0));
        let installed2 = installed.clone();
        let queries2 = queries.clone();
        let mock = tokio::spawn(async move {
            loop {
                let (stream, _) = socket.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                let req: meduza_np_protocol::local::LocalRequest = serde_json::from_str(&line).unwrap();
                let result = if req.method == "lookingglass.status" {
                    json!({"available":installed2.load(Ordering::SeqCst)})
                } else {
                    assert_eq!(req.method, "lookingglass.query");
                    queries2.fetch_add(1, Ordering::SeqCst);
                    json!({"results":[],"target":req.params["target"]})
                };
                let mut response = serde_json::to_vec(&meduza_np_protocol::local::LocalResponse::ok(req.id, result)).unwrap();
                response.push(b'\n');
                stream.get_mut().write_all(&response).await.unwrap();
            }
        });
        let request = |path: &str, peer: Option<&str>, method: Method, body: &str, header: bool| {
            let mut req = Request::builder()
                .method(method)
                .uri(path)
                .header("x-forwarded-for", "10.0.0.1")
                .header("forwarded", "for=10.0.0.1")
                .body(Full::new(Bytes::copy_from_slice(body.as_bytes())))
                .unwrap();
            if header {
                req.headers_mut().insert("x-meduza-lookingglass", "query".parse().unwrap());
            }
            if let Some(ip) = peer {
                req.extensions_mut().insert(ip.parse::<IpAddr>().unwrap());
            }
            req
        };
        for path in ["/ilg", "/ilg/", "/api/ilg/status", "/api/ilg/query"] {
            for peer in [None, Some("8.8.8.8"), Some("127.0.0.1"), Some("::ffff:8.8.8.8")] {
                assert_eq!(route(&app, request(path, peer, Method::GET, "", true)).await.status(), StatusCode::FORBIDDEN);
            }
            assert_eq!(route(&app, request(path, Some("10.42.0.1"), Method::GET, "", true)).await.status(), StatusCode::NOT_FOUND);
        }
        installed.store(true, Ordering::SeqCst);
        assert_eq!(route(&app, request("/ilg", Some("fd00::1"), Method::GET, "", false)).await.status(), StatusCode::OK);
        assert_eq!(route(&app, request("/api/ilg/status", Some("10.0.0.1"), Method::GET, "", false)).await.status(), StatusCode::OK);
        assert_eq!(
            route(&app, request("/api/ilg/query", Some("10.0.0.1"), Method::POST, "{}", false)).await.status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            route(&app, request("/api/ilg/query", Some("10.0.0.1"), Method::POST, &"x".repeat(1025), true)).await.status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            route(&app, request("/api/ilg/query", Some("10.0.0.1"), Method::POST, "[]", true)).await.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            route(&app, request("/api/ilg/query", Some("10.0.0.1"), Method::POST, r#"{"operation":"ping","target":"10.0.0.1"}"#, true))
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(queries.load(Ordering::SeqCst), 1);
        installed.store(false, Ordering::SeqCst);
        assert_eq!(
            route(&app, request("/api/ilg/query", Some("10.0.0.1"), Method::POST, "{}", true)).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(queries.load(Ordering::SeqCst), 1);
        mock.abort();
        let _ = mock.await;
        let _ = std::fs::remove_dir_all(root);
    }
}
