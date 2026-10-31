//! Built-in process supervisor (§11.2): process groups, restart policy with backoff, start-failure limit,
//! graceful stop with deadline, status for reports. No external supervisor is required.
//!
//! Process adoption (D32): a supervisor with a state directory records every started process (pid, kernel start
//! time, boot id, binary) and sends its stderr to a log file there that planner forwards. A planned planner restart
//! (`detach_all`) leaves the daemons running; the next planner adopts a recorded process when its launch spec is
//! unchanged and the process is provably the same one, so a planner-only upgrade does not restart tunnels.
//!
//! A process bound to another one (`bound_to`, e.g. FRR daemons to zebra) is restarted whenever that process starts
//! a new process, and is not adopted when it was started anew (D33).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use meduza_np_protocol::plan::RestartPolicy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub id: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub restart: RestartPolicy,
    pub stop_timeout: Duration,
    /// Changing digest ⇒ restart; equal ⇒ keep the running process (§11.1).
    pub restart_digest: String,
    /// Restart whenever this process (id) starts a new process; never adopt after it was started anew.
    pub bound_to: Option<String>,
}

impl LaunchSpec {
    /// Identity of everything that defines the process; an adopted process must have been started with it.
    fn digest(&self) -> String {
        let v = serde_json::json!({
            "id": self.id,
            "program": self.program.to_string_lossy(),
            "args": self.args,
            "env": self.env,
            "restart": self.restart,
            "stopTimeout": self.stop_timeout.as_secs(),
            "restartDigest": self.restart_digest,
        });
        // Only bound specs carry the field, so records of unbound processes stay adoptable across this change.
        let mut v = v;
        if let Some(b) = &self.bound_to {
            v["boundTo"] = b.as_str().into();
        }
        hex(&Sha256::digest(v.to_string().as_bytes()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProcState {
    Starting,
    Running,
    Backoff,
    /// Gave up after repeated quick failures; needs a new apply or manual restart.
    Failed,
    Exited,
    Stopped,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcStatus {
    pub id: String,
    pub state: ProcState,
    pub pid: Option<u32>,
    pub restarts: u32,
    pub last_exit: Option<String>,
    /// The running process was started by a previous planner and adopted by this one.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub adopted: bool,
    #[serde(skip)]
    pub running_since: Option<Instant>,
}

enum Control {
    Stop(oneshot::Sender<()>),
    /// Forget the process but leave it running (planned planner restart); its record stays for adoption.
    Detach(oneshot::Sender<()>),
    Restart,
}

struct Entry {
    spec: LaunchSpec,
    control: mpsc::Sender<Control>,
}

/// What `run` supervises: our own child, or a recorded process started by a previous planner.
enum Proc {
    Child(Child),
    Adopted(Record),
}

/// Persisted per supervised process in the state directory (`<sha>.json`, stderr in `<sha>.log`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    id: String,
    pid: u32,
    /// Kernel start time (clock ticks since boot, /proc/<pid>/stat field 22): a reused pid never matches.
    start_ticks: u64,
    boot_id: String,
    /// Canonical program path when started; a switched symlink view or replaced binary means "not the same".
    program: String,
    /// `<dev>:<inode>` of that program file when started. Checked when /proc/<pid>/exe cannot be read (daemons
    /// that drop privileges, e.g. FRR as user frr, without CAP_SYS_PTRACE for planner).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    program_file: Option<String>,
    spec: String,
}

#[derive(Clone)]
pub struct Supervisor {
    entries: Arc<Mutex<BTreeMap<String, Entry>>>,
    status: Arc<Mutex<BTreeMap<String, ProcStatus>>>,
    /// Records + logs for adoption; `None` (tests, non-Linux) keeps processes strictly tied to this planner.
    state_dir: Option<PathBuf>,
    /// Ids of processes that just started a new process (not adoptions), for `bound_to` dependents.
    starts: broadcast::Sender<String>,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self { entries: Default::default(), status: Default::default(), state_dir: None, starts: broadcast::channel(64).0 }
    }
}

const QUICK_FAILURE: Duration = Duration::from_secs(10);
const MAX_QUICK_FAILURES: u32 = 5;
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const ADOPTED_POLL: Duration = Duration::from_millis(500);
const LOG_POLL: Duration = Duration::from_millis(500);
/// The forwarded log file is truncated once this much has been forwarded (bounded tmpfs use on OpenWrt).
const LOG_TRUNCATE_AT: u64 = 64 * 1024;
const LOG_MAX_LINE: usize = 4096;

impl Supervisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Supervisor that records its processes in `dir` so a later planner can adopt them (Linux only; elsewhere
    /// this is the same as `new`).
    pub fn persistent(dir: &Path) -> Self {
        if !cfg!(target_os = "linux") {
            return Self::new();
        }
        if let Err(e) = meduza_np_runtime::fsutil::ensure_dir(dir, 0o700) {
            warn!("cannot create {}: {e}; supervised processes will not survive planner restarts", dir.display());
            return Self::new();
        }
        Self { state_dir: Some(dir.to_path_buf()), ..Self::default() }
    }

    /// Starts `spec`, or restarts it if the running spec differs. Identical running specs are left alone. Without a
    /// running entry, a recorded process with the same spec is adopted instead of starting a second copy.
    pub async fn ensure(&self, spec: LaunchSpec) {
        let same = {
            let entries = self.entries.lock().unwrap();
            let status = self.status.lock().unwrap();
            entries.get(&spec.id).map(|e| e.spec == spec).unwrap_or(false)
                && status.get(&spec.id).map(|s| s.state != ProcState::Failed && s.state != ProcState::Stopped).unwrap_or(false)
        };
        if same {
            return;
        }
        let had_entry = self.entries.lock().unwrap().contains_key(&spec.id);
        // `stop` without an entry would stop the recorded process instead of letting it be adopted.
        let adopt = if had_entry {
            self.stop(&spec.id).await;
            None
        } else {
            self.claim_record(&spec).await
        };
        self.spawn_entry(spec, adopt);
    }

    fn spawn_entry(&self, spec: LaunchSpec, adopt: Option<Record>) {
        let (tx, rx) = mpsc::channel(4);
        let id = spec.id.clone();
        self.status.lock().unwrap().insert(
            id.clone(),
            ProcStatus {
                id: id.clone(),
                state: ProcState::Starting,
                pid: None,
                restarts: 0,
                last_exit: None,
                // Set here, not by the task: a dependent ensured right after must see whether this one is new.
                adopted: adopt.is_some(),
                running_since: None,
            },
        );
        self.entries.lock().unwrap().insert(id, Entry { spec: spec.clone(), control: tx });
        let status = self.status.clone();
        // Subscribed before the task runs: a start of the target from now on restarts this process.
        let bound = spec.bound_to.as_ref().map(|_| self.starts.subscribe());
        tokio::spawn(run(spec, rx, status, self.state_dir.clone(), adopt, self.starts.clone(), bound));
    }

    /// Stops and forgets the process (graceful, bounded by its stop timeout). A recorded process of a previous
    /// planner that was never adopted is stopped too.
    pub async fn stop(&self, id: &str) {
        let entry = self.entries.lock().unwrap().remove(id);
        if let Some(entry) = entry {
            let (tx, rx) = oneshot::channel();
            if entry.control.send(Control::Stop(tx)).await.is_ok() {
                let _ = tokio::time::timeout(entry.spec.stop_timeout + Duration::from_secs(5), rx).await;
            }
        } else if let Some(dir) = &self.state_dir {
            if let Some(rec) = load_record(dir, id) {
                if record_alive(&rec) {
                    info!("{id}: stopping pid {} left by a previous planner", rec.pid);
                    terminate_adopted(&rec, Duration::from_secs(10)).await;
                }
                remove_record(dir, id, true);
            }
        }
        self.status.lock().unwrap().remove(id);
    }

    pub async fn restart(&self, id: &str) -> bool {
        let ctl = self.entries.lock().unwrap().get(id).map(|e| e.control.clone());
        match ctl {
            Some(c) => c.send(Control::Restart).await.is_ok(),
            None => false,
        }
    }

    /// Sends a signal (e.g. SIGHUP for reload) to the process group.
    pub fn signal(&self, id: &str, signal: &str) -> bool {
        let pid = self.status.lock().unwrap().get(id).and_then(|s| s.pid);
        match pid {
            Some(pid) => send_signal(pid, signal),
            None => false,
        }
    }

    pub fn ids(&self) -> BTreeSet<String> {
        self.entries.lock().unwrap().keys().cloned().collect()
    }

    pub fn statuses(&self) -> Vec<ProcStatus> {
        self.status.lock().unwrap().values().cloned().collect()
    }

    pub fn status(&self, id: &str) -> Option<ProcStatus> {
        self.status.lock().unwrap().get(id).cloned()
    }

    /// True if the process is running and has stayed up for `settle` (startup verification).
    pub async fn wait_settled(&self, id: &str, settle: Duration, deadline: Duration) -> bool {
        let start = Instant::now();
        loop {
            if let Some(s) = self.status(id) {
                match s.state {
                    ProcState::Running if s.running_since.map(|t| t.elapsed() >= settle).unwrap_or(false) => return true,
                    ProcState::Failed | ProcState::Stopped => return false,
                    _ => {}
                }
            } else {
                return false;
            }
            if start.elapsed() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn stop_all(&self) {
        let ids: Vec<String> = self.ids().into_iter().collect();
        for id in ids {
            self.stop(&id).await;
        }
    }

    /// Planned planner restart: leave every running process alone so the next planner can adopt it. Without a
    /// state directory nothing could be adopted, so this stops them like `stop_all`.
    pub async fn detach_all(&self) {
        if self.state_dir.is_none() {
            return self.stop_all().await;
        }
        let entries: Vec<(String, mpsc::Sender<Control>)> =
            std::mem::take(&mut *self.entries.lock().unwrap()).into_iter().map(|(id, e)| (id, e.control)).collect();
        for (id, ctl) in entries {
            let (tx, rx) = oneshot::channel();
            if ctl.send(Control::Detach(tx)).await.is_ok() {
                let _ = tokio::time::timeout(Duration::from_secs(5), rx).await;
            }
            self.status.lock().unwrap().remove(&id);
        }
    }

    /// Process groups of recorded processes that are still alive (their pidfiles are valid, not stale).
    pub fn live_recorded_pids(&self) -> BTreeSet<u32> {
        let Some(dir) = &self.state_dir else { return BTreeSet::new() };
        all_records(dir).into_iter().filter(record_alive).map(|r| r.pid).collect()
    }

    /// After the startup reconcile: stop recorded processes nothing claimed (instance removed or changed while
    /// planner was down) and drop their records.
    pub async fn reap_unclaimed(&self) {
        let Some(dir) = &self.state_dir else { return };
        let claimed = self.ids();
        for rec in all_records(dir) {
            if claimed.contains(&rec.id) {
                continue;
            }
            if record_alive(&rec) {
                warn!("{}: pid {} from a previous planner is no longer part of the configuration; stopping it", rec.id, rec.pid);
                terminate_adopted(&rec, Duration::from_secs(10)).await;
            }
            remove_record(dir, &rec.id, true);
        }
    }

    /// Takes the record of `spec.id` if its process can be adopted; otherwise stops whatever it names and drops it.
    async fn claim_record(&self, spec: &LaunchSpec) -> Option<Record> {
        let dir = self.state_dir.as_ref()?;
        let rec = load_record(dir, &spec.id)?;
        if !record_alive(&rec) {
            remove_record(dir, &spec.id, false);
            return None;
        }
        let program = canonical(&spec.program);
        let target_restarted = spec.bound_to.as_ref().and_then(|b| self.status(b)).is_some_and(|s| !s.adopted);
        let why = if rec.spec != spec.digest() {
            Some("its launch spec changed")
        } else if rec.program != program {
            Some("its program was switched")
        } else if binary_replaced(proc_exe(rec.pid).as_deref(), rec.program_file.as_deref(), file_id(Path::new(&program)).as_deref()) {
            // Replaced in place (package manager rename): the old image is still running.
            Some("its binary was replaced")
        } else if target_restarted {
            // Its state belongs to the previous instance of the process it depends on.
            Some("the process it is bound to was started anew")
        } else {
            None
        };
        match why {
            None => Some(rec),
            Some(why) => {
                info!("{}: not adopting pid {} ({why}); restarting it", spec.id, rec.pid);
                terminate_adopted(&rec, spec.stop_timeout).await;
                remove_record(dir, &spec.id, false);
                None
            }
        }
    }
}

fn set_state(status: &Arc<Mutex<BTreeMap<String, ProcStatus>>>, id: &str, f: impl FnOnce(&mut ProcStatus)) {
    if let Some(s) = status.lock().unwrap().get_mut(id) {
        f(s);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn record_stem(id: &str) -> String {
    hex(&Sha256::digest(id.as_bytes())[..12])
}

fn record_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{}.json", record_stem(id)))
}

fn log_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{}.log", record_stem(id)))
}

fn load_record(dir: &Path, id: &str) -> Option<Record> {
    let rec: Record = meduza_np_runtime::fsutil::read_json(&record_path(dir, id), 64 * 1024).ok().flatten()?;
    (rec.id == id).then_some(rec)
}

fn all_records(dir: &Path) -> Vec<Record> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = e.path();
        if path.extension().is_some_and(|x| x == "json") {
            if let Ok(Some(rec)) = meduza_np_runtime::fsutil::read_json::<Record>(&path, 64 * 1024) {
                out.push(rec);
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    out
}

fn remove_record(dir: &Path, id: &str, with_log: bool) {
    let _ = std::fs::remove_file(record_path(dir, id));
    if with_log {
        let _ = std::fs::remove_file(log_path(dir, id));
    }
}

fn canonical(program: &Path) -> String {
    std::fs::canonicalize(program).unwrap_or_else(|_| program.to_path_buf()).to_string_lossy().into_owned()
}

/// `<dev>:<inode>` of a file: a package manager replaces binaries by rename, which gives a new inode.
fn file_id(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|m| format!("{}:{}", m.dev(), m.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Whether a recorded process no longer runs the program file it was started from. `exe` is the
/// /proc/<pid>/exe link (unreadable for a daemon that dropped privileges), `recorded`/`current` the program file
/// identity at start and now. Without any evidence the process is not adopted.
fn binary_replaced(exe: Option<&str>, recorded: Option<&str>, current: Option<&str>) -> bool {
    let file_changed = recorded.is_some_and(|r| current != Some(r));
    match exe {
        Some(exe) => exe.ends_with(" (deleted)") || file_changed,
        None => recorded.is_none() || file_changed,
    }
}

#[cfg(target_os = "linux")]
mod procfs {
    pub struct Stat {
        pub state: char,
        pub pgrp: i64,
        pub start_ticks: u64,
    }

    pub fn stat(pid: u32) -> Option<Stat> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // comm may contain spaces and parentheses; the fields after the last ')' are fixed.
        let rest = &text[text.rfind(')')? + 1..];
        let f: Vec<&str> = rest.split_whitespace().collect();
        // f[0] = field 3 (state), f[2] = field 5 (pgrp), f[19] = field 22 (starttime).
        Some(Stat { state: f.first()?.chars().next()?, pgrp: f.get(2)?.parse().ok()?, start_ticks: f.get(19)?.parse().ok()? })
    }

    pub fn boot_id() -> String {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map(|s| s.trim().to_string()).unwrap_or_default()
    }

    pub fn exe(pid: u32) -> Option<String> {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok().map(|p| p.to_string_lossy().into_owned())
    }

    /// How long the process has been running.
    pub fn age(start_ticks: u64) -> Option<std::time::Duration> {
        let uptime: f64 = std::fs::read_to_string("/proc/uptime").ok()?.split_whitespace().next()?.parse().ok()?;
        // SAFETY: sysconf has no preconditions.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if hz <= 0 {
            return None;
        }
        let started = start_ticks as f64 / hz as f64;
        (uptime >= started).then(|| std::time::Duration::from_secs_f64(uptime - started))
    }
}

/// The recorded process still exists, is the same process (pid + start time + boot) and still leads its group.
#[cfg(target_os = "linux")]
fn record_alive(rec: &Record) -> bool {
    if rec.boot_id.is_empty() || rec.boot_id != procfs::boot_id() {
        return false;
    }
    match procfs::stat(rec.pid) {
        Some(s) => s.start_ticks == rec.start_ticks && s.pgrp == i64::from(rec.pid) && !matches!(s.state, 'Z' | 'X' | 'x'),
        None => false,
    }
}

#[cfg(not(target_os = "linux"))]
fn record_alive(_rec: &Record) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn proc_exe(pid: u32) -> Option<String> {
    procfs::exe(pid)
}

#[cfg(not(target_os = "linux"))]
fn proc_exe(_pid: u32) -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn new_record(spec: &LaunchSpec, pid: u32) -> Option<Record> {
    let stat = procfs::stat(pid)?;
    Some(Record {
        id: spec.id.clone(),
        pid,
        start_ticks: stat.start_ticks,
        boot_id: procfs::boot_id(),
        program: canonical(&spec.program),
        program_file: file_id(Path::new(&canonical(&spec.program))),
        spec: spec.digest(),
    })
}

#[cfg(not(target_os = "linux"))]
fn new_record(_spec: &LaunchSpec, _pid: u32) -> Option<Record> {
    None
}

fn adopted_since(rec: &Record) -> Instant {
    #[cfg(target_os = "linux")]
    if let Some(age) = procfs::age(rec.start_ticks) {
        return Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
    }
    let _ = rec;
    Instant::now()
}

fn spawn_child(spec: &LaunchSpec, log: Option<&Path>) -> std::io::Result<Child> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args).stdin(Stdio::null()).stdout(Stdio::null()).env_clear();
    match log {
        // A log file instead of planner's stderr: a pipe from procd/systemd would break (SIGPIPE) when planner
        // restarts and the adopted daemon writes its next line.
        Some(path) => {
            let mut o = std::fs::OpenOptions::new();
            o.create(true).append(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
            cmd.stderr(Stdio::from(o.open(path)?));
        }
        None => {
            cmd.stderr(Stdio::inherit());
        }
    }
    cmd.env("PATH", std::env::var_os("PATH").unwrap_or_default());
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    #[cfg(unix)]
    cmd.process_group(0);
    // Detached processes must survive dropping their handle.
    cmd.kill_on_drop(log.is_none());
    cmd.spawn()
}

async fn terminate(child: &mut Child, timeout: Duration) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: plain kill(2) on our own process group.
        unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
        if tokio::time::timeout(timeout, child.wait()).await.is_ok() {
            return;
        }
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    let _ = timeout;
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// Stops what is left of a process group whose leader exited on its own (crash, SIGKILL): its children are not
/// signalled by the kernel and would keep running next to the restarted process, still holding ports and files.
/// Returns whether anything was left. The kernel does not reuse a pid while a group of that id has members, and
/// this runs right after the leader was reaped.
async fn stop_orphaned_group(pgid: u32, timeout: Duration) -> bool {
    #[cfg(unix)]
    {
        let pg = -(pgid as i32);
        // SAFETY: plain kill(2) on the group our child led; ESRCH when it is empty.
        if unsafe { libc::kill(pg, libc::SIGTERM) } != 0 {
            return false;
        }
        let deadline = Instant::now() + timeout;
        while unsafe { libc::kill(pg, 0) } == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        unsafe { libc::kill(pg, libc::SIGKILL) };
        true
    }
    #[cfg(not(unix))]
    {
        let _ = (pgid, timeout);
        false
    }
}

/// Graceful stop of a recorded process group that is not our child (no wait(2); polls its identity).
async fn terminate_adopted(rec: &Record, timeout: Duration) {
    #[cfg(unix)]
    {
        if !record_alive(rec) {
            return;
        }
        // SAFETY: the group leader was verified to be the recorded process just above.
        unsafe { libc::kill(-(rec.pid as i32), libc::SIGTERM) };
        let deadline = Instant::now() + timeout;
        while record_alive(rec) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if record_alive(rec) {
            unsafe { libc::kill(-(rec.pid as i32), libc::SIGKILL) };
            let deadline = Instant::now() + Duration::from_secs(2);
            while record_alive(rec) && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = (rec, timeout);
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: &str) -> bool {
    let sig = match signal {
        "SIGHUP" | "HUP" => libc::SIGHUP,
        "SIGUSR1" | "USR1" => libc::SIGUSR1,
        "SIGUSR2" | "USR2" => libc::SIGUSR2,
        "SIGTERM" | "TERM" => libc::SIGTERM,
        _ => return false,
    };
    // SAFETY: signal to our own child's process group.
    unsafe { libc::kill(-(pid as i32), sig) == 0 }
}

#[cfg(not(unix))]
fn send_signal(_pid: u32, _signal: &str) -> bool {
    false
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TailEnd {
    Running,
    /// Forward the rest, then empty the file (the next planner forwards only what is written after).
    Detach,
    /// Forward the rest, then delete the file.
    Remove,
}

/// Forwards the daemon's stderr log file to planner's stderr, prefixed with the process id.
async fn forward_log(id: String, path: PathBuf, mut end: watch::Receiver<TailEnd>) {
    use std::io::{Read, Seek, SeekFrom};
    let mut offset = 0u64;
    let mut partial: Vec<u8> = Vec::new();
    loop {
        let finish = *end.borrow();
        if let Ok(mut f) = std::fs::File::open(&path) {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len < offset {
                offset = 0; // truncated by someone else
            }
            let mut buf = Vec::new();
            if f.seek(SeekFrom::Start(offset)).is_ok() && f.take(1 << 20).read_to_end(&mut buf).is_ok() {
                offset += buf.len() as u64;
                partial.extend_from_slice(&buf);
                while let Some(nl) = partial.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = partial.drain(..=nl).collect();
                    eprintln!("{id}| {}", String::from_utf8_lossy(&line[..line.len() - 1]));
                }
                if partial.len() > LOG_MAX_LINE {
                    eprintln!("{id}| {}", String::from_utf8_lossy(&partial));
                    partial.clear();
                }
            }
            if offset >= LOG_TRUNCATE_AT && finish == TailEnd::Running {
                if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&path) {
                    if f.set_len(0).is_ok() {
                        offset = 0;
                    }
                }
            }
        }
        match finish {
            TailEnd::Running => {}
            TailEnd::Detach => {
                if !partial.is_empty() {
                    eprintln!("{id}| {}", String::from_utf8_lossy(&partial));
                }
                if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&path) {
                    let _ = f.set_len(0);
                }
                return;
            }
            TailEnd::Remove => {
                if !partial.is_empty() {
                    eprintln!("{id}| {}", String::from_utf8_lossy(&partial));
                }
                let _ = std::fs::remove_file(&path);
                return;
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(LOG_POLL) => {}
            r = end.changed() => if r.is_err() { return },
        }
    }
}

/// Resolves when the process `target` started a new process (a missed event counts as one); never without a target.
async fn bound_restart(rx: &mut Option<broadcast::Receiver<String>>, target: Option<&str>) -> String {
    let (Some(rx), Some(target)) = (rx.as_mut(), target) else { return std::future::pending().await };
    loop {
        match rx.recv().await {
            Ok(id) if id == target => return id,
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => return target.to_string(),
            Err(broadcast::error::RecvError::Closed) => return std::future::pending().await,
        }
    }
}

/// Waits until the supervised process exits; returns the exit text and whether it counts as success.
async fn wait_exit(proc: &mut Proc) -> (String, bool) {
    match proc {
        Proc::Child(child) => match child.wait().await {
            Ok(st) => (st.to_string(), st.success()),
            Err(e) => (e.to_string(), false),
        },
        Proc::Adopted(rec) => {
            while record_alive(rec) {
                tokio::time::sleep(ADOPTED_POLL).await;
            }
            // Not our child: the exit status went to its new parent (init/subreaper).
            ("exited (status unknown: adopted process)".into(), false)
        }
    }
}

async fn run(
    spec: LaunchSpec,
    mut rx: mpsc::Receiver<Control>,
    status: Arc<Mutex<BTreeMap<String, ProcStatus>>>,
    state_dir: Option<PathBuf>,
    mut adopt: Option<Record>,
    starts: broadcast::Sender<String>,
    mut bound: Option<broadcast::Receiver<String>>,
) {
    let id = spec.id.clone();
    let log = state_dir.as_ref().map(|d| log_path(d, &id));
    let (tail_tx, tail_rx) = watch::channel(TailEnd::Running);
    if let Some(path) = &log {
        tokio::spawn(forward_log(id.clone(), path.clone(), tail_rx));
    }
    let finish = |end: TailEnd| {
        let _ = tail_tx.send(end);
        if end == TailEnd::Remove {
            if let Some(dir) = &state_dir {
                remove_record(dir, &id, false);
            }
        }
    };
    let mut quick_failures = 0u32;
    let mut backoff = Duration::from_secs(1);
    loop {
        set_state(&status, &id, |s| s.state = ProcState::Starting);
        let mut proc = if let Some(rec) = adopt.take() {
            let since = adopted_since(&rec);
            let pid = rec.pid;
            set_state(&status, &id, |s| {
                s.state = ProcState::Running;
                s.pid = Some(pid);
                s.adopted = true;
                s.running_since = Some(since);
            });
            info!("{id}: adopted running pid {pid} from the previous planner");
            Proc::Adopted(rec)
        } else {
            match spawn_child(&spec, log.as_deref()) {
                Ok(c) => {
                    let pid = c.id();
                    if let (Some(dir), Some(pid)) = (&state_dir, pid) {
                        match new_record(&spec, pid) {
                            Some(rec) => {
                                if let Err(e) = meduza_np_runtime::fsutil::write_json_atomic(&record_path(dir, &id), &rec, 0o600) {
                                    warn!("{id}: cannot record pid {pid} for adoption: {e}");
                                }
                            }
                            None => remove_record(dir, &id, false),
                        }
                    }
                    set_state(&status, &id, |s| {
                        s.state = ProcState::Running;
                        s.pid = pid;
                        s.adopted = false;
                        s.running_since = Some(Instant::now());
                    });
                    info!("{id}: started pid {}", pid.unwrap_or(0));
                    let _ = starts.send(id.clone());
                    Proc::Child(c)
                }
                Err(e) => {
                    warn!("{id}: cannot start {}: {e}", spec.program.display());
                    set_state(&status, &id, |s| {
                        s.state = ProcState::Failed;
                        s.last_exit = Some(format!("spawn failed: {e}"));
                    });
                    // Wait for stop/restart instructions; nothing to supervise.
                    match rx.recv().await {
                        Some(Control::Stop(ack)) | Some(Control::Detach(ack)) => {
                            finish(TailEnd::Remove);
                            let _ = ack.send(());
                            return;
                        }
                        Some(Control::Restart) => continue,
                        None => return,
                    }
                }
            }
        };
        let started = Instant::now();
        // Taken before waiting: a reaped child no longer reports its pid.
        let pgid = match &proc {
            Proc::Child(c) => c.id(),
            Proc::Adopted(rec) => Some(rec.pid),
        };
        // Starts of the target before this process was (re)started are already accounted for.
        if let Some(b) = bound.as_mut() {
            while !matches!(b.try_recv(), Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed)) {}
        }
        tokio::select! {
            (text, success) = wait_exit(&mut proc) => {
                let ran = started.elapsed();
                warn!("{id}: exited ({text}) after {}s", ran.as_secs());
                if let Some(pgid) = pgid {
                    if stop_orphaned_group(pgid, spec.stop_timeout).await {
                        warn!("{id}: stopped the processes it left behind (group {pgid})");
                    }
                }
                if ran < QUICK_FAILURE { quick_failures += 1 } else { quick_failures = 0; backoff = Duration::from_secs(1) }
                let restart = match spec.restart {
                    RestartPolicy::Never => false,
                    RestartPolicy::Always => true,
                    RestartPolicy::OnFailure => !success,
                };
                if let Some(dir) = &state_dir { remove_record(dir, &id, false) }
                if !restart {
                    set_state(&status, &id, |s| { s.state = ProcState::Exited; s.pid = None; s.last_exit = Some(text); });
                    match rx.recv().await {
                        Some(Control::Stop(ack)) | Some(Control::Detach(ack)) => { finish(TailEnd::Remove); let _ = ack.send(()); return; }
                        Some(Control::Restart) => continue,
                        None => return,
                    }
                }
                if quick_failures >= MAX_QUICK_FAILURES {
                    error!("{id}: {quick_failures} quick failures, giving up until the next apply or manual restart");
                    set_state(&status, &id, |s| { s.state = ProcState::Failed; s.pid = None; s.last_exit = Some(text); });
                    match rx.recv().await {
                        Some(Control::Stop(ack)) | Some(Control::Detach(ack)) => { finish(TailEnd::Remove); let _ = ack.send(()); return; }
                        Some(Control::Restart) => { quick_failures = 0; continue }
                        None => return,
                    }
                }
                set_state(&status, &id, |s| { s.state = ProcState::Backoff; s.pid = None; s.restarts += 1; s.last_exit = Some(text); });
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    ctl = rx.recv() => match ctl {
                        Some(Control::Stop(ack)) | Some(Control::Detach(ack)) => { finish(TailEnd::Remove); let _ = ack.send(()); return; }
                        Some(Control::Restart) => { backoff = Duration::from_secs(1); continue }
                        None => return,
                    }
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            target = bound_restart(&mut bound, spec.bound_to.as_deref()) => {
                info!("{id}: {target} started a new process; restarting with it");
                match &mut proc {
                    Proc::Child(child) => terminate(child, spec.stop_timeout).await,
                    Proc::Adopted(rec) => terminate_adopted(rec, spec.stop_timeout).await,
                }
                set_state(&status, &id, |s| { s.restarts += 1; s.adopted = false; });
                quick_failures = 0;
                continue;
            }
            ctl = rx.recv() => {
                if let Some(Control::Detach(ack)) = ctl {
                    // Leave it running; the record stays for the next planner.
                    finish(TailEnd::Detach);
                    info!("{id}: left running for the next planner (pid {})", status.lock().unwrap().get(&id).and_then(|s| s.pid).unwrap_or(0));
                    let _ = ack.send(());
                    return;
                }
                match &mut proc {
                    Proc::Child(child) => terminate(child, spec.stop_timeout).await,
                    Proc::Adopted(rec) => terminate_adopted(rec, spec.stop_timeout).await,
                }
                match ctl {
                    Some(Control::Stop(ack)) => {
                        set_state(&status, &id, |s| { s.state = ProcState::Stopped; s.pid = None; });
                        finish(TailEnd::Remove);
                        let _ = ack.send(());
                        info!("{id}: stopped");
                        return;
                    }
                    Some(Control::Restart) => {
                        set_state(&status, &id, |s| { s.restarts += 1; s.adopted = false; });
                        quick_failures = 0;
                        continue;
                    }
                    Some(Control::Detach(_)) | None => return,
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sleeper(id: &str, secs: &str, digest: &str) -> LaunchSpec {
        LaunchSpec {
            id: id.into(),
            program: "/bin/sleep".into(),
            args: vec![secs.into()],
            env: Default::default(),
            restart: RestartPolicy::OnFailure,
            stop_timeout: Duration::from_secs(2),
            restart_digest: digest.into(),
            bound_to: None,
        }
    }

    #[tokio::test]
    async fn start_keep_restart_stop() {
        let sup = Supervisor::new();
        sup.ensure(sleeper("t/a", "30", "d1")).await;
        assert!(sup.wait_settled("t/a", Duration::from_millis(200), Duration::from_secs(3)).await);
        let pid = sup.status("t/a").unwrap().pid;
        sup.ensure(sleeper("t/a", "30", "d1")).await;
        assert_eq!(sup.status("t/a").unwrap().pid, pid, "same spec keeps the process");
        sup.ensure(sleeper("t/a", "30", "d2")).await;
        assert!(sup.wait_settled("t/a", Duration::from_millis(100), Duration::from_secs(3)).await);
        assert_ne!(sup.status("t/a").unwrap().pid, pid, "changed digest restarts");
        sup.stop("t/a").await;
        assert!(sup.status("t/a").is_none());
    }

    #[tokio::test]
    async fn quick_failures_give_up() {
        let sup = Supervisor::new();
        let mut spec = sleeper("t/f", "x", "d");
        spec.program = "/bin/false".into();
        sup.ensure(spec).await;
        tokio::time::sleep(Duration::from_secs(1) * 20).await;
        assert_eq!(sup.status("t/f").unwrap().state, ProcState::Failed);
    }

    #[cfg(target_os = "linux")]
    fn alive(pid: u32) -> bool {
        procfs::stat(pid).map(|s| !matches!(s.state, 'Z' | 'X')).unwrap_or(false)
    }

    /// A planned restart leaves the process running; a new supervisor adopts it with the same spec, restarts it
    /// for a changed one, and stops unclaimed ones.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn detach_adopt_and_reap() {
        let dir = std::env::temp_dir().join(format!("meduza-np-sup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = Supervisor::persistent(&dir);
        first.ensure(sleeper("t/keep", "60", "d")).await;
        first.ensure(sleeper("t/change", "60", "d")).await;
        first.ensure(sleeper("t/gone", "60", "d")).await;
        for id in ["t/keep", "t/change", "t/gone"] {
            assert!(first.wait_settled(id, Duration::from_millis(100), Duration::from_secs(3)).await);
        }
        let pid = |s: &Supervisor, id: &str| s.status(id).unwrap().pid.unwrap();
        let (keep, change, gone) = (pid(&first, "t/keep"), pid(&first, "t/change"), pid(&first, "t/gone"));
        first.detach_all().await;
        assert!(first.ids().is_empty());
        assert!(alive(keep) && alive(change) && alive(gone), "detach leaves processes running");

        let second = Supervisor::persistent(&dir);
        assert_eq!(second.live_recorded_pids(), [keep, change, gone].into_iter().collect());
        second.ensure(sleeper("t/keep", "60", "d")).await;
        assert!(second.wait_settled("t/keep", Duration::from_millis(100), Duration::from_secs(3)).await);
        let st = second.status("t/keep").unwrap();
        assert_eq!((st.pid, st.adopted), (Some(keep), true), "same spec is adopted");
        second.ensure(sleeper("t/change", "60", "d2")).await;
        assert!(second.wait_settled("t/change", Duration::from_millis(100), Duration::from_secs(3)).await);
        assert_ne!(pid(&second, "t/change"), change, "changed spec restarts");
        assert!(!alive(change));
        second.reap_unclaimed().await;
        assert!(!alive(gone), "unclaimed recorded process is stopped");

        // An adopted process that exits is restarted by the policy like a child.
        unsafe { libc::kill(keep as i32, libc::SIGKILL) };
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(second.wait_settled("t/keep", Duration::from_millis(100), Duration::from_secs(5)).await);
        let st = second.status("t/keep").unwrap();
        assert!(st.pid != Some(keep) && !st.adopted && st.restarts == 1);

        second.stop_all().await;
        assert!(second.live_recorded_pids().is_empty());
        assert!(all_records(&dir).is_empty(), "stop removes records");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A leader that dies on its own (here: a wrapper whose daemon child keeps running) takes its group along, so
    /// the restarted process does not run next to an orphan (the lab's MosDNS wrapper killed with SIGKILL).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn exited_leader_takes_its_group_along() {
        let dir = std::env::temp_dir().join(format!("meduza-np-sup-orphan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("child.pid");
        let sup = Supervisor::new();
        let spec = LaunchSpec {
            id: "t/wrapper".into(),
            program: "/bin/sh".into(),
            args: vec!["-c".into(), format!("sleep 60 & echo $! > {}; sleep 0.5; kill -9 $$", pidfile.display())],
            env: Default::default(),
            restart: RestartPolicy::Never,
            stop_timeout: Duration::from_secs(2),
            restart_digest: "d".into(),
            bound_to: None,
        };
        sup.ensure(spec).await;
        let deadline = Instant::now() + Duration::from_secs(5);
        while sup.status("t/wrapper").map(|s| s.state) != Some(ProcState::Exited) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(sup.status("t/wrapper").unwrap().state, ProcState::Exited);
        let child: u32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        assert!(!alive(child), "the orphaned child was stopped with its group");
        sup.stop_all().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Daemon stderr goes to the log file and is forwarded; detaching empties it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn stderr_goes_to_log_file() {
        let dir = std::env::temp_dir().join(format!("meduza-np-sup-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sup = Supervisor::persistent(&dir);
        let spec = LaunchSpec {
            id: "t/log".into(),
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "echo hello >&2; exec sleep 60".into()],
            env: Default::default(),
            restart: RestartPolicy::OnFailure,
            stop_timeout: Duration::from_secs(2),
            restart_digest: "d".into(),
            bound_to: None,
        };
        sup.ensure(spec).await;
        assert!(sup.wait_settled("t/log", Duration::from_millis(300), Duration::from_secs(3)).await);
        let log = log_path(&dir, "t/log");
        assert!(log.exists());
        sup.detach_all().await;
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0, "forwarded output is dropped on detach");
        let rec = load_record(&dir, "t/log").unwrap();
        terminate_adopted(&rec, Duration::from_secs(2)).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn bound(id: &str, target: &str) -> LaunchSpec {
        LaunchSpec { bound_to: Some(target.into()), ..sleeper(id, "60", "d") }
    }

    async fn wait_new_pid(sup: &Supervisor, id: &str, old: Option<u32>) -> Option<u32> {
        for _ in 0..50 {
            if let Some(s) = sup.status(id) {
                if s.state == ProcState::Running && s.pid.is_some() && s.pid != old {
                    return s.pid;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{id} was not restarted (still {old:?})");
    }

    /// A bound process restarts whenever its target starts a new process (restart, changed spec, crash), and
    /// only then.
    #[tokio::test]
    async fn bound_process_restarts_with_target() {
        let sup = Supervisor::new();
        sup.ensure(sleeper("t/zebra", "60", "d")).await;
        sup.ensure(bound("t/bgpd", "t/zebra")).await;
        assert!(sup.wait_settled("t/bgpd", Duration::from_millis(200), Duration::from_secs(3)).await);
        let pid = |id: &str| sup.status(id).unwrap().pid;
        let b0 = pid("t/bgpd");
        sup.ensure(sleeper("t/zebra", "60", "d")).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(pid("t/bgpd"), b0, "an unchanged target keeps its dependents");

        assert!(sup.restart("t/zebra").await);
        let b1 = wait_new_pid(&sup, "t/bgpd", b0).await;
        sup.ensure(sleeper("t/zebra", "60", "d2")).await;
        let b2 = wait_new_pid(&sup, "t/bgpd", b1).await;
        #[cfg(unix)]
        unsafe {
            libc::kill(pid("t/zebra").unwrap() as i32, libc::SIGKILL)
        };
        let b3 = wait_new_pid(&sup, "t/bgpd", b2).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(pid("t/bgpd"), b3, "one restart per target start");
        sup.stop_all().await;
    }

    /// After a planner restart a bound process is adopted together with its target, but not when the target had
    /// to be started anew.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bound_process_not_adopted_after_target_restart() {
        let dir = std::env::temp_dir().join(format!("meduza-np-sup-bound-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = Supervisor::persistent(&dir);
        first.ensure(sleeper("t/zebra", "60", "d")).await;
        first.ensure(bound("t/bgpd", "t/zebra")).await;
        assert!(first.wait_settled("t/bgpd", Duration::from_millis(100), Duration::from_secs(3)).await);
        let (z, b) = (first.status("t/zebra").unwrap().pid, first.status("t/bgpd").unwrap().pid);
        first.detach_all().await;

        let second = Supervisor::persistent(&dir);
        second.ensure(sleeper("t/zebra", "60", "d")).await;
        second.ensure(bound("t/bgpd", "t/zebra")).await;
        for id in ["t/zebra", "t/bgpd"] {
            assert!(second.wait_settled(id, Duration::from_millis(100), Duration::from_secs(3)).await);
        }
        let (zs, bs) = (second.status("t/zebra").unwrap(), second.status("t/bgpd").unwrap());
        assert_eq!((zs.pid, zs.adopted, bs.pid, bs.adopted), (z, true, b, true), "both adopted");
        second.detach_all().await;

        let third = Supervisor::persistent(&dir);
        third.ensure(sleeper("t/zebra", "60", "d2")).await;
        third.ensure(bound("t/bgpd", "t/zebra")).await;
        assert!(third.wait_settled("t/bgpd", Duration::from_millis(100), Duration::from_secs(3)).await);
        let bs = third.status("t/bgpd").unwrap();
        assert!(bs.pid != b && !bs.adopted, "a restarted target restarts its dependents");
        assert!(!alive(b.unwrap()));
        third.stop_all().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replaced_binaries() {
        // Readable exe link: deleted image or a different file at the path.
        assert!(!binary_replaced(Some("/usr/sbin/tincd"), Some("1:2"), Some("1:2")));
        assert!(binary_replaced(Some("/usr/sbin/tincd (deleted)"), Some("1:2"), Some("1:2")));
        assert!(binary_replaced(Some("/usr/sbin/tincd"), Some("1:2"), Some("1:3")));
        assert!(!binary_replaced(Some("/usr/sbin/tincd"), None, None), "records of older planners");
        // Unreadable exe (daemon dropped privileges): the file identity decides; without one, do not adopt.
        assert!(!binary_replaced(None, Some("1:2"), Some("1:2")));
        assert!(binary_replaced(None, Some("1:2"), Some("1:3")));
        assert!(binary_replaced(None, Some("1:2"), None));
        assert!(binary_replaced(None, None, Some("1:2")));
    }
}
