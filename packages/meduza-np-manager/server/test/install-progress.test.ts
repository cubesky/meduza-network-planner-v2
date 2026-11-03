import { test } from "node:test";
import assert from "node:assert/strict";
import { adminHeaders, startManager, NodeIdentity } from "./helpers.js";
import { sessionSigningInput } from "../src/nodes.js";
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

test("authenticated node reports carry live and completed upgrade state through detail, list and overview", async () => {
  const { app, s } = await startManager();
  try {
    const headers = await adminHeaders(app);
    await app.inject({ method: "POST", url: "/api/v1/nodes", headers, payload: { id: "gw-a" } });
    const { token } = s.nodes.createEnrollmentToken("gw-a", "test");
    const identity = new NodeIdentity();
    const enrolled = await app.inject({ method: "POST", url: "/api/v1/nodes/enroll", payload: { enrollmentToken: token, publicKey: identity.publicKeyB64() } });
    assert.equal(enrolled.statusCode, 200, enrolled.body);
    const challenge = (await app.inject({ method: "POST", url: "/api/v1/nodes/self/session/challenge", payload: { nodeId: "gw-a" } })).json();
    const session = await app.inject({ method: "POST", url: "/api/v1/nodes/self/session", payload: {
      nodeId: "gw-a", nonce: challenge.nonce, signature: identity.sign(sessionSigningInput("gw-a", challenge.nonce)),
    } });
    assert.equal(session.statusCode, 200, session.body);
    const nodeHeaders = { authorization: `Bearer ${session.json().token}` };
    let sequence = 0;
    for (const phase of ["downloading", "retrying", "verifying", "done", "failed", "rolled-back"]) {
      const terminal = ["done", "failed", "rolled-back"].includes(phase);
      const completed = phase === "done" || phase === "verifying";
      const update = {
        phase, active: !terminal, installedGroupVersion: phase === "done" ? "v0.1.5" : "v0.1.4",
        availableGroupVersion: "v0.1.5", groupVersion: "v0.1.5", currentPackage: "meduza-np-updater",
        downloadedBytes: completed ? 14514061 : 7000000, bytesTotal: 14514061,
        completedPackages: completed ? 14 : 7, totalPackages: 14, pendingPackages: [],
        updatedAt: "2026-11-03T06:57:20Z", retryAtUnix: phase === "retrying" ? 1793689040 : null,
        retryError: phase === "retrying" ? "TLS EOF" : null, error: phase === "failed" ? "registration failed" : null,
        lastCheck: "2026-11-03T09:02:47Z", lastResult: terminal ? { groupVersion: "v0.1.5", outcome: phase, at: "2026-11-03T06:57:20Z" } : null,
      };
      const reported = await app.inject({ method: "POST", url: "/api/v1/nodes/self/reports", headers: nodeHeaders, payload: {
        reportVersion: 1, nodeId: "gw-a", bootId: "upgrade-boot", sequence: ++sequence, observedAt: new Date().toISOString(),
        sync: { mode: "push", transport: "websocket", behavior: "static", state: "in-sync", backoff: { failureLevel: 0, windowSeconds: 0, probing: false } },
        packages: [], features: [], instances: [], update,
      } });
      assert.equal(reported.statusCode, 200, reported.body);
      assert.equal(reported.json().accepted, true);
      const detail = (await app.inject({ method: "GET", url: "/api/v1/nodes/gw-a", headers })).json();
      const nodes = (await app.inject({ method: "GET", url: "/api/v1/nodes", headers })).json();
      const overview = (await app.inject({ method: "GET", url: "/api/v1/overview", headers })).json();
      assert.deepEqual(detail.report.update, update, `${phase}: raw report`);
      assert.deepEqual(detail.update, update, `${phase}: node detail`);
      assert.deepEqual(nodes[0].update, update, `${phase}: node list`);
      assert.deepEqual(overview.nodes[0].update, update, `${phase}: overview`);
    }
  } finally { await app.close(); }
});
