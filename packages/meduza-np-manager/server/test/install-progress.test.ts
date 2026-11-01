import { test } from "node:test";
import assert from "node:assert/strict";
import { adminHeaders, startManager } from "./helpers.js";
import { frrOf } from "../src/compilers/frr.js";
import type { ClusterDocument } from "../src/cluster.js";

test("node details retain live package progress and actionable download failures", async () => {
  const { app, s } = await startManager();
  try {
    const headers = await adminHeaders(app);
    await app.inject({ method: "POST", url: "/api/v1/nodes", headers, payload: { id: "gw-a" } });
    const now = new Date().toISOString();
    const update = { phase: "downloading", active: true, currentPackage: "meduza-np-frr", downloadedBytes: 500,
      bytesTotal: 1000, pendingPackages: ["meduza-np-frr", "meduza-np-tinc"] };
    const report = { packages: [], update };
    s.db.run("INSERT INTO node_reports (node_id, boot_id, sequence, observed_at, received_at, report) VALUES (?, 'boot-1', 1, ?, ?, ?)",
      "gw-a", now, now, JSON.stringify(report));
    let detail = await app.inject({ method: "GET", url: "/api/v1/nodes/gw-a", headers });
    assert.equal(detail.statusCode, 200, detail.body);
    assert.deepEqual(detail.json().update, update);
    const failed = { ...update, phase: "failed", active: false, error: "TLS EOF while downloading meduza-np-frr", nextCheckUnix: 12345 };
    s.db.run("UPDATE node_reports SET report = ? WHERE node_id = ?", JSON.stringify({ ...report, update: failed }), "gw-a");
    detail = await app.inject({ method: "GET", url: "/api/v1/nodes/gw-a", headers });
    assert.deepEqual(detail.json().report.update, failed);
    assert.ok(detail.json().problems.some((p: { id: string; reason: string }) => p.id === "updater.installation" && p.reason.includes("TLS EOF")));
  } finally { await app.close(); }
});

test("manager derives FRR config from Tinc and external BGP without a manual FRR service", () => {
  const doc: ClusterDocument = { schemaVersion: 1, routing: { internalAsn: 65010 }, nodes: {
    mesh: { services: { tinc: { address: "10.42.0.1/24" } } },
    external: { routing: { asn: 65020 }, services: { wireguard: { instances: [{ id: "wan", addresses: ["10.90.0.1/30"],
      peers: [{ id: "remote", allowedIps: ["10.90.0.2/32"], bgp: { remoteAsn: 65100 } }] }] } } },
    disabled: { services: { tinc: { enabled: false } } },
  } };
  assert.equal(frrOf("mesh", doc)?.meshInternal, true);
  assert.equal(frrOf("external", doc)?.localAsn, 65020);
  assert.equal(frrOf("disabled", doc), undefined);
});
