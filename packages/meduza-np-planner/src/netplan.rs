//! Planner-owned network features (§9.6): 1:1 prefix mappings and port forwards rendered into one owned
//! nftables table per family (`meduza_np_nat` / `meduza_np_nat6`), replaced atomically; health ports are TCP
//! listeners run by planner itself (see [`HealthPorts`]). No other table, chain or system rule is touched.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use ipnet::IpNet;
use meduza_np_protocol::config::{Networks, TransportProto};
use meduza_np_protocol::plan::{GenerationPlan, NetworkResource};
use meduza_np_protocol::PLUGIN_PROTOCOL_VERSION;

pub const INSTANCE: &str = "planner/network";

fn protos(p: TransportProto) -> &'static [&'static str] {
    match p {
        TransportProto::Tcp => &["tcp"],
        TransportProto::Udp => &["udp"],
        TransportProto::Both => &["tcp", "udp"],
    }
}

fn family_of(addr: &IpAddr) -> &'static str {
    if addr.is_ipv4() {
        "ip"
    } else {
        "ip6"
    }
}

fn set(nets: &[IpNet]) -> String {
    let items: Vec<String> = nets.iter().map(|n| n.to_string()).collect();
    format!("{{ {} }}", items.join(", "))
}

/// Renders the owned NAT table bodies per family (`ip`, `ip6`). Empty map = nothing to apply.
pub fn render(nets: &Networks) -> BTreeMap<&'static str, String> {
    let mut pre: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut post: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut out: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for pf in nets.port_forwards.iter().filter(|p| p.enabled) {
        let fam = family_of(&pf.target_address);
        let saddr = if pf.source_cidrs.is_empty() { String::new() } else { format!("{fam} saddr {} ", set(&pf.source_cidrs)) };
        let daddr = pf.listen_address.map(|a| format!("{fam} daddr {a} ")).unwrap_or_default();
        let target = if pf.target_address.is_ipv6() {
            format!("[{}]:{}", pf.target_address, pf.target_port)
        } else {
            format!("{}:{}", pf.target_address, pf.target_port)
        };
        // Connections made on the node to one of its own addresses never pass prerouting. Rootless Podman delivers
        // published ports that way (rootlessport-child dials the container address from inside the namespace),
        // so the same forward also sits in the output hook; loopback is left alone (DNAT from 127/8 cannot leave).
        let lo = if fam == "ip" { "ip daddr != 127.0.0.0/8" } else { "ip6 daddr != ::1" };
        for proto in protos(pf.proto) {
            pre.entry(fam)
                .or_default()
                .push(format!("{saddr}{daddr}{proto} dport {} dnat to {target} comment \"pf:{}\"", pf.listen_port, pf.id));
            out.entry(fam).or_default().push(format!(
                "fib daddr type local {lo} {saddr}{daddr}{proto} dport {} dnat to {target} comment \"pf-local:{}\"",
                pf.listen_port, pf.id
            ));
            if pf.snat {
                post.entry(fam).or_default().push(format!(
                    "{fam} daddr {} {proto} dport {} ct status dnat masquerade comment \"pf-snat:{}\"",
                    pf.target_address, pf.target_port, pf.id
                ));
            }
        }
    }
    for m in nets.mappings.iter().filter(|m| m.enabled) {
        let fam = if m.actual.addr().is_ipv4() { "ip" } else { "ip6" };
        // Inbound: advertised → actual; outbound: actual → advertised (netmap, prefix lengths are equal).
        pre.entry(fam)
            .or_default()
            .push(format!("{fam} daddr {} dnat {fam} prefix to {} comment \"map:{}\"", m.advertised, m.actual, m.id));
        post.entry(fam)
            .or_default()
            .push(format!("{fam} saddr {} snat {fam} prefix to {} comment \"map:{}\"", m.actual, m.advertised, m.id));
    }
    let mut tables = BTreeMap::new();
    for fam in ["ip", "ip6"] {
        let p = pre.remove(fam).unwrap_or_default();
        let q = post.remove(fam).unwrap_or_default();
        let o = out.remove(fam).unwrap_or_default();
        if p.is_empty() && q.is_empty() {
            continue;
        }
        let name = if fam == "ip" { "meduza_np_nat" } else { "meduza_np_nat6" };
        let mut t = format!("table {fam} {name} {{\n\tchain prerouting {{\n\t\ttype nat hook prerouting priority dstnat; policy accept;\n");
        for r in p {
            t.push_str(&format!("\t\t{r}\n"));
        }
        t.push_str("\t}\n");
        if !o.is_empty() {
            t.push_str("\tchain output {\n\t\ttype nat hook output priority dstnat; policy accept;\n");
            for r in o {
                t.push_str(&format!("\t\t{r}\n"));
            }
            t.push_str("\t}\n");
        }
        t.push_str("\tchain postrouting {\n\t\ttype nat hook postrouting priority srcnat; policy accept;\n");
        for r in q {
            t.push_str(&format!("\t\t{r}\n"));
        }
        t.push_str("\t}\n}\n");
        tables.insert(fam, t);
    }
    tables
}

/// The planner's own plan for the current configuration, or `None` when nothing is configured.
pub fn plan(nets: &Networks, request_id: &str) -> Option<GenerationPlan> {
    let tables = render(nets);
    if tables.is_empty() {
        return None;
    }
    let mut resources = Vec::new();
    if tables.contains_key("ip") {
        resources.push(NetworkResource::Sysctl { key: "net.ipv4.ip_forward".into(), value: "1".into() });
    }
    if tables.contains_key("ip6") {
        resources.push(NetworkResource::Sysctl { key: "net.ipv6.conf.all.forwarding".into(), value: "1".into() });
    }
    for (fam, content) in tables {
        let name = if fam == "ip" { "meduza_np_nat" } else { "meduza_np_nat6" };
        resources.push(NetworkResource::NftTable { family: fam.into(), name: name.into(), content });
    }
    Some(GenerationPlan {
        protocol_version: PLUGIN_PROTOCOL_VERSION,
        request_id: request_id.into(),
        instance: INSTANCE.into(),
        files: Vec::new(),
        processes: Vec::new(),
        resources,
        depends_on: Vec::new(),
        probes: Vec::new(),
        checks: Vec::new(),
        reload: Vec::new(),
    })
}

/// Health port listeners (§9.6 "健康端口"): plain TCP, meaning declared explicitly.
/// `planner-alive` accepts while planner runs; `feature:<id>` listens only while that feature is healthy.
pub struct HealthPorts;

impl HealthPorts {
    pub async fn run(planner: Arc<crate::sync::Planner>) {
        let mut listeners: BTreeMap<String, (String, tokio::task::JoinHandle<()>)> = BTreeMap::new();
        loop {
            let config = crate::state::load_last_good(&planner.paths).ok().flatten().map(|lg| lg.config);
            let wanted: Vec<_> = config.map(|c| c.networks.health_ports.into_iter().filter(|h| h.enabled).collect()).unwrap_or_default();
            let features = {
                let (running, healthy) = planner.running_packages();
                planner.registry().features(&planner.enabled_services(), &running, &healthy)
            };
            let mut keep = std::collections::BTreeSet::new();
            for hp in wanted {
                let active = match hp.semantics.strip_prefix("feature:") {
                    Some(id) => features.iter().any(|f| f.id == id && f.healthy),
                    None => hp.semantics == "planner-alive",
                };
                let addr = std::net::SocketAddr::new(hp.listen_address.unwrap_or(IpAddr::from([0, 0, 0, 0])), hp.port);
                let key = format!("{}@{addr}", hp.id);
                if !active {
                    continue;
                }
                keep.insert(key.clone());
                if listeners.contains_key(&key) {
                    continue;
                }
                match tokio::net::TcpListener::bind(addr).await {
                    Ok(l) => {
                        let id = hp.id.to_string();
                        let task = tokio::spawn(async move {
                            loop {
                                // Accept and close: the TCP handshake itself is the health signal.
                                if l.accept().await.is_err() {
                                    tokio::time::sleep(Duration::from_millis(200)).await;
                                }
                            }
                        });
                        info!("health port {id} listening on {addr}");
                        listeners.insert(key, (hp.id.to_string(), task));
                    }
                    Err(e) => warn!("health port {} cannot bind {addr}: {e}", hp.id),
                }
            }
            let stale: Vec<String> = listeners.keys().filter(|k| !keep.contains(*k)).cloned().collect();
            for k in stale {
                if let Some((id, task)) = listeners.remove(&k) {
                    task.abort();
                    info!("health port {id} closed");
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_owned_table_only() {
        let nets: Networks = serde_json::from_value(json!({
            "mappings": [{"id": "office", "advertised": "10.100.1.0/24", "actual": "192.168.1.0/24"}],
            "portForwards": [
                {"id": "web", "proto": "tcp", "listenPort": 8443, "targetAddress": "192.168.1.10", "targetPort": 443, "sourceCidrs": ["203.0.113.0/24"], "snat": true},
                {"id": "dns", "proto": "both", "listenAddress": "10.42.1.1", "listenPort": 5353, "targetAddress": "192.168.1.2", "targetPort": 53},
                {"id": "off", "enabled": false, "proto": "tcp", "listenPort": 1, "targetAddress": "192.168.1.3", "targetPort": 1}
            ]
        }))
        .unwrap();
        let t = render(&nets);
        let ip = &t["ip"];
        assert!(ip.starts_with("table ip meduza_np_nat {"));
        assert!(ip.contains("ip saddr { 203.0.113.0/24 } tcp dport 8443 dnat to 192.168.1.10:443 comment \"pf:web\""));
        assert!(ip.contains("ip daddr 192.168.1.10 tcp dport 443 ct status dnat masquerade"));
        assert!(ip.contains("ip daddr 10.42.1.1 udp dport 5353 dnat to 192.168.1.2:53"));
        // locally originated connections (rootless Podman published ports), not loopback
        assert!(ip.contains("chain output {\n\t\ttype nat hook output priority dstnat;"));
        assert!(ip.contains(
            "fib daddr type local ip daddr != 127.0.0.0/8 ip saddr { 203.0.113.0/24 } tcp dport 8443 dnat to 192.168.1.10:443 comment \"pf-local:web\""
        ));
        assert!(ip.contains("fib daddr type local ip daddr != 127.0.0.0/8 ip daddr 10.42.1.1 tcp dport 5353 dnat to 192.168.1.2:53"));
        assert!(!ip.contains("pf-local:off"));
        assert!(ip.contains("ip daddr 10.100.1.0/24 dnat ip prefix to 192.168.1.0/24"));
        assert!(ip.contains("ip saddr 192.168.1.0/24 snat ip prefix to 10.100.1.0/24"));
        assert!(!ip.contains("pf:off"));
        assert!(!ip.contains("flush"));
        let p = plan(&nets, "r").unwrap();
        assert!(p.validate().is_empty(), "{:?}", p.validate().errors());
        assert!(plan(&Networks::default(), "r").is_none());
    }
}
