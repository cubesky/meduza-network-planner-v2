// Import of the previous planner's etcd export (D58): conversion, preview, staging with supplied values, and what the
// imported cluster-wide settings compile to.

import { test } from "node:test";
import assert from "node:assert/strict";
import { adminHeaders, startManager } from "./helpers.js";
import { convertEtcdExport } from "../src/importer/etcd.js";
import { generateWireGuardKeyPair } from "../src/keys.js";

const kv = (key: string, value: string) => ({ key, key_encoding: "utf-8", value, value_encoding: "utf-8" });
const R = "[REDACTED]";

function fixture(): Record<string, unknown> {
  return {
    format: "meduza-etcd-export-v1",
    redacted: true,
    kvs: [
      kv("/commit", "2026-09-07T15:29:42+0800"),
      kv("/dns/hosts/git.example.test", "192.0.2.10"),
      kv("/dns/hosts/localhost", "127.0.0.1\n[::1]"),
      kv("/global/bgp/filter/in", "deny 0.0.0.0/0\npermit 0.0.0.0/0 le 32"),
      kv("/global/bgp/filter/out", "deny 0.0.0.0/0\npermit 0.0.0.0/0 le 32"),
      kv("/global/bgp/max_paths", "8"),
      kv("/global/bgp/edge_broadcast", "10.42.1.0/24"),
      kv("/global/clash/subscriptions/default/url", "https://sub.example.test/x?token=1"),
      kv("/global/mosdns/local", "domain:example.cn\n\n# comment\ndomain:example.com.cn"),
      kv("/global/mosdns/block", "domain:ads.example"),
      kv("/global/mosdns/ddns", ""),
      kv("/global/mosdns/global", "domain:google.com"),
      kv("/global/mosdns/refresh", "1440"),
      kv("/global/mosdns/rule_files", JSON.stringify({ "geosite-cn.txt": "https://rules.example.test/cn" })),
      kv("/global/mosdns/plugins", "  - tag: main\n    type: sequence\n    args:\n      - matches: qname &/etc/mosdns/etcd_local.txt\n        exec: reject\n      - matches: qname &/etc/mosdns/geosite-cn.txt\n        exec: reject\n  - tag: server\n    type: udp_server\n    args:\n      entry: main\n      listen: 0.0.0.0:1153\n"),
      kv("/global/tinc/cipher", "aes-256-cbc"),
      kv("/global/tinc/digest", "SHA512"),
      kv("/global/tinc/netname", "meduza"),
      kv("/global/ospf/area", "0.0.0.0"),
      kv("/global/easytier/network_name", "x"),
      // node A: mesh member, edge router with OpenVPN (static key, redacted) and WireGuard (peer key redacted)
      kv("/nodes/Node-A/router_id", "10.42.1.1"),
      kv("/nodes/Node-A/lan", "10.42.11.0/24\n10.42.50.0/24"),
      kv("/nodes/Node-A/bgp/enable", "true"),
      kv("/nodes/Node-A/bgp/local_asn", "64601"),
      kv("/nodes/Node-A/bgp/max_paths", "8"),
      kv("/nodes/Node-A/tinc/enable", "true"),
      kv("/nodes/Node-A/tinc/address", "a.example.test"),
      kv("/nodes/Node-A/tinc/ipv4", "10.42.1.1/24"),
      kv("/nodes/Node-A/tinc/port", "11010"),
      kv("/nodes/Node-A/tinc/dev_name", "mesh0"),
      kv("/nodes/Node-A/tinc/private_key", R),
      kv("/nodes/Node-A/clash/enable", "true"),
      kv("/nodes/Node-A/clash/mode", "tproxy"),
      kv("/nodes/Node-A/clash/active_subscription", "default"),
      kv("/nodes/Node-A/clash/refresh/interval_minutes", "1440"),
      kv("/nodes/Node-A/clash/tproxy_protocol", "tcp"),
      kv("/nodes/Node-A/clash/exclude_tproxy_port", "22\n53"),
      kv("/nodes/Node-A/clash/use_conntrack", "false"),
      kv("/nodes/Node-A/clash/exclude_rfc1918", "true"),
      kv("/nodes/Node-A/mosdns/enable", "true"),
      kv("/nodes/Node-A/dnsmasq/enable", "true"),
      kv("/nodes/Node-A/ospf/enable", "true"),
      kv("/nodes/Node-A/openvpn/Lemon/enable", "true"),
      kv("/nodes/Node-A/openvpn/Lemon/proto", "tcp-server"),
      kv("/nodes/Node-A/openvpn/Lemon/port", "21202"),
      kv("/nodes/Node-A/openvpn/Lemon/dev", "tuns4"),
      kv("/nodes/Node-A/openvpn/Lemon/ifconfig", "192.168.207.33 192.168.207.34"),
      kv("/nodes/Node-A/openvpn/Lemon/keepalive", "10 120"),
      kv("/nodes/Node-A/openvpn/Lemon/cipher", "AES-256-CBC"),
      kv("/nodes/Node-A/openvpn/Lemon/comp_lzo", "yes"),
      kv("/nodes/Node-A/openvpn/Lemon/allow_compression", "asym"),
      kv("/nodes/Node-A/openvpn/Lemon/secret", R),
      kv("/nodes/Node-A/openvpn/Lemon/bgp/peer_asn", "65252"),
      kv("/nodes/Node-A/openvpn/Lemon/bgp/peer_ip", "192.168.207.34"),
      kv("/nodes/Node-A/openvpn/Lemon/bgp/no_transit", "true"),
      kv("/nodes/Node-A/openvpn/Far/enable", "true"),
      kv("/nodes/Node-A/openvpn/Far/proto", "tcp-client"),
      kv("/nodes/Node-A/openvpn/Far/remote", "far.example.test"),
      kv("/nodes/Node-A/openvpn/Far/port", "8008"),
      kv("/nodes/Node-A/openvpn/Far/ifconfig", "100.66.8.2 100.66.8.1"),
      kv("/nodes/Node-A/openvpn/Far/client", "1"),
      kv("/nodes/Node-A/openvpn/Far/ca", R),
      kv("/nodes/Node-A/openvpn/Far/cert", R),
      kv("/nodes/Node-A/openvpn/Far/key", R),
      kv("/nodes/Node-A/openvpn/Far/remote_cert_tls", "server"),
      kv("/nodes/Node-A/openvpn/Far/bgp/peer_asn", "65200"),
      kv("/nodes/Node-A/openvpn/Far/bgp/peer_ip", "100.66.8.1"),
      kv("/nodes/Node-A/wireguard/KK/enable", "true"),
      kv("/nodes/Node-A/wireguard/KK/address", "10.42.248.2/30"),
      kv("/nodes/Node-A/wireguard/KK/dev", "wg-kk"),
      kv("/nodes/Node-A/wireguard/KK/listen_port", "21400"),
      kv("/nodes/Node-A/wireguard/KK/private_key", R),
      kv("/nodes/Node-A/wireguard/KK/peer/KK-1/endpoint", "kk.example.test:38129"),
      kv("/nodes/Node-A/wireguard/KK/peer/KK-1/persistent_keepalive", "30"),
      kv("/nodes/Node-A/wireguard/KK/peer/KK-1/public_key", R),
      kv("/nodes/Node-A/wireguard/KK/bgp/peer_asn", "65450"),
      kv("/nodes/Node-A/wireguard/KK/bgp/peer_ip", "10.42.248.1"),
      // node B: roaming mesh member, MosDNS off, no ASN of its own
      kv("/nodes/Node-B/behavior", "roaming"),
      kv("/nodes/Node-B/router_id", "10.42.1.6"),
      kv("/nodes/Node-B/lan", "10.42.9.0/24"),
      kv("/nodes/Node-B/tinc/enable", "true"),
      kv("/nodes/Node-B/tinc/ipv4", "10.42.1.6/24"),
      kv("/nodes/Node-B/tinc/port", "11010"),
      kv("/nodes/Node-B/tinc/private_key", R),
      kv("/nodes/Node-B/clash/enable", "true"),
      kv("/nodes/Node-B/clash/mode", "mixed"),
      kv("/nodes/Node-B/clash/refresh/interval_minutes", "60"),
      kv("/updated/Node-A/last", "2026-10-28T05:00:00Z"),
    ],
  };
}

test("etcd import (D58): conversion of every taken-over key family", () => {
  const r = convertEtcdExport(fixture());
  const d = r.document as any;
  assert.deepEqual(d.routing, {
    internalAsn: 64601,
    maximumPaths: 8,
    bgpFilter: { in: ["deny 0.0.0.0/0", "permit 0.0.0.0/0 le 32"], out: ["deny 0.0.0.0/0", "permit 0.0.0.0/0 le 32"] },
  });
  assert.deepEqual(d.dns.hosts, [{ hostname: "git.example.test", ips: ["192.0.2.10"] }, { hostname: "localhost", ips: ["127.0.0.1", "::1"] }]);
  assert.deepEqual(d.tinc, { cipher: "aes-256-cbc", digest: "sha512" }, "lowercase, as the plugin spells them");
  assert.deepEqual(d.mihomo, { subscriptions: [{ id: "default", name: "default", urlRef: "mihomo-sub-default" }], default: "default", refreshSeconds: 86400 });
  assert.equal(r.needs.find((n) => n.id === "mihomo-sub-default")!.value, "https://sub.example.test/x?token=1", "a value the export carries is taken over");
  assert.deepEqual(d.mosdns.domains, { local: ["domain:example.cn", "domain:example.com.cn"], block: ["domain:ads.example"], ddns: [], global: ["domain:google.com"] });
  assert.deepEqual(d.mosdns.ruleFiles, [{ id: "geosite-cn", fileName: "geosite-cn.txt", url: "https://rules.example.test/cn", refreshHours: 24 }]);
  assert.equal([d.mosdns.listen, d.mosdns.port].join(":"), "0.0.0.0:1153");
  assert.deepEqual(Object.keys(d.nodes), ["node-a", "node-b"], "stable lowercase ids");
  assert.equal(d.nodes["node-a"].displayName, "Node-A");

  const a = d.nodes["node-a"];
  assert.deepEqual(a.networks.lan, ["10.42.11.0/24", "10.42.50.0/24"]);
  assert.deepEqual(a.services.tinc, { enabled: true, privateKeyRef: "tinc-node-a", address: "10.42.1.1/24", publicAddresses: ["a.example.test"], port: 11010, device: "mesh0" });
  assert.deepEqual(a.services.mihomo.tproxy, { protocols: "tcp", excludePorts: [22, 53], excludePrivateDestinations: true, useConntrack: false });
  assert.equal(a.services.mihomo.refreshSeconds, undefined, "same as the cluster interval");
  assert.equal(d.nodes["node-b"].services.mihomo.refreshSeconds, 3600, "the node's own interval");
  assert.deepEqual(a.services.dnsmasq, { enabled: false, listenAddresses: [], fallbackServers: [] });
  const [lemon, far] = a.services.openvpn.instances;
  assert.deepEqual(
    [lemon.role, lemon.proto, lemon.port, lemon.compression, lemon.staticKeyRef, lemon.ifconfig.remote, lemon.keepalive.timeout],
    ["server", "tcp", 21202, "lzo-asym", "ovpn-node-a-lemon-secret", "192.168.207.34", 120],
  );
  assert.deepEqual(lemon.bgp, { remoteAsn: 65252, address: "192.168.207.34", noTransit: true, noForward: false, exportInternal: true });
  assert.deepEqual([far.role, far.remotes, far.p2pPull, far.tls.caRef, far.tls.remoteCertTls], ["client", [{ host: "far.example.test", port: 8008 }], true, "ovpn-node-a-far-ca", "server"]);
  const kk = a.services.wireguard.instances[0];
  assert.deepEqual(kk.peers[0].allowedIps, ["0.0.0.0/0"]);
  assert.equal(kk.peers[0].bgp.address, "10.42.248.1");
  assert.deepEqual(r.nodeKeys.map((k) => k.secretId), ["tinc-node-a", "tinc-node-b"], "redacted Tinc keys are generated on the nodes");
  assert.ok(r.notes[0]!.includes("/global/ospf") && r.notes[0]!.includes("/nodes/*/ospf") && r.notes[0]!.includes("/updated/"), r.notes[0]);
  assert.ok(r.notes.some((n) => n.includes("dnsmasq was enabled")));
  assert.throws(() => convertEtcdExport({ format: "other", kvs: [] }), /meduza-etcd-export-v1/);
});

test("etcd import (D58): preview, staging with supplied values, and the compiled result", async () => {
  const ctx = await startManager();
  const { app, s } = ctx;
  const admin = await adminHeaders(app);
  const preview = (await app.inject({ method: "POST", url: "/api/v1/import/etcd/preview", headers: admin, payload: { export: fixture() } })).json();
  assert.deepEqual(preview.existingNodes, []);
  assert.ok(!preview.problems.some((p: any) => /not registered/.test(p.message)), "nodes the import creates are not reported");
  assert.ok(preview.disabled.includes("/nodes/node-a/services/openvpn/instances/0/enabled"), "no static key: the tunnel stays off");
  // the preview shows the staged result: generated API secrets exist, requested Tinc keys are pending (warnings)
  const previewErrors = preview.problems.filter((p: any) => p.severity !== "warning").map((p: any) => p.message);
  assert.ok(!previewErrors.some((m: string) => /tinc key|tinc-node|mihomo-api|mihomo-sub|roaming/.test(m)), previewErrors.join("; "));
  assert.ok(preview.problems.some((p: any) => p.severity === "warning" && /joins once its tinc key is generated/.test(p.message)));

  const wg = generateWireGuardKeyPair();
  const peer = generateWireGuardKeyPair();
  const res = await app.inject({
    method: "POST",
    url: "/api/v1/import/etcd/apply",
    headers: admin,
    payload: {
      export: fixture(),
      createNodes: true,
      values: { "wg-node-a-kk": wg.privateKey, "public:/nodes/node-a/services/wireguard/instances/0/peers/0/publicKey": peer.publicKey },
    },
  });
  assert.equal(res.statusCode, 200, res.body);
  const out = res.json();
  assert.deepEqual(out.created, ["node-a", "node-b"]);
  assert.ok(out.stored.includes("wg-node-a-kk") && out.stored.includes("mihomo-sub-default") && out.stored.includes("mihomo-api-node-a"));
  assert.deepEqual(out.keys, ["tinc-node-a", "tinc-node-b"]);
  assert.equal(s.secrets.meta("wg-node-a-kk")!.publicValue, wg.publicKey);
  assert.deepEqual(s.secrets.meta("mihomo-sub-default")!.nodeScope, ["*"]);
  assert.equal(out.errors, 0, JSON.stringify(out.problems.filter((p: any) => p.severity !== "warning")));
  assert.ok(out.disabled.includes("/nodes/node-a/services/openvpn/instances/0/enabled") && out.disabled.includes("/nodes/node-a/services/openvpn/instances/1/enabled"));

  const draft = (await app.inject({ method: "GET", url: "/api/v1/drafts/mine", headers: admin })).json();
  assert.equal(draft.id, out.draftId, "staged as the account's saved changes");
  const doc = draft.document;
  assert.equal(doc.nodes["node-a"].services.wireguard.instances[0].peers[0].publicKey, peer.publicKey);
  assert.equal(doc.nodes["node-a"].services.wireguard.instances[0].enabled, true);
  assert.equal(doc.nodes["node-a"].services.openvpn.instances[0].enabled, false);

  // what the imported cluster-wide settings compile to
  const pub = s.config.publish(doc, "admin");
  const cfg = (n: string) => s.config.snapshot(n, pub.revision)!.config as any;
  const a = cfg("node-a");
  assert.deepEqual(a.dns.hosts.map((h: any) => h.hostname), ["git.example.test", "localhost"], "cluster DNS hosts on every node");
  assert.equal(a.services.tinc.cipher, "aes-256-cbc");
  assert.equal(a.services.tinc.digest, "sha512");
  assert.deepEqual(a.services.mosdns.ruleFiles[0].fileName, "geosite-cn.txt", "the shared MosDNS configuration");
  assert.equal(a.services.mosdns.enabled, true);
  const kkNeighbor = a.services.frr.externalNeighbors.find((n: any) => n.transport === "wireguard/kk");
  assert.deepEqual(kkNeighbor.filterIn, ["deny 0.0.0.0/0", "permit 0.0.0.0/0 le 32"], "the cluster filter on every eBGP session");
  assert.equal(cfg("node-b").services.mihomo.subscriptions[0].refreshSeconds, 3600, "the node's own refresh interval");
  assert.equal(a.services.mihomo.subscriptions[0].refreshSeconds, 86400);
  await app.close();
});
