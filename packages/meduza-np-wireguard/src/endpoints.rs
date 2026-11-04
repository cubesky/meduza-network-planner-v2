//! DNS endpoint reconciliation owned by the instance's generation plan. Local keys/peers are applied first.
use std::collections::BTreeMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use meduza_np_plugin_sdk::{run_plan_command, run_status_command};
use meduza_np_runtime::jitter::{jittered_period, Jitter, OsJitter};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerSpec {
    pub id: String,
    pub public_key: String,
    pub endpoint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Spec {
    pub instance: String,
    pub device: String,
    pub wg: String,
    pub peers: Vec<PeerSpec>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointStatus {
    pub public_key: String,
    pub desired_endpoint: String,
    pub phase: String,
    pub attempt: u32,
    pub retry_at_unix: u64,
    pub updated_unix: u64,
    pub resolved_endpoint: Option<String>,
    pub error: Option<String>,
}

pub fn state_path(run: &Path, instance: &str) -> PathBuf {
    run.join(format!("wireguard-{instance}.endpoints.json"))
}

/// A package upgrade must replace this helper even if its endpoint configuration is unchanged.
pub fn runner_identity() -> &'static str {
    static IDENTITY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    IDENTITY.get_or_init(|| {
        let executable = std::env::current_exe().ok();
        let metadata = executable.as_ref().and_then(|p| std::fs::metadata(p).ok());
        let base = format!("{executable:?}:{:?}", metadata.as_ref().map(|m| (m.len(), m.modified().ok())));
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            format!("{base}:{:?}", metadata.map(|m| (m.dev(), m.ino())))
        }
        #[cfg(not(unix))]
        {
            base
        }
    })
}

pub trait EndpointIo {
    fn resolve(&mut self, endpoint: &str) -> Result<SocketAddr, String>;
    /// Missing peers are not created here: only the required local syncconf may add/remove peers.
    fn current(&mut self, peer: &PeerSpec) -> Result<Option<SocketAddr>, String>;
    fn set(&mut self, peer: &PeerSpec, endpoint: SocketAddr) -> Result<(), String>;
}

pub fn reconcile(spec: &Spec, states: &mut BTreeMap<String, EndpointStatus>, io: &mut dyn EndpointIo, jitter: &mut dyn Jitter, now: u64) {
    for peer in &spec.peers {
        let state = states.entry(peer.id.clone()).or_insert_with(|| EndpointStatus {
            public_key: peer.public_key.clone(),
            desired_endpoint: peer.endpoint.clone(),
            ..Default::default()
        });
        if state.retry_at_unix > now {
            continue;
        }
        state.updated_unix = now;
        let result = match io.resolve(&peer.endpoint) {
            Err(e) => {
                state.phase = "resolution-failed".into();
                Err(e)
            }
            Ok(address) => {
                state.phase = "endpoint-update-failed".into();
                io.current(peer).and_then(|current| {
                    // Preserve a working/roaming endpoint when DNS is unchanged. Repair an empty endpoint after
                    // local link recreation even when the resolved address has not changed.
                    if current.is_none() || state.resolved_endpoint.as_deref() != Some(address.to_string().as_str()) {
                        io.set(peer, address)?;
                    }
                    state.resolved_endpoint = Some(address.to_string());
                    Ok(())
                })
            }
        };
        match result {
            Ok(()) => {
                state.phase = "ready".into();
                state.error = None;
                state.attempt = 0;
                state.retry_at_unix = now.saturating_add(60);
            }
            Err(error) => {
                state.attempt = state.attempt.saturating_add(1);
                let base = Duration::from_secs(1u64 << state.attempt.saturating_sub(1).min(6));
                let delay = jittered_period(jitter, base, 0.2).as_secs().clamp(1, 60);
                state.retry_at_unix = now.saturating_add(delay);
                // Errors contain endpoint/public-key context, never private keys or a native config dump.
                state.error = Some(format!("{}: {error}", peer.endpoint).chars().take(300).collect());
            }
        }
    }
}

struct NativeIo<'a> {
    spec: &'a Spec,
    executable: String,
}

impl EndpointIo for NativeIo<'_> {
    fn resolve(&mut self, endpoint: &str) -> Result<SocketAddr, String> {
        let out = run_status_command(&self.executable, &["resolve-endpoint", endpoint], Duration::from_secs(5), 4096)
            .map_err(|e| e.to_string())?;
        if out.timed_out {
            return Err("endpoint DNS resolution timed out".into());
        }
        if out.status != Some(0) {
            return Err(format!("endpoint DNS resolution failed: {}", out.stderr.trim()));
        }
        out.stdout.trim().parse().map_err(|_| "resolver returned an invalid address".into())
    }

    fn current(&mut self, peer: &PeerSpec) -> Result<Option<SocketAddr>, String> {
        let out = run_status_command(&self.spec.wg, &["show", &self.spec.device, "endpoints"], Duration::from_secs(3), 1024 * 1024)
            .map_err(|e| e.to_string())?;
        if out.status != Some(0) || out.timed_out {
            return Err("local endpoint observation is unavailable; retrying".into());
        }
        let value = out
            .stdout
            .lines()
            .find_map(|line| {
                let mut fields = line.split_whitespace();
                (fields.next()? == peer.public_key).then(|| fields.next()).flatten()
            })
            .ok_or("peer not yet loaded by local configuration; retrying")?;
        if value == "(none)" {
            return Ok(None);
        }
        value.parse().map(Some).map_err(|_| "cannot read the current peer endpoint".into())
    }

    fn set(&mut self, peer: &PeerSpec, endpoint: SocketAddr) -> Result<(), String> {
        let address = endpoint.to_string();
        let out = run_plan_command(
            &self.spec.wg,
            &["set", &self.spec.device, "peer", &peer.public_key, "endpoint", &address],
            Duration::from_secs(3),
            4096,
        )
        .map_err(|e| e.to_string())?;
        if out.timed_out {
            return Err("endpoint update timed out".into());
        }
        if out.status != Some(0) {
            return Err(format!("endpoint update failed: {}", out.stderr.trim()));
        }
        Ok(())
    }
}

/// Resolver runs in its own bounded subprocess: libc DNS delays cannot hang the instance runner or planner.
pub fn resolve(endpoint: &str) -> Result<SocketAddr, String> {
    if !crate::valid_endpoint(endpoint) {
        return Err("invalid endpoint".into());
    }
    endpoint.to_socket_addrs().map_err(|e| e.to_string())?.next().ok_or_else(|| "no endpoint addresses returned".into())
}

pub fn watch(spec_path: &Path, run: &Path) -> Result<(), String> {
    let spec: Spec =
        meduza_np_runtime::fsutil::read_json(spec_path, 1024 * 1024).map_err(|e| e.to_string())?.ok_or("endpoint spec missing")?;
    if meduza_np_protocol::ids::StableId::new(&spec.instance).is_err()
        || meduza_np_protocol::ids::validate_ifname(&spec.device).is_err()
        || !Path::new(&spec.wg).is_absolute()
        || spec.peers.iter().any(|p| crate::parse_key(&p.public_key).is_none() || !crate::valid_endpoint(&p.endpoint))
    {
        return Err("invalid local endpoint spec".into());
    }
    let executable = std::env::current_exe().map_err(|e| e.to_string())?.to_string_lossy().into_owned();
    let mut io = NativeIo { spec: &spec, executable };
    let mut states = BTreeMap::new();
    let mut jitter = OsJitter::new();
    let mut last_state = String::new();
    loop {
        let now = meduza_np_runtime::resources::unix_seconds();
        reconcile(&spec, &mut states, &mut io, &mut jitter, now);
        let serialized = serde_json::to_string(&states).map_err(|e| e.to_string())?;
        if serialized != last_state {
            meduza_np_runtime::fsutil::write_json_atomic(&state_path(run, &spec.instance), &states, 0o644).map_err(|e| e.to_string())?;
            last_state = serialized;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

pub fn attach_statuses(cfg: &crate::WireGuardConfig, rows: &mut [meduza_np_protocol::report::InstanceStatus], run: &Path, now: u64) {
    use meduza_np_protocol::report::ServiceState;
    for instance in cfg.instances.iter().filter(|i| cfg.enabled && i.enabled) {
        let states: BTreeMap<String, EndpointStatus> =
            meduza_np_runtime::fsutil::read_json(&state_path(run, instance.id.as_str()), 1024 * 1024).ok().flatten().unwrap_or_default();
        for peer in instance.peers.iter().filter(|p| p.enabled) {
            let Some(state) = states.get(peer.id.as_str()).filter(|s| {
                Some(&s.desired_endpoint) == peer.endpoint.as_ref()
                    && Some(&s.public_key) == peer.public_key.as_ref()
                    && now.saturating_sub(s.updated_unix) <= 120
            }) else {
                continue;
            };
            let Some(row) = rows.iter_mut().find(|r| r.id == format!("wireguard/{}/{}", instance.id, peer.id)) else { continue };
            row.detail.insert("endpointResolution".into(), serde_json::to_value(state).unwrap());
            if state.error.is_some() {
                row.detail.insert("connectionState".into(), serde_json::to_value(row.state).unwrap());
                row.detail.insert(
                    "reasonCode".into(),
                    format!("endpoint-{}", if state.phase == "resolution-failed" { "resolution-failed" } else { "update-failed" }).into(),
                );
                row.state = ServiceState::Degraded;
                row.reason = state.error.clone();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meduza_np_runtime::jitter::FixedJitter;

    struct FakeIo {
        dns_ok: bool,
        set_ok: bool,
        current: Option<SocketAddr>,
        sets: Vec<(String, SocketAddr)>,
    }
    impl EndpointIo for FakeIo {
        fn resolve(&mut self, endpoint: &str) -> Result<SocketAddr, String> {
            if endpoint.starts_with("broken.") && !self.dns_ok {
                Err("Name does not resolve".into())
            } else {
                Ok("192.0.2.7:51820".parse().unwrap())
            }
        }
        fn current(&mut self, _: &PeerSpec) -> Result<Option<SocketAddr>, String> {
            Ok(self.current)
        }
        fn set(&mut self, peer: &PeerSpec, endpoint: SocketAddr) -> Result<(), String> {
            if !self.set_ok {
                return Err("temporary endpoint update failure".into());
            }
            self.sets.push((peer.id.clone(), endpoint));
            Ok(())
        }
    }
    fn spec() -> Spec {
        Spec {
            instance: "uplink".into(),
            device: "wg-uplink".into(),
            wg: "/usr/bin/wg".into(),
            peers: vec![
                PeerSpec { id: "bad".into(), public_key: "bad-key".into(), endpoint: "broken.example:51820".into() },
                PeerSpec { id: "healthy".into(), public_key: "healthy-key".into(), endpoint: "healthy.example:51820".into() },
            ],
        }
    }

    #[test]
    fn dns_failure_is_per_peer_and_recovers_automatically_without_reloading_local_keys() {
        let spec = spec();
        let mut states = BTreeMap::new();
        let mut io = FakeIo { dns_ok: false, set_ok: true, current: Some("198.51.100.1:51820".parse().unwrap()), sets: vec![] };
        let mut jitter = FixedJitter::new(vec![0.5]);
        reconcile(&spec, &mut states, &mut io, &mut jitter, 100);
        assert_eq!(states["bad"].phase, "resolution-failed");
        assert_eq!(states["healthy"].phase, "ready");
        assert_eq!(io.sets.iter().map(|s| s.0.as_str()).collect::<Vec<_>>(), vec!["healthy"]);
        let retry = states["bad"].retry_at_unix;
        assert!(retry > 100 && retry <= 160);
        io.dns_ok = true;
        reconcile(&spec, &mut states, &mut io, &mut jitter, retry);
        assert_eq!(states["bad"].phase, "ready");
        assert!(states["bad"].error.is_none());
        assert_eq!(io.sets.len(), 2);
        io.dns_ok = false;
        reconcile(&spec, &mut states, &mut io, &mut jitter, retry + 60);
        assert_eq!(states["bad"].resolved_endpoint.as_deref(), Some("192.0.2.7:51820"));
        assert_eq!(io.sets.len(), 2, "unchanged DNS does not overwrite learned/roaming endpoints");
    }

    #[test]
    fn endpoint_updates_retry_and_repair_recreated_interfaces_even_when_dns_is_unchanged() {
        let spec = spec();
        let mut states = BTreeMap::new();
        let mut io = FakeIo { dns_ok: true, set_ok: false, current: None, sets: vec![] };
        let mut jitter = FixedJitter::new(vec![0.5]);
        reconcile(&spec, &mut states, &mut io, &mut jitter, 100);
        assert_eq!(states["bad"].phase, "endpoint-update-failed");
        io.set_ok = true;
        reconcile(&spec, &mut states, &mut io, &mut jitter, 101);
        assert_eq!(states["bad"].phase, "ready");
        assert_eq!(io.sets.len(), 2);
        reconcile(&spec, &mut states, &mut io, &mut jitter, 161);
        assert_eq!(io.sets.len(), 4, "empty endpoints are repaired without reapplying a revision");
    }

    #[test]
    fn retry_delay_stays_bounded_after_many_failures() {
        let spec = spec();
        let mut states = BTreeMap::new();
        let mut io = FakeIo { dns_ok: false, set_ok: false, current: None, sets: vec![] };
        let mut jitter = FixedJitter::new(vec![0.0, 1.0]);
        let mut now = 100;
        for _ in 0..100 {
            reconcile(&spec, &mut states, &mut io, &mut jitter, now);
            let due = states["bad"].retry_at_unix;
            assert!(due > now && due <= now + 60);
            now = due;
        }
        assert_eq!(states["bad"].attempt, 100);
    }
}
