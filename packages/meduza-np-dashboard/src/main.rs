//! `meduza-np-dashboard` — local node dashboard (AGENT.md §12.3). Serves the prebuilt Vite + Preact UI and a fixed
//! local API over planner/updater sockets. No Node.js on the node.
//!
//! Environment:
//!   MEDUZA_NP_DASHBOARD_LISTEN       default 127.0.0.1:8443 (loopback only unless changed)
//!   MEDUZA_NP_DASHBOARD_PUBLIC_URL   e.g. https://gw-a.example.net:8443 — enables sign-in with the manager identity
//!   MEDUZA_NP_DASHBOARD_TLS_CERT/_KEY  PEM files; required when listening on a non-loopback address
//!   MEDUZA_NP_DASHBOARD_WWW          default /usr/share/meduza-np/dashboard/www
//!   MEDUZA_NP_RUN_DIR                sockets and break-glass login tokens (default /run/meduza-np)

mod assets;
mod auth;
mod local;
mod server;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use hyper::service::service_fn;
use hyper_util::rt::TokioIo;

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("meduza-np-dashboard: {msg}");
    std::process::exit(1)
}

fn main() {
    if matches!(std::env::args().nth(1).as_deref(), Some("--version" | "version")) {
        println!("meduza-np-dashboard {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap_or_else(|e| fail(e));
    rt.block_on(run());
}

async fn run() {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let paths = meduza_np_runtime::paths::NodePaths::from_env();
    let listen: SocketAddr = env("MEDUZA_NP_DASHBOARD_LISTEN")
        .unwrap_or_else(|| "127.0.0.1:8443".into())
        .parse()
        .unwrap_or_else(|e| fail(format!("MEDUZA_NP_DASHBOARD_LISTEN: {e}")));
    let tls_files = (env("MEDUZA_NP_DASHBOARD_TLS_CERT"), env("MEDUZA_NP_DASHBOARD_TLS_KEY"));
    let tls = match tls_files {
        (Some(c), Some(k)) => Some(tls::acceptor(&c, &k).unwrap_or_else(|e| fail(e))),
        (None, None) => None,
        _ => fail("set both MEDUZA_NP_DASHBOARD_TLS_CERT and MEDUZA_NP_DASHBOARD_TLS_KEY"),
    };
    if !listen.ip().is_loopback() && tls.is_none() {
        fail("refusing to serve the dashboard on a non-loopback address without TLS");
    }
    let public_url = env("MEDUZA_NP_DASHBOARD_PUBLIC_URL").map(|u| u.trim_end_matches('/').to_string());
    if let Some(u) = &public_url {
        if !(u.starts_with("https://") || u.starts_with("http://127.0.0.1") || u.starts_with("http://localhost")) {
            fail("MEDUZA_NP_DASHBOARD_PUBLIC_URL must be https (http only for loopback)");
        }
    }
    let app = Arc::new(server::App {
        www: env("MEDUZA_NP_DASHBOARD_WWW").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/usr/share/meduza-np/dashboard/www")),
        planner_socket: paths.planner_socket(),
        updater_socket: paths.updater_socket(),
        run_dir: paths.run.clone(),
        secure_cookies: tls.is_some() || public_url.as_deref().map(|u| u.starts_with("https://")).unwrap_or(false),
        public_url,
        sessions: Mutex::new(auth::Sessions::default()),
    });
    let listener = tokio::net::TcpListener::bind(listen).await.unwrap_or_else(|e| fail(format!("bind {listen}: {e}")));
    eprintln!("meduza-np-dashboard listening on {}://{listen}", if tls.is_some() { "https" } else { "http" });
    serve(listener, app, tls, shutdown_signal()).await;
}

/// Accept loop (bounded concurrency: a dashboard is for a handful of operators).
async fn serve(
    listener: tokio::net::TcpListener,
    app: Arc<server::App>,
    tls: Option<tls::Acceptor>,
    shutdown: impl std::future::Future<Output = ()>,
) {
    tokio::pin!(shutdown);
    let slots = Arc::new(tokio::sync::Semaphore::new(32));
    loop {
        let (stream, _) = tokio::select! {
            r = listener.accept() => match r { Ok(x) => x, Err(_) => continue },
            _ = &mut shutdown => break,
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else { continue };
        let app = app.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let svc = service_fn(move |req| {
                let app = app.clone();
                async move { Ok::<_, std::convert::Infallible>(app.handle(req).await) }
            });
            // header_read_timeout needs a timer, otherwise hyper panics on the first connection
            let conn = hyper::server::conn::http1::Builder::new()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(std::time::Duration::from_secs(15))
                .to_owned();
            match tls {
                Some(acc) => {
                    if let Ok(Ok(s)) = tokio::time::timeout(std::time::Duration::from_secs(10), tls::accept(&acc, stream)).await {
                        let _ = conn.serve_connection(TokioIo::new(s), svc).await;
                    }
                }
                None => {
                    let _ = conn.serve_connection(TokioIo::new(stream), svc).await;
                }
            }
        });
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("signal handler");
        tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(unix)]
mod tls {
    use std::sync::Arc;

    pub type Acceptor = tokio_rustls::TlsAcceptor;

    pub fn acceptor(cert: &str, key: &str) -> Result<Acceptor, String> {
        let certs: Vec<_> =
            rustls_pemfile::certs(&mut std::io::BufReader::new(std::fs::File::open(cert).map_err(|e| format!("{cert}: {e}"))?))
                .collect::<Result<_, _>>()
                .map_err(|e| format!("{cert}: {e}"))?;
        let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(std::fs::File::open(key).map_err(|e| format!("{key}: {e}"))?))
            .map_err(|e| format!("{key}: {e}"))?
            .ok_or_else(|| format!("{key}: no private key"))?;
        let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| e.to_string())?;
        Ok(tokio_rustls::TlsAcceptor::from(Arc::new(cfg)))
    }

    pub async fn accept(
        acc: &Acceptor,
        s: tokio::net::TcpStream,
    ) -> std::io::Result<tokio_rustls::server::TlsStream<tokio::net::TcpStream>> {
        acc.accept(s).await
    }
}

#[cfg(not(unix))]
mod tls {
    #[derive(Clone)]
    pub struct Acceptor;
    pub fn acceptor(_cert: &str, _key: &str) -> Result<Acceptor, String> {
        Err("TLS is only built for Unix targets".into())
    }
    pub async fn accept(_acc: &Acceptor, s: tokio::net::TcpStream) -> std::io::Result<tokio::net::TcpStream> {
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn request(addr: SocketAddr, raw: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(raw.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        String::from_utf8_lossy(&out).into_owned()
    }

    #[tokio::test]
    async fn serves_session_api_assets_and_local_login_over_real_sockets() {
        let root = std::env::temp_dir().join(format!("mnp-dash-http-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("www/assets")).unwrap();
        std::fs::write(root.join("www/index.html"), "<html>ui</html>").unwrap();
        std::fs::write(root.join("www/assets/app-1.js"), "x").unwrap();
        let app = Arc::new(server::App {
            www: root.join("www"),
            planner_socket: root.join("run/planner.sock"),
            updater_socket: root.join("run/updater.sock"),
            run_dir: root.join("run"),
            public_url: None,
            secure_cookies: false,
            sessions: Mutex::new(auth::Sessions::default()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(serve(listener, app, None, async move {
            let _ = stop_rx.await;
        }));
        let get = |path: &str| {
            format!(
                "GET {path} HTTP/1.1
Host: x
Connection: close

"
            )
        };

        let r = request(addr, &get("/api/session")).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(r.contains("\"authenticated\":false") && r.to_ascii_lowercase().contains("content-security-policy"), "{r}");
        assert!(request(addr, &get("/api/status")).await.starts_with("HTTP/1.1 401"));
        let index = request(addr, &get("/")).await;
        assert!(index.contains("<html>ui</html>") && index.to_ascii_lowercase().contains("cache-control: no-cache"), "{index}");
        assert!(request(addr, &get("/instances")).await.contains("<html>ui</html>"), "hash-route fallback");
        assert!(request(addr, &get("/assets/app-1.js")).await.contains("immutable"));
        assert!(request(addr, &get("/assets/../index.html")).await.starts_with("HTTP/1.1 404"));

        // break-glass link → admin session cookie; a second use is refused
        let tok = auth::random_token();
        std::fs::create_dir_all(auth::local_token_dir(&root.join("run"))).unwrap();
        std::fs::write(auth::local_token_dir(&root.join("run")).join(auth::sha256_hex(&tok)), b"").unwrap();
        let r = request(addr, &get(&format!("/auth/local?token={tok}"))).await;
        assert!(r.starts_with("HTTP/1.1 302"), "{r}");
        let cookie = r.lines().find(|l| l.to_ascii_lowercase().starts_with("set-cookie: mnp_dash=")).unwrap();
        let value = cookie.split_once(':').unwrap().1.trim().split(';').next().unwrap().to_string();
        assert!(request(addr, &get(&format!("/auth/local?token={tok}"))).await.starts_with("HTTP/1.1 403"));
        let with_cookie = |path: &str, method: &str, extra: &str| {
            format!(
                "{method} {path} HTTP/1.1
Host: x
Cookie: {value}
{extra}Content-Length: 0
Connection: close

"
            )
        };
        let s = request(addr, &with_cookie("/api/session", "GET", "")).await;
        assert!(s.contains("\"role\":\"admin\""), "{s}");
        // writes need the CSRF token; planner is not running here, so with the token the call reaches the socket layer
        assert!(request(addr, &with_cookie("/api/sync", "POST", "")).await.starts_with("HTTP/1.1 403"));
        let csrf = s.split("\"csrf\":\"").nth(1).unwrap().split('"').next().unwrap().to_string();
        let r = request(
            addr,
            &with_cookie(
                "/api/sync",
                "POST",
                &format!(
                    "X-CSRF-Token: {csrf}
"
                ),
            ),
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 503"), "planner unavailable is reported, not hidden: {r}");
        // a node without the updater: explicit "not available"
        assert!(request(addr, &with_cookie("/api/update/status", "GET", "")).await.contains("\"available\":false"));

        let _ = stop_tx.send(());
        let _ = task.await;
        let _ = std::fs::remove_dir_all(&root);
    }
}
