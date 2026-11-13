//! Access exports forwarding routes, not every retained BGP path or the host's default route.
use ipnet::Ipv4Net;
use serde_json::Value;

pub fn selected(table: &Value) -> Result<Vec<Ipv4Net>, String> {
    let entries = table.as_object().ok_or("FRR IPv4 routing table unavailable")?;
    let mut routes = Vec::new();
    for (prefix, paths) in entries {
        let net: Ipv4Net = prefix.parse().map_err(|_| "invalid FRR IPv4 prefix")?;
        if net.prefix_len() == 0
            || net.addr().is_loopback()
            || net.addr().is_link_local()
            || net.addr().is_multicast()
            || net.addr().is_unspecified()
        {
            continue;
        }
        let paths = paths.as_array().ok_or("invalid FRR route entries")?;
        if paths.iter().any(|p| {
            matches!(p.get("protocol").and_then(Value::as_str), Some("bgp" | "ospf" | "static"))
                && p.get("selected").and_then(Value::as_bool) == Some(true)
                && p.get("installed").and_then(Value::as_bool) == Some(true)
                && !p
                    .get("nexthops")
                    .and_then(Value::as_array)
                    .is_some_and(|hops| hops.iter().any(|h| h.get("blackhole").and_then(Value::as_bool) == Some(true)))
        }) {
            routes.push(net.trunc());
        }
        if routes.len() > 4096 {
            return Err("FRR Access route export exceeds 4096 prefixes".into());
        }
    }
    routes.sort_by_key(|n| n.prefix_len());
    let mut kept: Vec<Ipv4Net> = Vec::new();
    for net in routes {
        if !kept.iter().any(|k| k.contains(&net)) {
            kept.push(net);
        }
    }
    kept.sort();
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_forwarding_frr_routes_are_exported() {
        let route = |protocol, selected, installed| json!([{"protocol":protocol,"selected":selected,"installed":installed}]);
        let table = json!({
            "10.8.0.0/16":route("bgp",true,true), "10.8.1.0/24":route("ospf",true,true),
            "192.0.2.0/24":route("static",true,true), "198.51.100.0/24":route("bgp",false,true),
            "203.0.113.0/24":route("bgp",true,false), "10.9.0.0/16":route("kernel",true,true),
            "10.10.0.0/16":route("connected",true,true), "0.0.0.0/0":route("bgp",true,true),
            "127.0.0.0/8":route("bgp",true,true), "169.254.0.0/16":route("ospf",true,true),
            "224.0.0.0/4":route("static",true,true),
            "10.11.0.0/16":[{"protocol":"static","selected":true,"installed":true,"nexthops":[{"blackhole":true}]}]
        });
        assert_eq!(selected(&table).unwrap(), ["10.8.0.0/16".parse::<Ipv4Net>().unwrap(), "192.0.2.0/24".parse().unwrap()]);
        assert!(selected(&json!({})).unwrap().is_empty());
        assert!(selected(&json!(null)).is_err());
        assert!(selected(&json!({"not-a-prefix":[]})).is_err());
        assert!(selected(&json!({"10.8.0.0/16":null})).is_err());
    }
}
