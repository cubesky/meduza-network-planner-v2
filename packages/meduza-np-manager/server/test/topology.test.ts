import { test } from "node:test";
import assert from "node:assert/strict";
import type { Services } from "../src/app.js";
import { topology } from "../src/topology.js";

const neighbor = (id: string, address: string, asn = 65100) => ({ id, address, remoteAsn: asn, transport: `wireguard/${id}` });
const bgp = (id: string, routerId?: string, bgpState = "Established", extra: Record<string, unknown> = {}) => ({
  id: `frr/main/bgp-${id}`, kind: "bgp-neighbor", bgpState, ...(routerId ? { routerId } : {}), ...extra,
});
function fixture(configs: Record<string, any>, reports: Record<string, any[]> = {}) {
  const services = {
    nodes: {
      list: () => Object.keys(configs).map((id) => ({ id, display_name: `name-${id}`, behavior: "static", tags: "[]", public_key: "enrolled", last_seen_at: new Date().toISOString() })),
      latestReport: (id: string) => ({ report: { instances: reports[id] ?? [] } }),
      syncState: () => undefined,
    },
    config: {
      desired: () => ({ revision: "rev-000001" }), snapshot: (id: string) => ({ config: configs[id] }),
      currentDocument: () => ({ revision: "rev-000001" }),
    },
    db: { get: () => undefined }, hub: { connected: () => false, size: 0 }, limiter: { stats: () => ({}) },
  } as unknown as Services;
  return { read: () => topology(services), reports };
}
const node = (routerId: string, externalNeighbors: ReturnType<typeof neighbor>[] = [], asn = 65010) => ({
  node: { routerId }, routing: { internalAsn: 65010 }, services: { frr: { localAsn: asn, externalNeighbors } },
});

test("topology: an established remote router ID merges an external peer into its managed node without tunnel-address equality", () => {
  const f = fixture({ a: node("10.0.0.1", [neighbor("uplink", "203.0.113.1")]), b: node("10.1.0.1", [], 65100) }, {
    a: [bgp("uplink", "10.1.0.1", "Established", { routes: ["192.0.2.0/24"], prefixesReceived: 1 })],
  });
  const t = f.read();
  assert.equal(t.vertices.filter((v) => v.kind === "peer").length, 0);
  assert.equal(t.vertices.find((v) => v.id === "node:b")?.routerId, "10.1.0.1");
  assert.equal(t.vertices.find((v) => v.id === "node:b")?.label, "name-b");
  assert.equal(t.links[0]?.target, "node:b");
  assert.deepEqual(t.links[0]?.sessions[0]?.routes, ["192.0.2.0/24"]);
});

test("topology: two sessions merge after establishment and preserve all addresses, states and AS-path hops", () => {
  const f = fixture({ a: node("10.0.0.1", [neighbor("one", "203.0.113.1"), neighbor("two", "203.0.113.2")]) });
  assert.equal(f.read().vertices.filter((v) => v.kind === "peer").length, 2);
  f.reports.a = [bgp("one", "10.1.0.1", "Established", { hostname: "remote-bgpd", asPaths: [[65100, 65200]] }), bgp("two", "10.1.0.1")];
  const t = f.read();
  const peers = t.vertices.filter((v) => v.kind === "peer");
  assert.equal(peers.length, 1);
  assert.equal(peers[0]?.id, "peer:65100:10.1.0.1");
  assert.deepEqual((peers[0] as any).addresses, ["203.0.113.1", "203.0.113.2"]);
  const l = t.links.find((l) => l.kind === "ebgp")!;
  assert.equal(l.sessions.length, 2);
  assert.equal(l.state, "up");
  assert.equal(t.links.find((l) => l.kind === "path")?.source, peers[0]?.id);
  const previous = JSON.stringify(t);
  f.reports.a.reverse();
  assert.equal(JSON.stringify(f.read()), previous, "heartbeat report order must not change merged identities");
  f.reports.a = [bgp("one", "10.1.0.1", "Active"), bgp("two", "10.1.0.1")];
  assert.equal(f.read().vertices.filter((v) => v.kind === "peer").length, 1, "a retained learned ID remains the same router during reconnect");
  assert.equal(f.read().links.find((l) => l.kind === "ebgp")?.state, "partial");
  f.reports.a = [bgp("one", "10.1.0.1", "Active"), bgp("two", "10.1.0.1", "Active")];
  assert.equal(f.read().vertices.filter((v) => v.kind === "peer").length, 1);
  assert.equal(f.read().links.find((l) => l.kind === "ebgp")?.state, "down");
});

test("topology: a confirmed session address joins its learned identity even when another observer has no router ID", () => {
  const f = fixture({ a: node("10.0.0.1", [neighbor("one", "203.0.113.1")]), b: node("10.0.0.2", [neighbor("two", "203.0.113.1")]) }, {
    a: [bgp("one", "10.1.0.1")], b: [bgp("two", undefined, "Active")],
  });
  const t = f.read();
  assert.equal(t.vertices.filter((v) => v.kind === "peer").length, 1);
  assert.equal(t.links.length, 2);
  assert.deepEqual(t.links.map((l) => l.state).sort(), ["down", "up"]);
});

test("topology: router IDs from another AS, invalid/unknown IDs and duplicate managed IDs cannot collapse nodes", () => {
  for (const routerId of ["10.1.0.1", "0.0.0.0", "::1", "bad-id"]) {
    const f = fixture({ a: node("10.0.0.1", [neighbor("one", "203.0.113.1", 65100), neighbor("two", "203.0.113.2", 65200)]), b: node("10.1.0.1", [], 65300) }, {
      a: [bgp("one", routerId), bgp("two", routerId)],
    });
    assert.equal(f.read().vertices.filter((v) => v.kind === "peer").length, 2);
  }
  const f = fixture({ a: node("10.0.0.1", [neighbor("one", "203.0.113.1"), neighbor("two", "203.0.113.2")]), b: node("10.1.0.1", [], 65100), c: node("10.1.0.1", [], 65100) }, {
    a: [bgp("one", undefined, "Active"), bgp("two", undefined, "Active")],
  });
  assert.equal(f.read().vertices.filter((v) => v.kind === "peer").length, 2, "wait for a learned identity");
  f.reports.a = [bgp("one", "10.1.0.1"), bgp("two", "10.1.0.1")];
  const t = f.read();
  assert.equal(t.vertices.filter((v) => v.kind === "node").length, 3, "never collapse distinct managed node registrations");
  assert.equal(t.vertices.filter((v) => v.kind === "peer").length, 1, "ambiguous managed identity remains a peer");
});

test("topology: FRR router ID overrides the generic node ID when identifying a managed peer", () => {
  const remote = node("10.1.0.9", [], 65100);
  (remote.services.frr as any).routerId = "10.1.0.1";
  const f = fixture({ a: node("10.0.0.1", [neighbor("one", "203.0.113.1")]), b: remote }, { a: [bgp("one", "10.1.0.1")] });
  assert.equal(f.read().links[0]?.target, "node:b");
});

test("topology: reciprocal managed sessions meet on one link using router IDs and retain both observations", () => {
  const f = fixture({ a: node("10.0.0.1", [neighbor("to-b", "203.0.113.1")]), b: node("10.1.0.1", [neighbor("to-a", "198.51.100.1", 65010)], 65100) }, {
    a: [bgp("to-b", "10.1.0.1")], b: [bgp("to-a", "10.0.0.1", "Active")],
  });
  const t = f.read();
  assert.deepEqual(t.vertices.map((v) => v.id), ["node:a", "node:b"]);
  assert.equal(t.links.length, 1);
  assert.equal(t.links[0]?.state, "partial");
  assert.deepEqual(t.links[0]?.sessions.map((s) => s.node), ["a", "b"]);
});

test("topology: overlapping addresses with conflicting learned IDs cannot identify an unknown observer's peer", () => {
  const configs = Object.fromEntries(["a", "b", "c"].map((id, i) => [id, node(`10.0.0.${i + 1}`, [neighbor("one", "203.0.113.1")])]));
  const f = fixture(configs, { a: [bgp("one", "10.1.0.1")], b: [bgp("one", "10.1.0.2")], c: [bgp("one", undefined, "Active")] });
  assert.equal(f.read().vertices.filter((v) => v.kind === "peer").length, 3, "an address conflict must not arbitrarily join either known router");
});
