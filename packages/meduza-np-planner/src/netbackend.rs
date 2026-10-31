//! Network resource backend (§9.6). Applies [`NetworkResource`]s with `ip`, `nft` and `/proc/sys`, one owned
//! object at a time. Never flushes tables, chains or routes it does not own; nft state lives only in
//! `meduza_np_*` tables that are replaced atomically.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use meduza_np_protocol::plan::{IpFamily, NetworkResource};
use tokio::process::Command;

#[derive(Debug, thiserror::Error)]
#[error("{op}: {message}")]
pub struct NetError {
    pub op: String,
    pub message: String,
}

/// Object-safe backend so the engine can run against the real system or a recorder in tests.
pub trait NetBackend: Send + Sync {
    fn add<'a>(&'a self, r: &'a NetworkResource) -> BoxFuture<'a, Result<(), NetError>>;
    fn remove<'a>(&'a self, r: &'a NetworkResource) -> BoxFuture<'a, Result<(), NetError>>;
    /// Whether an owned resource is still in place: `Some(false)` when it is known to be gone (fresh network
    /// namespace of a re-created container, reboot, deleted by hand), `None` when the backend cannot tell.
    fn exists<'a>(&'a self, _r: &'a NetworkResource) -> BoxFuture<'a, Option<bool>> {
        Box::pin(async { None })
    }
}

/// Linux implementation using iproute2 and nftables.
pub struct LinuxBackend {
    timeout: Duration,
}

impl Default for LinuxBackend {
    fn default() -> Self {
        Self { timeout: Duration::from_secs(10) }
    }
}

impl LinuxBackend {
    async fn run(&self, program: &str, args: &[String], stdin: Option<&str>) -> Result<(), NetError> {
        let op = format!("{program} {}", args.join(" "));
        let mut cmd = Command::new(program);
        cmd.args(args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
        if stdin.is_some() {
            cmd.stdin(std::process::Stdio::piped());
        }
        let mut child = cmd.spawn().map_err(|e| NetError { op: op.clone(), message: e.to_string() })?;
        if let Some(input) = stdin {
            use tokio::io::AsyncWriteExt;
            let mut pipe = child.stdin.take().expect("piped");
            pipe.write_all(input.as_bytes()).await.map_err(|e| NetError { op: op.clone(), message: e.to_string() })?;
            drop(pipe);
        }
        let out = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| NetError { op: op.clone(), message: "timed out".into() })?
            .map_err(|e| NetError { op: op.clone(), message: e.to_string() })?;
        if out.status.success() {
            Ok(())
        } else {
            Err(NetError { op, message: String::from_utf8_lossy(&out.stderr).trim().chars().take(500).collect() })
        }
    }

    /// Standard output of a read-only query; `None` when it fails or times out.
    async fn query(&self, program: &str, args: &[String]) -> Option<String> {
        let mut cmd = Command::new(program);
        cmd.args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).kill_on_drop(true);
        let out = tokio::time::timeout(self.timeout, cmd.output()).await.ok()?.ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Like [`run`](Self::run) but treats "does not exist" as success (idempotent removal).
    async fn run_absent_ok(&self, program: &str, args: &[String]) -> Result<(), NetError> {
        match self.run(program, args, None).await {
            Err(e) if is_absent(&e.message) => Ok(()),
            other => other,
        }
    }
}

fn is_absent(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("cannot find")
        || m.contains("no such")
        || m.contains("not exist")
        || m.contains("does not exist")
        || m.contains("cannot assign")
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn family_flag(f: IpFamily) -> &'static str {
    match f {
        IpFamily::Ipv4 => "-4",
        IpFamily::Ipv6 => "-6",
    }
}

fn route_args(r: &NetworkResource) -> Option<(Vec<String>, String)> {
    let NetworkResource::Route { destination, via, device, table, metric, route_type } = r else { return None };
    let mut args = Vec::new();
    if let Some(t) = route_type {
        args.push(t.clone());
    }
    args.push(destination.clone());
    if let Some(v) = via {
        args.extend(s(&["via", v]));
    }
    if let Some(d) = device {
        args.extend(s(&["dev", d]));
    }
    if let Some(t) = table {
        args.extend(["table".to_string(), t.to_string()]);
    }
    if let Some(m) = metric {
        args.extend(["metric".to_string(), m.to_string()]);
    }
    let fam = if destination.contains(':') { "-6" } else { "-4" };
    Some((args, fam.to_string()))
}

/// nft script that atomically replaces one owned table: create-if-missing, delete, then define.
pub fn nft_replace_script(family: &str, name: &str, content: &str) -> String {
    format!("table {family} {name}\ndelete table {family} {name}\n{content}\n")
}

fn sysctl_path(key: &str) -> String {
    format!("/proc/sys/{}", key.replace('.', "/"))
}

impl NetBackend for LinuxBackend {
    fn add<'a>(&'a self, r: &'a NetworkResource) -> BoxFuture<'a, Result<(), NetError>> {
        Box::pin(async move {
            match r {
                NetworkResource::Link { device, link_type, mtu, up } => {
                    match self.run("ip", &s(&["link", "add", device, "type", link_type]), None).await {
                        Err(e) if e.message.contains("File exists") => {}
                        other => other?,
                    }
                    if let Some(m) = mtu {
                        self.run("ip", &["link".into(), "set".into(), "dev".into(), device.clone(), "mtu".into(), m.to_string()], None)
                            .await?;
                    }
                    if *up {
                        self.run("ip", &s(&["link", "set", "dev", device, "up"]), None).await?;
                    }
                    Ok(())
                }
                NetworkResource::Address { device, cidr } => self.run("ip", &s(&["addr", "replace", cidr, "dev", device]), None).await,
                NetworkResource::LinkSettings { device, mtu, up } => {
                    if let Some(m) = mtu {
                        self.run("ip", &["link".into(), "set".into(), "dev".into(), device.clone(), "mtu".into(), m.to_string()], None)
                            .await?;
                    }
                    if *up {
                        self.run("ip", &s(&["link", "set", "dev", device, "up"]), None).await?;
                    }
                    Ok(())
                }
                NetworkResource::Route { .. } => {
                    let (args, fam) = route_args(r).expect("route");
                    let mut full = vec![fam, "route".into(), "replace".into()];
                    full.extend(args);
                    self.run("ip", &full, None).await
                }
                NetworkResource::Rule { priority, fwmark, mask, table, family } => {
                    let mark = match mask {
                        Some(m) => format!("{fwmark:#x}/{m:#x}"),
                        None => format!("{fwmark:#x}"),
                    };
                    // Remove an identical rule first so re-applying never duplicates it.
                    let base = vec![family_flag(*family).to_string(), "rule".into()];
                    let tail = vec!["priority".into(), priority.to_string(), "fwmark".into(), mark, "table".into(), table.to_string()];
                    let mut del = base.clone();
                    del.push("del".into());
                    del.extend(tail.clone());
                    self.run_absent_ok("ip", &del).await?;
                    let mut add = base;
                    add.push("add".into());
                    add.extend(tail);
                    self.run("ip", &add, None).await
                }
                NetworkResource::NftTable { family, name, content } => {
                    self.run("nft", &s(&["-f", "-"]), Some(&nft_replace_script(family, name, content))).await
                }
                NetworkResource::Sysctl { key, value } => {
                    let path = sysctl_path(key);
                    // Already as required: nothing to write. Containers usually mount /proc/sys read-only; the value
                    // then comes from the deployment (Quadlet `Sysctl=` in bridge mode, the host in host mode).
                    if std::fs::read_to_string(&path).map(|cur| cur.trim() == value.trim()).unwrap_or(false) {
                        return Ok(());
                    }
                    std::fs::write(&path, value).map_err(|e| NetError {
                        op: format!("sysctl {key}"),
                        message: if e.raw_os_error() == Some(30) {
                            format!("{key} must be {value} but /proc/sys is read-only here; set it in the deployment (Podman --sysctl / Quadlet Sysctl= in bridge mode, on the host in host mode)")
                        } else {
                            e.to_string()
                        },
                    })
                }
                NetworkResource::Reservation { .. } => Ok(()),
            }
        })
    }

    fn remove<'a>(&'a self, r: &'a NetworkResource) -> BoxFuture<'a, Result<(), NetError>> {
        Box::pin(async move {
            match r {
                NetworkResource::Link { device, .. } => self.run_absent_ok("ip", &s(&["link", "del", "dev", device])).await,
                NetworkResource::Address { device, cidr } => self.run_absent_ok("ip", &s(&["addr", "del", cidr, "dev", device])).await,
                NetworkResource::LinkSettings { .. } | NetworkResource::Sysctl { .. } | NetworkResource::Reservation { .. } => Ok(()),
                NetworkResource::Route { .. } => {
                    let (args, fam) = route_args(r).expect("route");
                    let mut full = vec![fam, "route".into(), "del".into()];
                    full.extend(args);
                    self.run_absent_ok("ip", &full).await
                }
                NetworkResource::Rule { priority, fwmark, mask, table, family } => {
                    let mark = match mask {
                        Some(m) => format!("{fwmark:#x}/{m:#x}"),
                        None => format!("{fwmark:#x}"),
                    };
                    self.run_absent_ok(
                        "ip",
                        &[
                            family_flag(*family).into(),
                            "rule".into(),
                            "del".into(),
                            "priority".into(),
                            priority.to_string(),
                            "fwmark".into(),
                            mark,
                            "table".into(),
                            table.to_string(),
                        ],
                    )
                    .await
                }
                NetworkResource::NftTable { family, name, .. } => self.run_absent_ok("nft", &s(&["delete", "table", family, name])).await,
            }
        })
    }

    fn exists<'a>(&'a self, r: &'a NetworkResource) -> BoxFuture<'a, Option<bool>> {
        Box::pin(async move {
            match r {
                NetworkResource::Link { device, .. } => Some(std::path::Path::new("/sys/class/net").join(device).exists()),
                NetworkResource::NftTable { family, name, .. } => match self.run("nft", &s(&["list", "table", family, name]), None).await {
                    Ok(()) => Some(true),
                    Err(e) if is_absent(&e.message) => Some(false),
                    Err(_) => None,
                },
                NetworkResource::Route { destination, table, route_type, .. } => {
                    let fam = if destination.contains(':') { "-6" } else { "-4" };
                    let mut args = s(&[fam, "route", "show", "table"]);
                    args.push(table.map_or("main".to_string(), |t| t.to_string()));
                    if let Some(t) = route_type {
                        args.extend(["type".to_string(), t.clone()]);
                    }
                    args.extend(["exact".to_string(), destination.clone()]);
                    self.query("ip", &args).await.map(|out| !out.trim().is_empty())
                }
                NetworkResource::Rule { priority, fwmark, table, family, .. } => {
                    let out = self.query("ip", &[family_flag(*family).into(), "rule".into(), "show".into()]).await?;
                    Some(rule_listed(&out, *priority, *fwmark, *table))
                }
                // Addresses and link settings are re-asserted on every apply, sysctls are checked when added.
                _ => None,
            }
        })
    }
}

/// Whether `ip rule show` output lists the rule (`<prio>:<tab>from all fwmark 0x.. lookup <table>`; planner's
/// tables are numbers, so iproute2 prints the number).
fn rule_listed(out: &str, priority: u32, fwmark: u32, table: u32) -> bool {
    let (mark, lookup) = (format!("{fwmark:#x}"), table.to_string());
    out.lines().any(|l| {
        let Some((prio, rest)) = l.split_once(':') else { return false };
        let words: Vec<&str> = rest.split_whitespace().collect();
        let after = |key: &str| words.windows(2).find(|w| w[0] == key).map(|w| w[1]);
        prio.trim() == priority.to_string()
            && after("fwmark").is_some_and(|m| m == mark || m.starts_with(&format!("{mark}/")))
            && after("lookup") == Some(lookup.as_str())
    })
}

/// Records operations instead of touching the system (tests, dry runs, non-Linux development).
#[derive(Clone, Default)]
pub struct RecordingBackend {
    pub log: Arc<Mutex<Vec<String>>>,
    /// Resources whose `add` fails, for failure-injection tests.
    pub fail_on: Arc<Mutex<Vec<NetworkResource>>>,
    /// Resources reported as gone (lost namespace) until they are added again; all others are "cannot tell".
    pub missing: Arc<Mutex<Vec<NetworkResource>>>,
}

impl NetBackend for RecordingBackend {
    fn add<'a>(&'a self, r: &'a NetworkResource) -> BoxFuture<'a, Result<(), NetError>> {
        Box::pin(async move {
            if self.fail_on.lock().unwrap().contains(r) {
                return Err(NetError { op: "add".into(), message: format!("injected failure for {r:?}") });
            }
            self.missing.lock().unwrap().retain(|m| m != r);
            self.log.lock().unwrap().push(format!("add {}", serde_json::to_string(r).unwrap()));
            Ok(())
        })
    }
    fn remove<'a>(&'a self, r: &'a NetworkResource) -> BoxFuture<'a, Result<(), NetError>> {
        Box::pin(async move {
            self.log.lock().unwrap().push(format!("remove {}", serde_json::to_string(r).unwrap()));
            Ok(())
        })
    }
    fn exists<'a>(&'a self, r: &'a NetworkResource) -> BoxFuture<'a, Option<bool>> {
        Box::pin(async move { self.missing.lock().unwrap().contains(r).then_some(false) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nft_script_replaces_only_its_table() {
        let script = nft_replace_script("inet", "meduza_np_pf", "table inet meduza_np_pf {\n}\n");
        assert!(script.starts_with("table inet meduza_np_pf\ndelete table inet meduza_np_pf\n"));
        assert!(!script.contains("flush ruleset"));
    }

    #[test]
    fn rule_listing() {
        let out = "0:\tfrom all lookup local\n19791:\tfrom all fwmark 0x4d4f lookup 233\n32766:\tfrom all lookup main\n";
        assert!(rule_listed(out, 19791, 0x4d4f, 233));
        assert!(rule_listed("100:\tfrom all fwmark 0x1/0xff lookup 7 \n", 100, 1, 7));
        assert!(!rule_listed(out, 19791, 0x4d4f, 234));
        assert!(!rule_listed(out, 19792, 0x4d4f, 233));
        assert!(!rule_listed("", 19791, 0x4d4f, 233));
    }

    #[test]
    fn route_arguments() {
        let r = NetworkResource::Route {
            destination: "10.0.0.0/8".into(),
            via: None,
            device: Some("wg0".into()),
            table: Some(100),
            metric: None,
            route_type: None,
        };
        let (args, fam) = route_args(&r).unwrap();
        assert_eq!(fam, "-4");
        assert_eq!(args, vec!["10.0.0.0/8", "dev", "wg0", "table", "100"]);
        let local = NetworkResource::Route {
            destination: "0.0.0.0/0".into(),
            via: None,
            device: Some("lo".into()),
            table: Some(233),
            metric: None,
            route_type: Some("local".into()),
        };
        assert_eq!(route_args(&local).unwrap().0[0], "local");
    }
}
