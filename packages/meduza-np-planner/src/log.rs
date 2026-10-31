//! Minimal stderr logger. procd/Podman capture stderr; no logging framework is linked (§16).
//! Messages must never contain secrets; callers log ids and reason codes, not payloads.
//!
//! Sink (D55): `MEDUZA_NP_LOG_TARGET=split` (procd init script) sends INFO/DEBUG to stdout and WARN/ERROR to
//! stderr, so `logread` shows `daemon.info` / `daemon.err`. When stderr is the journal stream systemd passed
//! (`JOURNAL_STREAM`), each line carries a `<N>` priority prefix that journald turns into the entry's priority.
//! Otherwise everything goes to stderr unchanged (Podman, a shell).

use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

static LEVEL: AtomicU8 = AtomicU8::new(2);
static SINK: AtomicU8 = AtomicU8::new(Sink::Stderr as u8);
static RING: Mutex<Option<Ring>> = Mutex::new(None);

/// Bounded in-memory log tail for `logs` / diagnostics (§6.2 "有界日志").
struct Ring {
    lines: std::collections::VecDeque<String>,
    cap: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Sink {
    Stderr = 0,
    Split = 1,
    Journal = 2,
}

pub fn init_from_env() {
    let level = match std::env::var("MEDUZA_NP_LOG").as_deref() {
        Ok("error") => 0,
        Ok("warn") => 1,
        Ok("debug") => 3,
        _ => 2,
    };
    LEVEL.store(level, Ordering::Relaxed);
    let target = std::env::var("MEDUZA_NP_LOG_TARGET").ok();
    let sink = choose_sink(target.as_deref(), stderr_is_journal());
    SINK.store(sink as u8, Ordering::Relaxed);
    *RING.lock().unwrap() = Some(Ring { lines: Default::default(), cap: 500 });
}

/// An explicit target wins; `auto`/unset picks the journal when stderr is systemd's journal stream.
pub fn choose_sink(target: Option<&str>, stderr_is_journal: bool) -> Sink {
    match target {
        Some("split") => Sink::Split,
        Some("stderr") => Sink::Stderr,
        Some("journal") => Sink::Journal,
        _ if stderr_is_journal => Sink::Journal,
        _ => Sink::Stderr,
    }
}

/// systemd sets `JOURNAL_STREAM=<dev>:<inode>` for the stream it connected; a child whose stderr was redirected
/// elsewhere inherits the variable, so it only counts when fd 2 is still that stream.
#[cfg(unix)]
fn stderr_is_journal() -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(value) = std::env::var("JOURNAL_STREAM") else { return false };
    let Some((dev, ino)) = value.split_once(':') else { return false };
    let (Ok(dev), Ok(ino)) = (dev.parse::<u64>(), ino.parse::<u64>()) else { return false };
    std::fs::metadata("/proc/self/fd/2").map(|m| m.dev() == dev && m.ino() == ino).unwrap_or(false)
}

#[cfg(not(unix))]
fn stderr_is_journal() -> bool {
    false
}

pub fn enabled(level: u8) -> bool {
    level <= LEVEL.load(Ordering::Relaxed)
}

/// Stream (`true` = stdout) and line prefix for a level under a sink.
pub fn route(sink: Sink, level: u8) -> (bool, &'static str) {
    match sink {
        Sink::Stderr => (false, ""),
        Sink::Split => (level >= 2, ""),
        // sd-daemon(3): 3 err, 4 warning, 6 info, 7 debug
        Sink::Journal => (false, ["<3>", "<4>", "<6>", "<7>"][level.min(3) as usize]),
    }
}

pub fn emit(level: u8, target: &str, msg: std::fmt::Arguments<'_>) {
    let name = ["ERROR", "WARN", "INFO", "DEBUG"][level.min(3) as usize];
    let line = format!("{} {name} {target}: {msg}", meduza_np_plugin_sdk::now_rfc3339());
    let sink = match SINK.load(Ordering::Relaxed) {
        1 => Sink::Split,
        2 => Sink::Journal,
        _ => Sink::Stderr,
    };
    let (stdout, prefix) = route(sink, level);
    // A closed log pipe must not take the planner down (println!/eprintln! panic on write errors).
    let _ =
        if stdout { writeln!(std::io::stdout().lock(), "{prefix}{line}") } else { writeln!(std::io::stderr().lock(), "{prefix}{line}") };
    if let Ok(mut guard) = RING.lock() {
        if let Some(ring) = guard.as_mut() {
            if ring.lines.len() >= ring.cap {
                ring.lines.pop_front();
            }
            ring.lines.push_back(line);
        }
    }
}

/// Last `n` log lines.
pub fn tail(n: usize) -> Vec<String> {
    RING.lock().ok().and_then(|g| g.as_ref().map(|r| r.lines.iter().rev().take(n).rev().cloned().collect())).unwrap_or_default()
}

#[macro_export]
macro_rules! error { ($($arg:tt)*) => { if $crate::log::enabled(0) { $crate::log::emit(0, module_path!(), format_args!($($arg)*)) } } }
#[macro_export]
macro_rules! warn { ($($arg:tt)*) => { if $crate::log::enabled(1) { $crate::log::emit(1, module_path!(), format_args!($($arg)*)) } } }
#[macro_export]
macro_rules! info { ($($arg:tt)*) => { if $crate::log::enabled(2) { $crate::log::emit(2, module_path!(), format_args!($($arg)*)) } } }
#[macro_export]
macro_rules! debug { ($($arg:tt)*) => { if $crate::log::enabled(3) { $crate::log::emit(3, module_path!(), format_args!($($arg)*)) } } }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sink_choice() {
        assert_eq!(choose_sink(None, false), Sink::Stderr);
        assert_eq!(choose_sink(None, true), Sink::Journal);
        assert_eq!(choose_sink(Some("auto"), true), Sink::Journal);
        assert_eq!(choose_sink(Some("split"), true), Sink::Split);
        assert_eq!(choose_sink(Some("stderr"), true), Sink::Stderr);
        assert_eq!(choose_sink(Some("journal"), false), Sink::Journal);
    }

    #[test]
    fn routing() {
        assert_eq!(route(Sink::Stderr, 2), (false, ""));
        assert_eq!(route(Sink::Split, 0), (false, ""));
        assert_eq!(route(Sink::Split, 1), (false, ""));
        assert_eq!(route(Sink::Split, 2), (true, ""));
        assert_eq!(route(Sink::Split, 3), (true, ""));
        assert_eq!(route(Sink::Journal, 0), (false, "<3>"));
        assert_eq!(route(Sink::Journal, 1), (false, "<4>"));
        assert_eq!(route(Sink::Journal, 3), (false, "<7>"));
    }
}
