//! Async client of the local control protocol (`planner.sock`, `updater.sock`): one request per connection.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

/// Returns the result, or `(code, message)`; `unavailable` when the socket cannot be reached.
#[cfg(unix)]
pub async fn call(socket: &Path, method: &str, params: Value, timeout: Duration) -> Result<Value, (String, String)> {
    use meduza_np_protocol::local::{LocalRequest, LocalResponse, LOCAL_PROTOCOL_VERSION, MAX_FRAME};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let unavailable = |e: String| ("unavailable".to_string(), e);
    let work = async {
        let mut s = tokio::net::UnixStream::connect(socket).await.map_err(|e| unavailable(format!("{}: {e}", socket.display())))?;
        let req = LocalRequest { protocol: LOCAL_PROTOCOL_VERSION, id: 1, method: method.into(), params };
        let mut line = serde_json::to_vec(&req).map_err(|e| unavailable(e.to_string()))?;
        line.push(b'\n');
        s.write_all(&line).await.map_err(|e| unavailable(e.to_string()))?;
        let (r, _w) = s.split();
        let mut buf = String::new();
        BufReader::new(r.take(MAX_FRAME as u64 * 4)).read_line(&mut buf).await.map_err(|e| unavailable(e.to_string()))?;
        let resp: LocalResponse = serde_json::from_str(&buf).map_err(|e| unavailable(format!("bad response: {e}")))?;
        match (resp.ok, resp.error) {
            (true, _) => Ok(resp.result),
            (false, Some(e)) => Err((e.code, e.message)),
            (false, None) => Err(("internal".into(), "error without detail".into())),
        }
    };
    tokio::time::timeout(timeout, work).await.unwrap_or_else(|_| Err(("unavailable".into(), format!("{method}: timed out"))))
}

#[cfg(not(unix))]
pub async fn call(_socket: &Path, _method: &str, _params: Value, _timeout: Duration) -> Result<Value, (String, String)> {
    Err(("unavailable".into(), "local sockets need a Unix host".into()))
}
