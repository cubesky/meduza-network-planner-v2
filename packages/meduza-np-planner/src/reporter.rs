//! Builds the node report (§12.1, §12.2): sync state with backoff, packages, features derived from installed
//! packages only, instances (processes + plugin probes), resources and maintenance.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use meduza_np_protocol::report::{InstanceStatus, NodeReport, ServiceState, SyncStatus, SystemInfo};
use meduza_np_protocol::REPORT_VERSION;
use serde_json::Value;

use crate::supervisor::ProcState;
use crate::sync::Planner;

pub fn build(p: &Arc<Planner>) -> NodeReport {
    let registry = p.registry();
    let st = p.status.lock().unwrap().clone();
    let backoff = p.client().map(|c| c.gate.lock().unwrap().status(Instant::now())).unwrap_or_default();
    let node_id = p.client().map(|c| c.node_id().to_string()).unwrap_or_default();
    let (running, healthy) = p.running_packages();
    let enabled = p.enabled_services();
    // Owning instance of each supervised process: the console restarts a process through it.
    let owner: BTreeMap<String, String> = crate::state::AppliedState::load(&p.paths)
        .map(|a| a.instances.iter().flat_map(|(inst, ai)| ai.processes.keys().map(move |proc| (proc.clone(), inst.clone()))).collect())
        .unwrap_or_default();

    let mut instances: Vec<InstanceStatus> = p
        .engine
        .supervisor
        .statuses()
        .into_iter()
        .map(|s| {
            let mut detail = BTreeMap::new();
            detail.insert("restarts".into(), Value::from(s.restarts));
            if let Some(pid) = s.pid {
                detail.insert("pid".into(), Value::from(pid));
            }
            if let Some(inst) = owner.get(&s.id) {
                detail.insert("instance".into(), Value::from(inst.clone()));
            }
            InstanceStatus {
                id: format!("{}/process", s.id),
                kind: "process".into(),
                state: match s.state {
                    ProcState::Running => ServiceState::Up,
                    ProcState::Starting => ServiceState::Starting,
                    ProcState::Backoff => ServiceState::Degraded,
                    ProcState::Failed | ProcState::Exited => ServiceState::Down,
                    ProcState::Stopped => ServiceState::Disabled,
                },
                observed_at: Some(meduza_np_plugin_sdk::now_rfc3339()),
                reason: s.last_exit,
                detail,
            }
        })
        .collect();
    instances.extend(st.probes.iter().cloned());

    let network_mode = crate::platform::network_mode(&p.platform);
    let mut features = registry.features(&enabled, &running, &healthy);
    registry.mark_network_scope(&mut features, network_mode);

    let mut resources = crate::platform::resources();
    resources.supervised_processes = p.engine.supervisor.ids().len() as u32;
    resources.state_disk_free_kib = meduza_np_runtime::fsutil::free_space(&p.paths.state).ok().map(|b| b / 1024);

    NodeReport {
        report_version: REPORT_VERSION,
        node_id,
        boot_id: p.boot_id.clone(),
        sequence: p.next_sequence(),
        observed_at: meduza_np_plugin_sdk::now_rfc3339(),
        sync: SyncStatus {
            mode: st.mode,
            transport: st.transport,
            behavior: st.behavior,
            state: st.state,
            revisions: st.pointers.clone(),
            degraded_reason: st.degraded_reason.clone(),
            event_cursor: st.event_cursor.clone(),
            apply_stage: st.apply_stage.clone(),
            last_error: st.last_error.clone(),
            backoff,
        },
        packages: registry.package_statuses(),
        features,
        instances,
        resources: Some(resources),
        maintenance: st.maintenance.clone(),
        update: None,
        system: Some(SystemInfo {
            arch: std::env::consts::ARCH.into(),
            os: std::env::consts::OS.into(),
            os_release: std::fs::read_to_string("/etc/os-release").ok().and_then(|t| {
                t.lines()
                    .find(|l| l.starts_with("PRETTY_NAME="))
                    .map(|l| l.trim_start_matches("PRETTY_NAME=").trim_matches('"').to_string())
            }),
            kernel: std::fs::read_to_string("/proc/sys/kernel/osrelease").ok().map(|s| s.trim().to_string()),
            openwrt: p.platform.openwrt,
            container: p.platform.container,
            network_mode: network_mode.map(String::from),
            capabilities: crate::platform::capabilities(&p.platform),
        }),
    }
}
