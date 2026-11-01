//! Project-owned DNS options and validation. Subscription DNS never changes these settings.
use std::net::{IpAddr, SocketAddr};

use meduza_np_protocol::Validator;
use serde_json::Value;

use crate::MihomoDns;

/// Literal endpoint of a resolver. Encrypted DNS does not reach a plain DNS listener.
fn plain_endpoint(value: &str) -> Option<SocketAddr> {
    let value = value.trim();
    let value = if let Some((scheme, rest)) = value.split_once("://") {
        if !matches!(scheme, "udp" | "tcp") {
            return None;
        }
        rest.split('/').next()?
    } else {
        value
    };
    let (value, suffix) = value.split_once('#').unwrap_or((value, ""));
    let value = value.split('?').next()?;
    if let Some(port) = value.strip_prefix("localhost:").and_then(|p| p.parse::<u16>().ok()) {
        return Some(SocketAddr::from(([127, 0, 0, 1], port)));
    }
    if let Ok(addr) = value.parse::<SocketAddr>() {
        return Some(addr);
    }
    let ip = if value == "localhost" { "127.0.0.1" } else { value };
    let ip = ip.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().ok()?;
    Some(SocketAddr::new(ip, suffix.parse::<u16>().ok().filter(|p| *p != 0).unwrap_or(53)))
}

fn literal_host(value: &str) -> bool {
    let value = value.split_once("://").map_or(value, |(_, rest)| rest);
    let value = value.split(['/', '#', '?']).next().unwrap_or_default();
    value.parse::<IpAddr>().is_ok()
        || value.parse::<SocketAddr>().is_ok()
        || value.strip_prefix('[').and_then(|v| v.split_once(']')).is_some_and(|(ip, _)| ip.parse::<IpAddr>().is_ok())
}

pub fn overlaps(a: SocketAddr, b: SocketAddr) -> bool {
    a.port() == b.port() && (a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified())
}

fn reaches(target: SocketAddr, listener: SocketAddr, local_ips: &[IpAddr]) -> bool {
    target.port() == listener.port()
        && (target.ip() == listener.ip()
            || listener.ip().is_unspecified()
                && (target.ip().is_loopback() || target.ip().is_unspecified() || local_ips.contains(&target.ip())))
}

pub fn validate(d: &MihomoDns, front: &[SocketAddr], local_ips: &[IpAddr], v: &mut Validator) {
    v.check(d.listen.port() != 0, "/dns/listen", "ports are 1-65535");
    v.check(!front.iter().any(|f| overlaps(*f, d.listen)), "/dns/listen", "Mihomo DNS must use its own listener, not the front end's");
    v.check(!d.nameservers.is_empty(), "/dns/nameservers", "at least one nameserver is required when DNS is enabled");
    v.check(
        !d.respect_rules || !d.proxy_server_nameservers.is_empty(),
        "/dns/proxyServerNameservers",
        "respectRules requires independent proxy-server nameservers",
    );
    v.check(d.fake_ip_range.is_none_or(|n| n.addr().is_ipv4()), "/dns/fakeIpRange", "TPROXY Fake-IP range must be IPv4");
    let mut upstreams = Vec::new();
    for (field, servers) in [
        ("nameservers", &d.nameservers),
        ("fallback", &d.fallback),
        ("proxyServerNameservers", &d.proxy_server_nameservers),
        ("directNameservers", &d.direct_nameservers),
    ] {
        upstreams.extend(servers.iter().enumerate().map(|(i, s)| (format!("/dns/{field}/{i}"), s.as_str())));
    }
    for (field, policy) in [("nameserverPolicy", &d.nameserver_policy), ("proxyServerNameserverPolicy", &d.proxy_server_nameserver_policy)]
    {
        for (key, servers) in policy {
            let key = key.replace('~', "~0").replace('/', "~1");
            v.check(!servers.servers().is_empty(), format!("/dns/{field}/{key}"), "policy needs at least one resolver");
            upstreams.extend(servers.servers().iter().map(|s| (format!("/dns/{field}/{key}"), s.as_str())));
        }
    }
    v.check(
        !d.default_nameservers.is_empty() || upstreams.iter().all(|(_, s)| literal_host(s)),
        "/dns/defaultNameservers",
        "plain IP bootstrap resolvers are required for DoH/DoT names",
    );
    for (path, server) in upstreams {
        v.check(!server.trim().is_empty(), &path, "resolver cannot be empty");
        if let Some(target) = plain_endpoint(server) {
            v.check(!front.iter().any(|f| reaches(target, *f, local_ips)), &path, "nameserver points back at the dnsmasq front end (loop)");
            v.check(!reaches(target, d.listen, local_ips), &path, "nameserver points back at Mihomo DNS itself (loop)");
        }
    }
    for (i, ip) in d.default_nameservers.iter().enumerate() {
        let target = SocketAddr::new(*ip, 53);
        v.check(
            !front.iter().any(|f| reaches(target, *f, local_ips)) && !reaches(target, d.listen, local_ips),
            format!("/dns/defaultNameservers/{i}"),
            "bootstrap must not depend on a local DNS backend or front end (loop)",
        );
    }
}

pub fn controlled(d: &MihomoDns) -> Value {
    if !d.enabled {
        return serde_json::json!({"enable": false});
    }
    let mut dns = serde_json::json!({
        "enable": true, "listen": d.listen.to_string(), "ipv6": false,
        "enhanced-mode": if d.fake_ip { "fake-ip" } else { "redir-host" },
        "nameserver": d.nameservers,
        "default-nameserver": d.default_nameservers.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
        "respect-rules": d.respect_rules,
        "direct-nameserver-follow-policy": d.direct_nameserver_follow_policy,
        "fake-ip-filter-mode": d.fake_ip_filter_mode,
    });
    for (key, servers) in [
        ("fallback", &d.fallback),
        ("proxy-server-nameserver", &d.proxy_server_nameservers),
        ("direct-nameserver", &d.direct_nameservers),
        ("fake-ip-filter", &d.fake_ip_filter),
    ] {
        if !servers.is_empty() {
            dns[key] = serde_json::json!(servers);
        }
    }
    for (key, policy) in
        [("nameserver-policy", &d.nameserver_policy), ("proxy-server-nameserver-policy", &d.proxy_server_nameserver_policy)]
    {
        if !policy.is_empty() {
            dns[key] = serde_json::json!(policy);
        }
    }
    if let Some(range) = d.fake_ip_range {
        dns["fake-ip-range"] = serde_json::json!(range);
    }
    if let Some(filter) = &d.fallback_filter {
        let mut value = serde_json::to_value(filter).expect("serializable DNS filter");
        if let Some(code) = value.as_object_mut().and_then(|m| m.remove("geoipCode")) {
            value["geoip-code"] = code;
        }
        dns["fallback-filter"] = value;
    }
    dns
}
