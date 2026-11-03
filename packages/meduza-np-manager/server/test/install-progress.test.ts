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
      const binaryProgress = { phase:"downloading", active:true, targetVersion:"v1.19.33", platform:"linux/amd64",
        downloadedBytes:1048576, bytesTotal:4194304, attempt:2, retryAtUnix:null, error:null, updatedAt:"2026-11-03T09:02:47Z" };
      const resources = { supervisedProcesses: 2, packages: [
        { package:"meduza-np-openvpn",diskBytes:8192,dataBytes:4096,memoryRssBytes:32768,
          diskObservedAt: "2026-11-03T09:02:00Z",memoryObservedAt:"2026-11-03T09:02:47Z",processes:[
            { id:"openvpn/lemon",instance:"openvpn/lemon",pid:10,scope:"planner",memoryRssBytes:16384 },
            { id:"openvpn/second",instance:"openvpn/second",pid:11,scope:"planner",memoryRssBytes:16384 },
          ] },
        { package:"meduza-np-updater",diskObservedAt:"2026-11-03T09:02:00Z",memoryObservedAt:"2026-11-03T09:02:47Z",
          reason:"measurement-unavailable",processes:[] },
      ] };
      const reported = await app.inject({ method: "POST", url: "/api/v1/nodes/self/reports", headers: nodeHeaders, payload: {
        reportVersion: 1, nodeId: "gw-a", bootId: "upgrade-boot", sequence: ++sequence, observedAt: new Date().toISOString(),
        sync: { mode: "push", transport: "websocket", behavior: "static", state: "in-sync", backoff: { failureLevel: 0, windowSeconds: 0, probing: false },
          ...(phase === "failed" ? { lastError:"openvpn/lemon: compression unsupported", recoveryError:"rev-000003: dnsmasq: at least one listen address" } : {}) },
        packages: [], features: [], instances: [{ id:"mihomo-pkg/provider", kind:"mihomo-provider", state:"starting", downloadProgress:binaryProgress }], update, resources,
      } });
      assert.equal(reported.statusCode, 200, reported.body);
      assert.equal(reported.json().accepted, true);
      const detail = (await app.inject({ method: "GET", url: "/api/v1/nodes/gw-a", headers })).json();
      const nodes = (await app.inject({ method: "GET", url: "/api/v1/nodes", headers })).json();
      const overview = (await app.inject({ method: "GET", url: "/api/v1/overview", headers })).json();
      assert.deepEqual(detail.report.update, update, `${phase}: raw report`);
      assert.deepEqual(detail.report.instances[0].downloadProgress, binaryProgress, "binary progress is independent of package-group installation progress");
      assert.deepEqual(detail.update, update, `${phase}: node detail`);
      assert.deepEqual(nodes[0].update, update, `${phase}: node list`);
      assert.deepEqual(overview.nodes[0].update, update, `${phase}: overview`);
      assert.deepEqual(detail.report.resources, resources, "schema validation preserves per-instance resource measurements and unknown values");
      assert.deepEqual(detail.resources, resources);
      assert.deepEqual(nodes[0].resources, resources);
      assert.deepEqual(overview.nodes[0].resources, resources);
      if (phase === "failed") {
        assert.equal(detail.report.sync.lastError, "openvpn/lemon: compression unsupported");
        assert.equal(detail.report.sync.recoveryError, "rev-000003: dnsmasq: at least one listen address");
        assert.ok(detail.problems.some((p: { id: string; reason: string }) => p.id === "configuration.recovery" && p.reason.startsWith("rev-000003:")));
      }
    }
  } finally { await app.close(); }
});
