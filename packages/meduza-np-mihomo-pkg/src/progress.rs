//! Read-only transfer status, separate from the usable provider record. A kernel file lock proves activity
//! across processes and is released even if a failed configuration kills the preparer.
use std::fs::{File, OpenOptions, TryLockError};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::Dirs;

pub const BUSY: &str = "binary download is already running";
pub const PREPARER: &str = "mihomo-pkg/prepare";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadProgress {
    pub phase: String,
    pub active: bool,
    pub target_version: String,
    pub platform: String,
    pub downloaded_bytes: u64,
    pub bytes_total: u64,
    pub attempt: u32,
    pub retry_at_unix: Option<u64>,
    pub error: Option<String>,
    pub updated_at: String,
}

fn save(path: &Path, p: &DownloadProgress) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(p).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    std::fs::rename(tmp, path).map_err(|e| e.to_string())
}

fn released(path: &Path) -> bool {
    match File::open(path) {
        Ok(file) => file.try_lock_shared().is_ok(),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

pub fn preparer(d: &Dirs) -> Result<File, String> {
    std::fs::create_dir_all(&d.state_dir).map_err(|e| e.to_string())?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(d.state_dir.join("prepare.lock"))
        .map_err(|e| e.to_string())?;
    file.try_lock().map_err(|e| e.to_string())?;
    Ok(file)
}

pub fn load(d: &Dirs) -> Option<DownloadProgress> {
    let mut bytes = Vec::new();
    File::open(d.state_dir.join("download-progress.json")).ok()?.take(64 * 1024).read_to_end(&mut bytes).ok()?;
    let mut p: DownloadProgress = serde_json::from_slice(&bytes).ok()?;
    if (p.active && released(&d.state_dir.join("download.lock"))) || (p.phase == "retrying" && released(&d.state_dir.join("prepare.lock")))
    {
        p.active = false;
        p.retry_at_unix = None;
        let ready = crate::load_state(d).is_some_and(|s| {
            s.state == "registered" && s.verified && s.version.as_deref() == Some(&p.target_version) && d.binary().is_file()
        });
        p.phase = if ready { "done" } else { "interrupted" }.into();
        p.error = if ready { None } else { Some("binary preparation stopped before completion".into()) };
    }
    Some(p)
}

pub fn download_lock(d: &Dirs) -> Result<File, String> {
    std::fs::create_dir_all(&d.state_dir).map_err(|e| e.to_string())?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(d.state_dir.join("download.lock"))
        .map_err(|e| e.to_string())?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(TryLockError::WouldBlock) => Err(BUSY.into()),
        Err(TryLockError::Error(e)) => Err(e.to_string()),
    }
}

/// One writer for the complete download/verify/activate operation; readers never write or take a blocking lock.
pub struct Transfer {
    _lock: File,
    file: PathBuf,
    path: PathBuf,
    value: Arc<Mutex<DownloadProgress>>,
    stop: Arc<(Mutex<bool>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

impl Transfer {
    pub fn start(d: &Dirs, file: &Path, version: &str, platform: &str, size: u64) -> Result<Self, String> {
        let lock = download_lock(d)?;
        match std::fs::remove_file(file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
        let attempt = load(d).filter(|p| p.target_version == version && p.phase != "done").map_or(1, |p| p.attempt.saturating_add(1));
        let value = Arc::new(Mutex::new(DownloadProgress {
            phase: "downloading".into(),
            active: true,
            target_version: version.into(),
            platform: platform.into(),
            downloaded_bytes: 0,
            bytes_total: size,
            attempt,
            retry_at_unix: None,
            error: None,
            updated_at: meduza_np_plugin_sdk::now_rfc3339(),
        }));
        let path = d.state_dir.join("download-progress.json");
        save(&path, &value.lock().unwrap())?;
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let source_file = file.to_path_buf();
        let (value2, path2, stop2, file) = (value.clone(), path.clone(), stop.clone(), source_file.clone());
        let worker = std::thread::Builder::new()
            .name("mihomo-progress".into())
            .stack_size(128 * 1024)
            .spawn(move || loop {
                let (lock, cv) = &*stop2;
                let (end, _) = cv.wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(1), |s| !*s).unwrap();
                if *end {
                    break;
                }
                drop(end);
                let mut p = value2.lock().unwrap();
                if p.phase == "downloading" {
                    p.downloaded_bytes = std::fs::metadata(&file).map(|m| m.len().min(p.bytes_total)).unwrap_or(0);
                }
                p.updated_at = meduza_np_plugin_sdk::now_rfc3339();
                let _ = save(&path2, &p);
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { _lock: lock, file: source_file, path, value, stop, worker: Some(worker) })
    }

    pub fn stage(&self, phase: &str) -> Result<(), String> {
        let mut p = self.value.lock().unwrap();
        p.phase = phase.into();
        if let Ok(metadata) = std::fs::metadata(&self.file) {
            p.downloaded_bytes = metadata.len().min(p.bytes_total);
        }
        p.updated_at = meduza_np_plugin_sdk::now_rfc3339();
        save(&self.path, &p)
    }

    pub fn finish(&self, error: Option<&str>) -> Result<(), String> {
        let mut p = self.value.lock().unwrap();
        if let Ok(metadata) = std::fs::metadata(&self.file) {
            p.downloaded_bytes = metadata.len().min(p.bytes_total);
        }
        p.active = false;
        p.phase = if error.is_some() { "failed" } else { "done" }.into();
        p.error = error.map(str::to_string);
        p.updated_at = meduza_np_plugin_sdk::now_rfc3339();
        save(&self.path, &p)
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        let (lock, cv) = &*self.stop;
        *lock.lock().unwrap() = true;
        cv.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn retry(d: &Dirs, delay: Duration, error: &str) {
    let Ok(lock) = OpenOptions::new().read(true).write(true).open(d.state_dir.join("download.lock")) else { return };
    if lock.try_lock().is_err() {
        return;
    }
    if let Some(mut p) = load(d) {
        p.phase = "retrying".into();
        p.active = false;
        p.error = Some(error.into());
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        p.retry_at_unix = Some(now.saturating_add(delay.as_secs()));
        p.updated_at = meduza_np_plugin_sdk::now_rfc3339();
        let _ = save(&d.state_dir.join("download-progress.json"), &p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BinarySourceConfig, Mode};

    #[test]
    fn live_bytes_exclusive_writer_interruption_and_retry() {
        let root = std::env::temp_dir().join(format!("mnp-binary-transfer-{}", std::process::id()));
        let cfg = BinarySourceConfig { mode: Mode::AutoDownload, target_version: Some("v1".into()), data_dir: None, download_proxy: None };
        let d = Dirs::new(root.to_str().unwrap(), &cfg);
        let owner = preparer(&d).unwrap();
        std::fs::create_dir_all(d.staging()).unwrap();
        let file = d.staging().join("asset.gz");
        let transfer = Transfer::start(&d, &file, "v1", "linux/amd64", 1000).unwrap();
        std::fs::write(&file, [0u8; 300]).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while load(&d).unwrap().downloaded_bytes != 300 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let p = load(&d).unwrap();
        assert!(p.active);
        assert_eq!(p.downloaded_bytes, 300);
        assert!(matches!(Transfer::start(&d, &file, "v1", "linux/amd64", 1000), Err(e) if e == BUSY));
        assert_eq!(std::fs::metadata(&file).unwrap().len(), 300, "another request cannot truncate a running download");
        retry(&d, Duration::from_secs(60), "another request failed");
        assert_eq!(load(&d).unwrap().phase, "downloading", "a different request cannot overwrite active progress");
        drop(transfer);
        let p = load(&d).unwrap();
        assert!(!p.active);
        assert_eq!(p.phase, "interrupted", "a released owner lock immediately exposes a stopped preparer");
        retry(&d, Duration::from_secs(60), "TLS EOF");
        assert!(load(&d).unwrap().retry_at_unix.is_some());
        drop(owner);
        assert_eq!(load(&d).unwrap().phase, "interrupted", "stopping during backoff must not leave a phantom retry scheduled");
        assert!(load(&d).unwrap().retry_at_unix.is_none());
        let transfer = Transfer::start(&d, &file, "v1", "linux/amd64", 1000).unwrap();
        assert_eq!(load(&d).unwrap().attempt, 2);
        assert_eq!(load(&d).unwrap().downloaded_bytes, 0, "retries do not inherit the previous partial file");
        std::fs::write(&file, b"123").unwrap();
        transfer.stage("verifying").unwrap();
        assert_eq!(load(&d).unwrap().downloaded_bytes, 3, "a short response must not become a fabricated 100% at validation");
        transfer.finish(Some("TLS EOF")).unwrap();
        drop(transfer);
        assert_eq!(load(&d).unwrap().phase, "failed");
        assert_eq!(load(&d).unwrap().error.as_deref(), Some("TLS EOF"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
