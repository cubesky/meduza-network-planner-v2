//! Crash-safe file helpers (§11.3): write to a temp file in the same directory, fsync, rename, fsync dir.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Atomically replaces `path` with `content`, creating it with `mode` (unix permission bits).
pub fn write_atomic(path: &Path, content: &[u8], mode: u32) -> io::Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = temp_sibling(path);
    let result = (|| {
        let mut f = open_new(&tmp, mode)?;
        f.write_all(content)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        sync_dir(dir)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Serializes `value` as pretty JSON and writes it atomically.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T, mode: u32) -> io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    write_atomic(path, &bytes, mode)
}

/// Reads JSON; `Ok(None)` if the file does not exist. Parse errors are errors — never "empty" (§9.10).
pub fn read_json<T: DeserializeOwned>(path: &Path, max_bytes: u64) -> io::Result<Option<T>> {
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut buf = Vec::new();
    f.take(max_bytes + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > max_bytes {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} exceeds {max_bytes} bytes", path.display())));
    }
    serde_json::from_slice(&buf).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))
}

fn temp_sibling(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut rnd = [0u8; 6];
    let _ = getrandom::getrandom(&mut rnd);
    let suffix: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
    path.with_file_name(format!(".{name}.tmp-{suffix}"))
}

#[cfg(unix)]
fn open_new(path: &Path, mode: u32) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new().write(true).create_new(true).mode(mode).open(path)
}

#[cfg(not(unix))]
fn open_new(path: &Path, _mode: u32) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(unix)]
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
pub fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Creates a directory (and parents) and sets its mode on unix.
pub fn ensure_dir(path: &Path, mode: u32) -> io::Result<()> {
    fs::create_dir_all(path)?;
    set_mode(path, mode)
}

#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
pub fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// Free bytes available to unprivileged writers on the filesystem holding `path`.
#[cfg(unix)]
pub fn free_space(path: &Path) -> io::Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid NUL-terminated path and a properly sized out-parameter.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(not(unix))]
pub fn free_space(_path: &Path) -> io::Result<u64> {
    Ok(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_and_read_json() {
        let dir = std::env::temp_dir().join(format!("mnp-fsutil-{}", std::process::id()));
        let path = dir.join("state.json");
        write_json_atomic(&path, &serde_json::json!({"a": 1}), 0o600).unwrap();
        write_json_atomic(&path, &serde_json::json!({"a": 2}), 0o600).unwrap();
        let v: serde_json::Value = read_json(&path, 1024).unwrap().unwrap();
        assert_eq!(v["a"], 2);
        assert!(read_json::<serde_json::Value>(&dir.join("missing.json"), 1024).unwrap().is_none());
        fs::write(&path, b"{broken").unwrap();
        assert!(read_json::<serde_json::Value>(&path, 1024).is_err(), "corrupt is an error, not empty");
        let leftovers: Vec<_> =
            fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().contains(".tmp-")).collect();
        assert!(leftovers.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
}
