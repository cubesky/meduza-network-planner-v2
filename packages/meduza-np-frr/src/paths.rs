//! All received paths retained in FRR's post-policy unicast RIB, including non-best/invalid paths.

use std::collections::BTreeMap;
use std::net::IpAddr;

use meduza_np_protocol::report::{InstanceStatus, ServiceState};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetainedPath {
    pub prefix: String,
    pub family: String,
    /// Exact notation, including prepends, AS sets and confederation segments. None means unavailable,
    /// while Some("") is an actual empty AS_PATH received over iBGP.
    pub as_path: Option<String>,
    pub valid: Option<bool>,
    pub best: bool,
    pub next_hops: Vec<String>,
}

pub fn as_path(path: &Value) -> Option<&str> {
    path.get("path")
        .and_then(Value::as_str)
        .or_else(|| path.pointer("/aspath/string").and_then(Value::as_str))
        .or_else(|| path.get("aspath").and_then(Value::as_str))
}

pub fn retained_paths(table: &Value, family: &str) -> BTreeMap<String, Vec<RetainedPath>> {
    let mut out: BTreeMap<String, Vec<RetainedPath>> = BTreeMap::new();
    for (prefix, paths) in table.get("routes").and_then(Value::as_object).into_iter().flatten() {
        for path in paths.as_array().into_iter().flatten() {
            let Some(peer) = path.get("peerId").and_then(Value::as_str).and_then(|p| p.parse::<IpAddr>().ok()) else { continue };
            out.entry(peer.to_string()).or_default().push(RetainedPath {
                prefix: prefix.clone(),
                family: family.into(),
                as_path: as_path(path).map(str::to_string),
                valid: path.get("valid").and_then(Value::as_bool),
                best: path
                    .get("bestpath")
                    .and_then(Value::as_bool)
                    .or_else(|| path.pointer("/bestpath/overall").and_then(Value::as_bool))
                    .unwrap_or(false),
                next_hops: path
                    .get("nexthops")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|n| n.get("ip").and_then(Value::as_str).map(str::to_string))
                    .collect(),
            });
        }
    }
    out
}

#[derive(Default)]
pub struct Snapshot {
    peers: BTreeMap<String, Vec<RetainedPath>>,
    families: Vec<String>,
}

impl Snapshot {
    /// Extract only the report fields, allowing the much larger FRR table JSON to be dropped before
    /// querying the next address family on memory-constrained nodes.
    pub fn add_table(&mut self, family: &str, table: &Value) {
        self.families.push(family.into());
        for (peer, paths) in retained_paths(table, family) {
            self.peers.entry(peer).or_default().extend(paths);
        }
    }
    pub fn status(self, errors: &[String], ts: &str) -> InstanceStatus {
        let count: usize = self.peers.values().map(Vec::len).sum();
        InstanceStatus {
            id: "frr/main/paths".into(),
            kind: "bgp-paths".into(),
            state: if errors.is_empty() { ServiceState::Up } else { ServiceState::Degraded },
            observed_at: Some(ts.into()),
            reason: (!errors.is_empty()).then(|| format!("BGP path collection incomplete: {}", errors.join("; "))),
            detail: [
                ("scope".into(), Value::from("retained-post-policy")),
                ("complete".into(), Value::from(errors.is_empty())),
                ("addressFamilies".into(), Value::from(self.families)),
                ("pathCount".into(), Value::from(count)),
                ("errors".into(), Value::from(errors)),
                ("pathsByNeighbor".into(), serde_json::to_value(self.peers).unwrap_or_default()),
            ]
            .into(),
        }
    }
}

pub fn status(tables: &[(&str, &Value)], errors: &[String], ts: &str) -> InstanceStatus {
    let mut snapshot = Snapshot::default();
    for (family, table) in tables {
        snapshot.add_table(family, table);
    }
    snapshot.status(errors, ts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn all_paths_survive_the_old_32_path_and_64_prefix_sample_limits() {
        let routes: serde_json::Map<String, Value> = (0..100)
            .map(|i| {
                (
                    format!("10.0.{i}.0/24"),
                    json!([
                        {"peerId":"203.0.113.1","valid":i%2==0,"bestpath":i%3==0,"path":format!("65100 {} 65500",65200+i)}
                    ]),
                )
            })
            .collect();
        let table = json!({"routes":routes});
        let paths = super::super::bgp_as_paths_by_peer(&table);
        assert_eq!(paths["203.0.113.1"].len(), 100, "non-best and invalid paths must not be sampled out");
        let report = status(&[("ipv4", &table)], &[], "time");
        assert_eq!(report.detail["pathCount"], json!(100));
        assert_eq!(report.detail["pathsByNeighbor"]["203.0.113.1"].as_array().unwrap().len(), 100);
    }

    #[test]
    fn retains_all_candidates_exact_paths_and_empty_ibgp_paths_across_families() {
        let table = json!({"routes":{
        "192.0.2.0/24":[
            {"peerId":"203.0.113.1","valid":true,"bestpath":true,"path":"65100 65100 {65200,65300} (65400) 65500","nexthops":[{"ip":"203.0.113.1"}]},
            {"peerId":"203.0.113.1","valid":false,"aspath":{"string":"65100 1.10 65500"}},
            {"peerId":"10.0.0.2","valid":true,"path":""},
            {"peerId":"(unspec)","valid":true,"path":""}
        ]}});
        let v6 = json!({"routes":{"2001:db8::/32":[{"peerId":"2001:0db8::1","path":"65100 65200","bestpath":{"overall":true}}]}});
        let row = status(&[("ipv4", &table), ("ipv6", &v6)], &[], "time");
        assert_eq!(row.detail["pathCount"], json!(4));
        assert_eq!(row.detail["pathsByNeighbor"]["203.0.113.1"].as_array().unwrap().len(), 2);
        assert_eq!(row.detail["pathsByNeighbor"]["203.0.113.1"][1]["valid"], json!(false));
        assert_eq!(row.detail["pathsByNeighbor"]["203.0.113.1"][0]["asPath"], json!("65100 65100 {65200,65300} (65400) 65500"));
        assert_eq!(row.detail["pathsByNeighbor"]["10.0.0.2"][0]["asPath"], json!(""));
        assert_eq!(row.detail["pathsByNeighbor"]["2001:db8::1"][0]["family"], json!("ipv6"));
        assert_eq!(row.detail["pathsByNeighbor"]["2001:db8::1"][0]["best"], json!(true));
        assert_eq!(row.detail["complete"], json!(true));
        assert!(super::super::bgp_as_paths_by_peer(&v6).contains_key("2001:db8::1"));
        assert!(super::super::bgp_routes_by_peer(&v6).is_empty(), "unknown validity is not mislabeled accepted");
        let partial = status(&[("ipv4", &table)], &["ipv6: query timed out".into()], "time");
        assert_eq!(partial.state, ServiceState::Degraded);
        assert_eq!(partial.detail["complete"], json!(false));
        assert_eq!(partial.detail["pathCount"], json!(3));
    }
}
