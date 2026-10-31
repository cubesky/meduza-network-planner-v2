//! Invokes plugin executables with the versioned JSON protocol (§5.1, D2): argv = [operation], stdin = one
//! request, stdout = one response, bounded sizes, timeout + kill, concurrency limit.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use meduza_np_plugin_sdk::{PluginError, PluginRequest, PluginResponse, MAX_REQUEST_BYTES};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Semaphore;

const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_STDERR_BYTES: u64 = 16 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum PluginCallError {
    #[error("plugin could not be started: {0}")]
    Spawn(std::io::Error),
    #[error("plugin timed out after {0:?}")]
    Timeout(Duration),
    #[error("plugin request too large")]
    RequestTooLarge,
    #[error("plugin produced an invalid response: {0}")]
    BadResponse(String),
    #[error("{0}")]
    Plugin(PluginError),
}

impl PluginCallError {
    pub fn code(&self) -> String {
        match self {
            PluginCallError::Plugin(e) => e.code.clone(),
            PluginCallError::Timeout(_) => "plugin-timeout".into(),
            _ => "plugin-failed".into(),
        }
    }
}

#[derive(Clone)]
pub struct PluginRunner {
    slots: Arc<Semaphore>,
    timeout: Duration,
}

impl PluginRunner {
    /// `max_concurrent` keeps probes/plans from exhausting the node (§2.2); control channels never wait on it.
    pub fn new(max_concurrent: usize, timeout: Duration) -> Self {
        Self { slots: Arc::new(Semaphore::new(max_concurrent)), timeout }
    }

    pub async fn call(&self, program: &Path, req: &PluginRequest) -> Result<Value, PluginCallError> {
        let body = serde_json::to_vec(req).map_err(|e| PluginCallError::BadResponse(e.to_string()))?;
        if body.len() > MAX_REQUEST_BYTES {
            return Err(PluginCallError::RequestTooLarge);
        }
        let _permit = self.slots.acquire().await.expect("semaphore never closed");
        let mut child = Command::new(program)
            .arg(&req.operation)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("LANG", "C")
            .kill_on_drop(true)
            .spawn()
            .map_err(PluginCallError::Spawn)?;
        let mut stdin = child.stdin.take().expect("piped");
        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let io = async {
            let write = async {
                stdin.write_all(&body).await?;
                stdin.shutdown().await?;
                drop(stdin);
                Ok::<_, std::io::Error>(())
            };
            let read_out = async {
                let mut out = Vec::new();
                (&mut stdout).take(MAX_RESPONSE_BYTES + 1).read_to_end(&mut out).await?;
                Ok::<_, std::io::Error>(out)
            };
            let read_err = async {
                let mut err = Vec::new();
                (&mut stderr).take(MAX_STDERR_BYTES).read_to_end(&mut err).await?;
                Ok::<_, std::io::Error>(err)
            };
            let (w, out, err) = tokio::join!(write, read_out, read_err);
            // A plugin that exits without reading stdin produces a broken pipe; its stdout still decides.
            let _ = w;
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((status, out?, err.unwrap_or_default()))
        };
        let (status, out, err) = match tokio::time::timeout(self.timeout, io).await {
            Ok(r) => r.map_err(|e| PluginCallError::BadResponse(e.to_string()))?,
            Err(_) => return Err(PluginCallError::Timeout(self.timeout)),
        };
        if !err.is_empty() {
            for line in String::from_utf8_lossy(&err).lines().take(20) {
                debug!("plugin {} {}: {line}", program.display(), req.operation);
            }
        }
        if out.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(PluginCallError::BadResponse("response too large".into()));
        }
        let resp: PluginResponse = serde_json::from_slice(&out).map_err(|e| PluginCallError::BadResponse(format!("exit {status}: {e}")))?;
        if resp.request_id != req.request_id && !resp.request_id.is_empty() {
            return Err(PluginCallError::BadResponse("response for another request".into()));
        }
        match (resp.ok, resp.error) {
            (true, _) => Ok(resp.result),
            (false, Some(e)) => Err(PluginCallError::Plugin(e)),
            (false, None) => Err(PluginCallError::BadResponse("failure without error".into())),
        }
    }
}
