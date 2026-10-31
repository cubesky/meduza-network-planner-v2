//! Outbound connections to the manager: TCP + TLS (rustls, system roots + optional CA file), HTTP/1.1 with a
//! tiny keep-alive pool, and WebSocket upgrade over the same stream type. No retries happen here: every attempt
//! is accounted for by the shared backoff gate one layer up (§7.5.2).

#[cfg(unix)]
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub type BoxIo = Box<dyn Io>;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("tls: {0}")]
    Tls(String),
    #[error("certificate not trusted: {0}")]
    Trust(String),
    #[error("timeout")]
    Timeout,
    #[error("http: {0}")]
    Http(String),
    #[error("response too large")]
    TooLarge,
    #[error("invalid url: {0}")]
    Url(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub tls: bool,
    pub host: String,
    pub port: u16,
}

impl Endpoint {
    /// Parses `https://host[:port]` / `http://host[:port]` (IPv6 hosts in brackets).
    pub fn parse(base: &str) -> Result<Self, TransportError> {
        let (tls, rest) = if let Some(r) = base.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = base.strip_prefix("http://") {
            (false, r)
        } else {
            return Err(TransportError::Url(base.into()));
        };
        let authority = rest.split('/').next().unwrap_or_default();
        let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
            let (h, after) = stripped.split_once(']').ok_or_else(|| TransportError::Url(base.into()))?;
            (h.to_string(), after.strip_prefix(':').map(|p| p.parse()).transpose().map_err(|_| TransportError::Url(base.into()))?)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), Some(p.parse().map_err(|_| TransportError::Url(base.into()))?)),
                None => (authority.to_string(), None),
            }
        };
        if host.is_empty() {
            return Err(TransportError::Url(base.into()));
        }
        Ok(Self { tls, port: port.unwrap_or(if tls { 443 } else { 80 }), host })
    }

    pub fn authority(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if (self.tls && self.port == 443) || (!self.tls && self.port == 80) {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

pub struct Transport {
    endpoint: Endpoint,
    #[cfg(unix)]
    tls: Option<Arc<rustls::ClientConfig>>,
    connect_timeout: Duration,
    request_timeout: Duration,
    idle: Mutex<Vec<SendRequest<Full<Bytes>>>>,
}

impl Transport {
    pub fn new(base: &str, ca_file: Option<&str>) -> Result<Self, TransportError> {
        let endpoint = Endpoint::parse(base)?;
        #[cfg(unix)]
        let tls = if endpoint.tls { Some(Arc::new(tls_config(ca_file)?)) } else { None };
        #[cfg(not(unix))]
        {
            let _ = ca_file;
            if endpoint.tls {
                return Err(TransportError::Tls(
                    "TLS is only built for Linux targets; use http:// with allowInsecure for development".into(),
                ));
            }
        }
        Ok(Self {
            endpoint,
            #[cfg(unix)]
            tls,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(20),
            idle: Mutex::new(Vec::new()),
        })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub async fn connect(&self) -> Result<BoxIo, TransportError> {
        let addr = format!("{}:{}", self.endpoint.host, self.endpoint.port);
        let tcp = tokio::time::timeout(self.connect_timeout, TcpStream::connect(&addr))
            .await
            .map_err(|_| TransportError::Timeout)?
            .map_err(|e| TransportError::Connect(e.to_string()))?;
        let _ = tcp.set_nodelay(true);
        #[cfg(unix)]
        if let Some(cfg) = &self.tls {
            let name =
                rustls::pki_types::ServerName::try_from(self.endpoint.host.clone()).map_err(|e| TransportError::Tls(e.to_string()))?;
            let connector = tokio_rustls::TlsConnector::from(cfg.clone());
            let stream = tokio::time::timeout(self.connect_timeout, connector.connect(name, tcp))
                .await
                .map_err(|_| TransportError::Timeout)?
                .map_err(classify_tls)?;
            return Ok(Box::new(stream));
        }
        Ok(Box::new(tcp))
    }

    async fn sender(&self) -> Result<SendRequest<Full<Bytes>>, TransportError> {
        loop {
            let cached = self.idle.lock().unwrap().pop();
            match cached {
                Some(s) if !s.is_closed() => return Ok(s),
                Some(_) => continue,
                None => break,
            }
        }
        let io = self.connect().await?;
        let (sender, conn) =
            hyper::client::conn::http1::handshake(TokioIo::new(io)).await.map_err(|e| TransportError::Http(e.to_string()))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        Ok(sender)
    }

    /// One HTTP request. `max_body` bounds the response size.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
        body: Option<Vec<u8>>,
        max_body: usize,
    ) -> Result<HttpResponse, TransportError> {
        let fut = async {
            let mut sender = self.sender().await?;
            sender.ready().await.map_err(|e| TransportError::Http(e.to_string()))?;
            let mut req = hyper::Request::builder()
                .method(method)
                .uri(path)
                .header("host", self.endpoint.authority())
                .header("user-agent", concat!("meduza-np-planner/", env!("CARGO_PKG_VERSION")));
            for (k, v) in headers {
                req = req.header(*k, v);
            }
            let has_body = body.is_some();
            if has_body {
                req = req.header("content-type", "application/json");
            }
            let req = req.body(Full::new(Bytes::from(body.unwrap_or_default()))).map_err(|e| TransportError::Http(e.to_string()))?;
            let resp = sender.send_request(req).await.map_err(|e| TransportError::Http(e.to_string()))?;
            let status = resp.status().as_u16();
            let headers = resp.headers().iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string())).collect();
            let body = Limited::new(resp.into_body(), max_body).collect().await.map_err(|e| {
                if e.to_string().contains("length limit") {
                    TransportError::TooLarge
                } else {
                    TransportError::Http(e.to_string())
                }
            })?;
            if !sender.is_closed() {
                let mut idle = self.idle.lock().unwrap();
                if idle.len() < 2 {
                    idle.push(sender);
                }
            }
            Ok(HttpResponse { status, headers, body: body.to_bytes() })
        };
        tokio::time::timeout(self.request_timeout, fut).await.map_err(|_| TransportError::Timeout)?
    }

    /// Drops pooled connections (after errors or credential changes).
    pub fn reset(&self) {
        self.idle.lock().unwrap().clear();
    }

    /// Opens the push WebSocket with the given bearer token.
    pub async fn websocket(
        &self,
        path: &str,
        bearer: &str,
    ) -> Result<tokio_tungstenite::WebSocketStream<BoxIo>, (TransportError, Option<u16>, Option<String>)> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let scheme = if self.endpoint.tls { "wss" } else { "ws" };
        let url = format!("{scheme}://{}{path}", self.endpoint.authority());
        let mut req = url.into_client_request().map_err(|e| (TransportError::Url(e.to_string()), None, None))?;
        req.headers_mut().insert(
            "authorization",
            format!("Bearer {bearer}").parse().map_err(|_| (TransportError::Http("bad token".into()), None, None))?,
        );
        let io = self.connect().await.map_err(|e| (e, None, None))?;
        let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(256 * 1024))
            .max_frame_size(Some(64 * 1024));
        match tokio::time::timeout(self.connect_timeout, tokio_tungstenite::client_async_with_config(req, io, Some(config))).await {
            Err(_) => Err((TransportError::Timeout, None, None)),
            Ok(Ok((ws, _))) => Ok(ws),
            Ok(Err(tokio_tungstenite::tungstenite::Error::Http(resp))) => {
                let retry = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(str::to_string);
                Err((TransportError::Http(format!("upgrade refused with {}", resp.status())), Some(resp.status().as_u16()), retry))
            }
            Ok(Err(e)) => Err((TransportError::Http(e.to_string()), None, None)),
        }
    }
}

#[cfg(unix)]
fn classify_tls(e: std::io::Error) -> TransportError {
    let msg = e.to_string();
    if msg.contains("certificate") || msg.contains("UnknownIssuer") || msg.contains("NotValidForName") {
        TransportError::Trust(msg)
    } else {
        TransportError::Tls(msg)
    }
}

#[cfg(unix)]
fn tls_config(ca_file: Option<&str>) -> Result<rustls::ClientConfig, TransportError> {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        let _ = roots.add(cert);
    }
    if let Some(path) = ca_file {
        let pem = std::fs::read(path).map_err(|e| TransportError::Tls(format!("{path}: {e}")))?;
        for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
            let cert = cert.map_err(|e| TransportError::Tls(format!("{path}: {e}")))?;
            roots.add(cert).map_err(|e| TransportError::Tls(format!("{path}: {e}")))?;
        }
    }
    if roots.is_empty() {
        return Err(TransportError::Tls("no trusted CA certificates found (install ca-bundle or set caFile)".into()));
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| TransportError::Tls(e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints() {
        assert_eq!(Endpoint::parse("https://m.example.test").unwrap(), Endpoint { tls: true, host: "m.example.test".into(), port: 443 });
        assert_eq!(Endpoint::parse("http://127.0.0.1:8787/").unwrap().authority(), "127.0.0.1:8787");
        assert_eq!(Endpoint::parse("https://[fd00::1]:8443").unwrap().host, "fd00::1");
        assert!(Endpoint::parse("ftp://x").is_err());
    }
}
