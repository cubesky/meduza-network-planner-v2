//! Private-network, package-gated public diagnostics; only the actual TCP peer is authoritative.

use http_body_util::{BodyExt, Limited};
use hyper::{Method, Request, StatusCode};
use serde_json::{json, Value};
use std::{net::IpAddr, time::Duration};

type Resp = hyper::Response<http_body_util::Full<bytes::Bytes>>;
pub struct App {
    pub planner_socket: std::path::PathBuf,
}

fn json_resp(status: StatusCode, value: &Value) -> Resp {
    hyper::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("cache-control", "no-store")
        .header("x-content-type-options", "nosniff")
        .body(http_body_util::Full::new(bytes::Bytes::from(value.to_string())))
        .unwrap()
}
fn not_found() -> Resp {
    json_resp(StatusCode::NOT_FOUND, &json!({"error":"not-found"}))
}
impl App {
    fn reply(&self, result: Result<Value, (String, String)>) -> Resp {
        match result {
            Ok(v) => json_resp(StatusCode::OK, &v),
            Err((code, message)) => {
                let status = match code.as_str() {
                    "forbidden" => StatusCode::FORBIDDEN,
                    "not-found" => StatusCode::NOT_FOUND,
                    "invalid-params" => StatusCode::BAD_REQUEST,
                    "busy" => StatusCode::TOO_MANY_REQUESTS,
                    "unavailable" => StatusCode::SERVICE_UNAVAILABLE,
                    _ => StatusCode::BAD_GATEWAY,
                };
                json_resp(status, &json!({"error":code,"message":message}))
            }
        }
    }
}

pub async fn serve(listener: tokio::net::TcpListener, app: std::sync::Arc<App>) -> std::io::Result<()> {
    use hyper_util::rt::{TokioIo, TokioTimer};
    let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(8));
    loop {
        let (stream, peer) = listener.accept().await?;
        let Ok(permit) = slots.clone().try_acquire_owned() else { continue };
        let app = app.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let service = hyper::service::service_fn(move |mut req: Request<hyper::body::Incoming>| {
                let app = app.clone();
                req.extensions_mut().insert(peer.ip());
                async move { Ok::<_, std::convert::Infallible>(route(&app, req).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .max_buf_size(16 * 1024)
                .max_headers(32)
                .header_read_timeout(Duration::from_secs(5))
                .keep_alive(false)
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

pub fn run(listen: &str) -> Result<(), Box<dyn std::error::Error>> {
    let address: std::net::SocketAddr = listen.parse()?;
    if address.port() == 0 {
        return Err("LookingGlass requires a nonzero port".into());
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        let paths = meduza_np_runtime::paths::NodePaths::from_env();
        let listener = tokio::net::TcpListener::bind(address).await?;
        meduza_np_runtime::resources::publish_daemon(paths.run.clone(), "meduza-np-lookingglass");
        eprintln!("meduza-np-lookingglass API listening on http://{address}");
        let app = std::sync::Arc::new(App { planner_socket: paths.planner_socket() });
        tokio::select! {
            result = serve(listener, app) => result,
            _ = shutdown() => Ok(()),
        }
    })?;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

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
    if !matches!(req.uri().path(), "/api/ilg/status" | "/api/ilg/query") {
        return not_found();
    }
    if !req.extensions().get::<IpAddr>().copied().is_some_and(private) {
        return app.reply(Err(("forbidden".into(), "LookingGlass requires an RFC1918 or RFC4193 source address".into())));
    }
    let status = crate::local::call(&app.planner_socket, "lookingglass.status", Value::Null, Duration::from_secs(3)).await;
    if !status.is_ok_and(|v| v.get("available").and_then(Value::as_bool) == Some(true)) {
        return app.reply(Err(("not-found".into(), "install and register meduza-np-lookingglass first".into())));
    }
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/api/ilg/status") => app.reply(Ok(json!({"available":true}))),
        (&Method::POST, "/api/ilg/query") => {
            if req.headers().get("x-meduza-lookingglass").and_then(|v| v.to_str().ok()) != Some("query") {
                return app.reply(Err(("forbidden".into(), "set X-Meduza-LookingGlass: query".into())));
            }
            let body = match tokio::time::timeout(Duration::from_secs(3), Limited::new(req.into_body(), 1024).collect()).await {
                Ok(Ok(body)) => body.to_bytes(),
                Ok(Err(_)) => return json_resp(StatusCode::PAYLOAD_TOO_LARGE, &json!({"error":"too-large"})),
                Err(_) => return json_resp(StatusCode::REQUEST_TIMEOUT, &json!({"error":"request-timeout"})),
            };
            let params = match serde_json::from_slice::<Value>(&body) {
                Ok(v) if v.is_object() => v,
                _ => return app.reply(Err(("invalid-params".into(), "expected JSON query".into()))),
            };
            app.reply(crate::local::call(&app.planner_socket, "lookingglass.query", params, Duration::from_secs(28)).await)
        }
        _ => not_found(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn dedicated_tcp_listener_has_no_page_and_cannot_spoof_a_private_peer() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(serve(listener, std::sync::Arc::new(App { planner_socket: "/not-present/planner.sock".into() })));
        for (path, expected) in [("/", "404"), ("/ilg", "404"), ("/api/ilg/status", "403"), ("/api/ilg/query", "403")] {
            let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
            socket
                .write_all(
                    format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-For: 10.0.0.1\r\nConnection: close\r\n\r\n").as_bytes(),
                )
                .await
                .unwrap();
            let mut out = Vec::new();
            socket.read_to_end(&mut out).await.unwrap();
            let text = String::from_utf8(out).unwrap();
            assert!(text.starts_with(&format!("HTTP/1.1 {expected}")), "{text}");
            assert!(text.to_lowercase().contains("cache-control: no-store"));
        }
        task.abort();
        let _ = task.await;
    }
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
    async fn api_requires_private_peer_and_live_package_without_requiring_login_and_has_no_page() {
        use bytes::Bytes;
        use http_body_util::Full;
        use std::sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        };
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let root = std::env::temp_dir().join(format!("mnp-ilg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let app = App { planner_socket: root.join("planner.sock") };
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
        for path in ["/ilg", "/ilg/", "/ilg/example"] {
            for peer in [None, Some("8.8.8.8"), Some("10.42.0.1")] {
                assert_eq!(route(&app, request(path, peer, Method::GET, "", true)).await.status(), StatusCode::NOT_FOUND);
            }
        }
        for path in ["/api/ilg/status", "/api/ilg/query"] {
            for peer in [None, Some("8.8.8.8"), Some("127.0.0.1"), Some("::ffff:8.8.8.8")] {
                assert_eq!(route(&app, request(path, peer, Method::GET, "", true)).await.status(), StatusCode::FORBIDDEN);
            }
            assert_eq!(route(&app, request(path, Some("10.42.0.1"), Method::GET, "", true)).await.status(), StatusCode::NOT_FOUND);
        }
        installed.store(true, Ordering::SeqCst);
        assert_eq!(route(&app, request("/ilg", Some("fd00::1"), Method::GET, "", false)).await.status(), StatusCode::NOT_FOUND);
        assert_eq!(route(&app, request("/api/ilg/status", Some("fd00::1"), Method::GET, "", false)).await.status(), StatusCode::OK);
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
