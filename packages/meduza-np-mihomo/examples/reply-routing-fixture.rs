//! Synthetic plan tables for tools/mihomo-reply-network-test.py; never applies network resources.
use meduza_np_mihomo::{load, plan};
use meduza_np_plugin_sdk::PluginRequest;
use meduza_np_protocol::plan::NetworkResource;
use serde_json::json;

fn main() {
    let fixtures: Vec<_> = [true, false]
        .into_iter()
        .map(|conntrack| {
            let req: PluginRequest = serde_json::from_value(json!({
                "protocolVersion":1,"requestId":"reply-fixture","operation":"plan","params":null,
                "config":{"schemaVersion":1,"revision":"rev-000001","node":{"id":"fixture"},
                    "networks":{"lan":["192.0.2.0/24"]},
                    "services":{"mihomo":{"mode":"tproxy","api":{"secretRef":"api"},
                        "subscriptions":[{"id":"main","urlRef":"sub"}],"activeSubscription":"main",
                        "tproxy":{"protocols":"both","useConntrack":conntrack,"excludePorts":[5900],
                            "sideRouter":{"gateway":"192.0.2.1","replyPorts":[5900]}}},
                        "openvpn":{"instances":[{"id":"access","kind":"access","device":"tun-access"}]}},
                    "secrets":{"api":{"kind":"text","value":"synthetic-api-secret-1234567890"},
                        "sub":{"kind":"text","value":"https://sub.example.test/"}}},
                "providers":{"mihomo.binary":{"path":"/bin/true","source":"synthetic"}},
                "context":{"generationRoot":"/tmp/reply-test","stateDir":"/tmp/reply-test",
                    "runDir":"/tmp/reply-test","libexecDir":"/usr/libexec/meduza-np","platform":{},"capabilities":[]}
            }))
            .unwrap();
            let cfg = load(&req).unwrap().unwrap();
            let table = plan(&cfg, &req)
                .unwrap()
                .into_iter()
                .flat_map(|p| p.resources)
                .find_map(|r| match r {
                    NetworkResource::NftTable { content, .. } => Some(content),
                    _ => None,
                })
                .unwrap();
            json!({"conntrack":conntrack,"table":table})
        })
        .collect();
    println!("{}", serde_json::to_string(&fixtures).unwrap());
}
