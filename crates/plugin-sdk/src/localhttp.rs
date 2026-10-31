//! Minimal HTTP/1.1 client for local controller APIs (Mihomo/OpenClash on loopback): bounded connect/read
//! timeouts and response size, bearer secret in a header (never in the URL), chunked bodies decoded.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// Sends one request and returns (status code, body).
pub fn call(addr: &str, secret: &str, method: &str, path: &str, body: Option<&str>) -> Result<(u16, String), String> {
    let sa: SocketAddr = addr.parse().map_err(|_| "bad API address".to_string())?;
    let mut s = TcpStream::connect_timeout(&sa, Duration::from_secs(2)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
    let body = body.unwrap_or("");
    let auth = if secret.is_empty() { String::new() } else { format!("Authorization: Bearer {secret}\r\n") };
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    let _ = s.take(4 * 1024 * 1024).read_to_end(&mut buf);
    Ok(parse_response(&String::from_utf8_lossy(&buf)))
}

pub fn parse_response(text: &str) -> (u16, String) {
    let code = text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text, ""));
    if !head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        return (code, body.to_string());
    }
    let mut out = String::new();
    let mut rest = body;
    while let Some((size, after)) = rest.split_once("\r\n") {
        let Ok(n) = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16) else { break };
        if n == 0 || after.len() < n {
            out.push_str(&after[..n.min(after.len())]);
            break;
        }
        out.push_str(&after[..n]);
        rest = after[n..].strip_prefix("\r\n").unwrap_or(&after[n..]);
    }
    (code, out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn chunked_and_plain() {
        let (c, b) = super::parse_response("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n2\r\n1}\r\n0\r\n\r\n");
        assert_eq!((c, b.as_str()), (200, "{\"a\":1}"));
        let (c, b) = super::parse_response("HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\n\r\n{}");
        assert_eq!((c, b.as_str()), (401, "{}"));
    }
}
