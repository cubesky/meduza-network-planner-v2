// D41 (§14.3.3): a node whose planner runs in a bridge-network container gets a pre-publish warning for features that
// act on host/LAN traffic (routing, TPROXY, mDNS, NAT); the same document on a host-network node does not.

import { test } from "node:test";
import assert from "node:assert/strict";
import { enabledCapabilities, switchOn } from "../src/cluster.js";
import type { PackageManifest } from "../src/packages.js";
import { adminHeaders, startManager } from "./helpers.js";

test("capability switches evaluate like the planner", () => {
  const tproxy = { capability: "mihomo.tproxy", pointer: "/mode", equals: "tproxy" };
  assert.equal(switchOn(tproxy, { mode: "tproxy" }), true);
  assert.equal(switchOn(tproxy, { mode: "proxy" }), false);
  assert.equal(switchOn(tproxy, {}), false);
  const dns = { capability: "mihomo.dns", pointer: "/dns/enabled" };
  assert.equal(switchOn(dns, { dns: { enabled: true } }), true);
  assert.equal(switchOn(dns, { dns: {} }), false);
  const access = { capability: "openvpn.access", pointer: "/instances", item: { kind: "access" } };
  assert.equal(switchOn(access, { instances: [{ kind: "site2site" }, { kind: "access" }] }), true);
  assert.equal(switchOn(access, { instances: [{ kind: "access", enabled: false }] }), false);
  const manifest = { provides: ["mihomo.proxy", "mihomo.tproxy"], capabilitySwitches: [tproxy] } as unknown as PackageManifest;
  assert.deepEqual(enabledCapabilities(manifest, { mode: "proxy" }), ["mihomo.proxy"]);
});

test("bridge-network nodes are warned about host-network features before publishing", async () => {
  const { app, s } = await startManager();
  const headers = await adminHeaders(app);
  for (const id of ["br-a", "host-a", "mac-a"]) {
    assert.equal((await app.inject({ method: "POST", url: "/api/v1/nodes", headers, payload: { id } })).statusCode, 200);
  }
  const report = (mode: string) =>
    JSON.stringify({
      packages: [
        { name: "meduza-np-planner", registration: "registered", provides: ["planner.core", "network.mappings", "network.port-forwards"] },
        { name: "meduza-np-avahi", registration: "registered", provides: ["mdns.avahi", "mdns.bridge"] },
      ],
      system: { arch: "x86_64", os: "linux", container: true, networkMode: mode },
    });
  const now = new Date().toISOString();
  for (const [id, mode] of [["br-a", "bridge"], ["host-a", "host"], ["mac-a", "macvlan"]] as const) {
    s.db.run(
      "INSERT INTO node_reports (node_id, boot_id, sequence, observed_at, received_at, report) VALUES (?, 'boot-1', 1, ?, ?, ?)",
      id, now, now, report(mode),
    );
  }
  const node = { services: { avahi: {} } };
  const doc = { schemaVersion: 1, nodes: { "br-a": node, "host-a": node, "mac-a": node } };
  const res = await app.inject({ method: "POST", url: "/api/v1/validate", headers, payload: { document: doc } });
  assert.equal(res.statusCode, 200, res.body);
  const problems = res.json().problems as Array<{ node?: string; path: string; message: string; severity?: string }>;
  const bridge = problems.filter((p) => p.message.includes("bridge network mode"));
  assert.equal(bridge.length, 1, JSON.stringify(problems));
  assert.equal(bridge[0]!.node, "br-a");
  assert.equal(bridge[0]!.path, "/nodes/br-a/services/avahi");
  assert.equal(bridge[0]!.severity, "warning");
  assert.match(bridge[0]!.message, /mdns\.avahi/);
  await app.close();
});
