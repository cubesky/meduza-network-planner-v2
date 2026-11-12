//! Configuration activation is authoritative over cached probes and failed-delivery retry state.

use std::collections::BTreeMap;

use meduza_np_protocol::config::NodeConfig;
use meduza_np_protocol::report::{InstanceStatus, ServiceState};
use serde_json::Value;

fn enabled(value: &Value) -> bool {
    value.get("enabled") != Some(&Value::Bool(false))
}

fn entry<'a>(items: Option<&'a Value>, id: &str) -> Option<&'a Value> {
    // A disabled draft does not shadow an active entry with the same id.
    let items = items?.as_array()?;
    items.iter().enumerate().filter(|(index, _)| entry_id(items, *index) == id).map(|(_, v)| v).max_by_key(|v| enabled(v))
}

pub fn entry_id(items: &[Value], index: usize) -> String {
    if let Some(id) = items[index].get("id").and_then(Value::as_str).filter(|id| meduza_np_protocol::ids::validate_stable_id(id).is_ok()) {
        return id.into();
    }
    let mut id = format!("invalid-{index}");
    while items.iter().any(|v| v.get("id").and_then(Value::as_str) == Some(&id)) {
        id.push_str("-x");
    }
    id
}

pub fn is_enabled(config: &NodeConfig, id: &str) -> Option<bool> {
    let mut parts = id.split('/');
    let short = parts.next()?;
    if !matches!(short, "wireguard" | "openvpn") {
        return None;
    }
    let service = config.services.get(short)?;
    if !enabled(service) {
        return Some(false);
    }
    let instance = entry(service.get("instances"), parts.next()?)?;
    if !enabled(instance) {
        return Some(false);
    }
    match parts.next() {
        None => Some(true),
        Some(peer) if short == "wireguard" => entry(instance.get("peers"), peer).map(enabled),
        _ => None,
    }
}

fn disable(rows: &mut Vec<InstanceStatus>, id: &str, kind: &str, value: &Value, parent: Option<&str>) {
    let index = rows.iter().position(|r| r.id == id).unwrap_or_else(|| {
        rows.push(InstanceStatus {
            id: id.into(),
            kind: kind.into(),
            state: ServiceState::Disabled,
            observed_at: None,
            reason: None,
            detail: BTreeMap::new(),
        });
        rows.len() - 1
    });
    let row = &mut rows[index];
    row.kind = kind.into();
    row.state = ServiceState::Disabled;
    row.reason = Some("disabled".into());
    row.observed_at = Some(meduza_np_plugin_sdk::now_rfc3339());
    row.detail.clear();
    row.detail.insert("name".into(), value.get("name").and_then(Value::as_str).unwrap_or(id.rsplit('/').next().unwrap_or(id)).into());
    if let Some(parent) = parent {
        row.detail.insert("instance".into(), parent.into());
    }
    if let Some(device) = value.get("device").and_then(Value::as_str) {
        row.detail.insert("device".into(), device.into());
    }
}

/// Disabled entries remain visible without loading a plugin, requiring its provider or parsing dormant
/// operational fields. Clear cached errors/counters immediately; removed entries must not linger either.
pub fn overlay_disabled(config: &NodeConfig, rows: &mut Vec<InstanceStatus>) {
    rows.retain(|row| {
        let mut parts = row.id.split('/');
        let short = parts.next().unwrap_or("");
        if !matches!(short, "wireguard" | "openvpn") {
            return true;
        }
        let Some(service) = config.services.get(short) else { return false };
        let Some(id) = parts.next() else { return false };
        let Some(instance) = entry(service.get("instances"), id) else { return false };
        match parts.next() {
            None => true,
            Some(peer) if short == "wireguard" => entry(instance.get("peers"), peer).is_some(),
            _ => enabled(service) && enabled(instance),
        }
    });
    for short in ["wireguard", "openvpn"] {
        let Some(service) = config.services.get(short) else { continue };
        let Some(instances) = service.get("instances").and_then(Value::as_array) else { continue };
        for (index, instance) in instances.iter().enumerate() {
            let id = entry_id(instances, index);
            // Only the selected (active, if present) entry supplies nested configuration.
            if !std::ptr::eq(entry(service.get("instances"), &id).unwrap(), instance) {
                continue;
            }
            let root = format!("{short}/{id}");
            let disabled = is_enabled(config, &root) == Some(false);
            if disabled {
                let kind = if short == "wireguard" {
                    "wireguard-interface"
                } else if instance.get("kind").and_then(Value::as_str) == Some("access") {
                    "openvpn-access"
                } else {
                    "openvpn-site2site"
                };
                disable(rows, &root, kind, instance, None);
            }
            if short == "wireguard" {
                let Some(peers) = instance.get("peers").and_then(Value::as_array) else { continue };
                for (index, peer) in peers.iter().enumerate() {
                    let id = entry_id(peers, index);
                    let id = format!("{root}/{id}");
                    if disabled || is_enabled(config, &id) == Some(false) {
                        disable(rows, &id, "wireguard-peer", peer, Some(&root));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(services: Value) -> NodeConfig {
        serde_json::from_value(json!({"schemaVersion":1,"revision":"rev-000001","node":{"id":"test"},"services":services})).unwrap()
    }

    fn stale(id: &str) -> InstanceStatus {
        InstanceStatus {
            id: id.into(),
            kind: "probe".into(),
            state: ServiceState::Down,
            reason: Some("old failure".into()),
            observed_at: None,
            detail: [("applyError".into(), json!("old failure")), ("retryAtUnix".into(), json!(123)), ("rxBytes".into(), json!(99))].into(),
        }
    }

    #[test]
    fn disabled_rows_override_cached_errors_and_parent_disabling_propagates() {
        let config = config(json!({
            "wireguard":{"instances":[
                {"id":"live","peers":[{"id":"draft","enabled":false,"endpoint":false}]},
                {"id":"paused","enabled":false,"peers":[{"id":"child"}]}
            ]},
            "openvpn":{"enabled":false,"instances":[{"id":"server","kind":"access","name":"Dormant server","port":"bad"}]}
        }));
        let mut rows = vec![
            stale("wireguard/paused"),
            stale("wireguard/live/draft"),
            stale("openvpn/server"),
            stale("openvpn/server/client"),
            stale("wireguard/removed"),
        ];
        overlay_disabled(&config, &mut rows);
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|r| r.state == ServiceState::Disabled && r.reason.as_deref() == Some("disabled")));
        assert!(rows
            .iter()
            .all(|r| !r.detail.contains_key("applyError") && !r.detail.contains_key("retryAtUnix") && !r.detail.contains_key("rxBytes")));
        assert_eq!(rows.iter().find(|r| r.id == "openvpn/server").unwrap().detail["name"], json!("Dormant server"));
        assert_eq!(is_enabled(&config, "wireguard/live"), Some(true));
        assert_eq!(is_enabled(&config, "wireguard/live/draft"), Some(false));
        assert_eq!(is_enabled(&config, "wireguard/paused/child"), Some(false));
        assert_eq!(is_enabled(&config, "openvpn/server"), Some(false));
        overlay_disabled(&config, &mut rows);
        assert_eq!(rows.len(), 4, "disabled rows are not duplicated");
    }

    #[test]
    fn disabled_drafts_do_not_shadow_active_ids_and_synthetic_ids_do_not_collide() {
        let config = config(json!({"wireguard":{"instances":[
            {"id":"bad/id"}, {"id":"invalid-0"},
            {"id":"live","enabled":false,"peers":[{"id":"draft"}]},
            {"id":"live","peers":[{"id":"p","enabled":false},{"id":"p"}]}
        ]}}));
        let items = config.services["wireguard"]["instances"].as_array().unwrap();
        assert_eq!(entry_id(items, 0), "invalid-0-x");
        assert_eq!(is_enabled(&config, "wireguard/invalid-0-x"), Some(true));
        assert_eq!(is_enabled(&config, "wireguard/live"), Some(true));
        assert_eq!(is_enabled(&config, "wireguard/live/p"), Some(true));
        let mut rows = vec![];
        overlay_disabled(&config, &mut rows);
        assert!(rows.is_empty());
    }
}
