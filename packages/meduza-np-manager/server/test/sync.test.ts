import { test } from "node:test";
import assert from "node:assert/strict";
import { WebSocket } from "ws";
import { adminHeaders, startManager, NodeIdentity } from "./helpers.js";
import { sessionSigningInput } from "../src/nodes.js";
import { canonicalDigest, canonicalJson } from "../src/canonical.js";
import { overview } from "../src/views.js";

async function enrolledNode(ctx: Awaited<ReturnType<typeof startManager>>, nodeId: string, behavior: "static" | "roaming" = "static") {
  const { app, s } = ctx;
  s.nodes.createNode({ id: nodeId, behavior }, "test");
  const { token } = s.nodes.createEnrollmentToken(nodeId, "test");
  const identity = new NodeIdentity();
  const enroll = await app.inject({ method: "POST", url: "/api/v1/nodes/enroll", payload: { enrollmentToken: token, publicKey: identity.publicKeyB64(), hostname: nodeId, arch: "x86_64", plannerApi: 1 } });
  assert.equal(enroll.statusCode, 200, enroll.body);
  const reuse = await app.inject({ method: "POST", url: "/api/v1/nodes/enroll", payload: { enrollmentToken: token, publicKey: identity.publicKeyB64() } });
  assert.equal(reuse.statusCode, 401, "enrollment tokens are single use");
  const ch = (await app.inject({ method: "POST", url: "/api/v1/nodes/self/session/challenge", payload: { nodeId } })).json();
  const sess = await app.inject({ method: "POST", url: "/api/v1/nodes/self/session", payload: { nodeId, nonce: ch.nonce, signature: identity.sign(sessionSigningInput(nodeId, ch.nonce)) } });
  assert.equal(sess.statusCode, 200, sess.body);
  return { identity, auth: { authorization: `Bearer ${sess.json().token}` } };
}

function report(nodeId: string, bootId: string, sequence: number, packages: Array<{ name: string; provides: string[] }> = []) {
  return {
    reportVersion: 1,
    nodeId,
    bootId,
    sequence,
    observedAt: new Date().toISOString(),
    sync: { mode: "push", transport: "websocket", behavior: "static", state: "in-sync", backoff: { failureLevel: 0, windowSeconds: 0, probing: false } },
    packages: packages.map((p) => ({ name: p.name, version: "0.1.0", role: "service-adapter", manifestDigest: "x", registration: "registered", provides: p.provides })),
    features: [],
    instances: [],
  };
}

test("canonical JSON matches the Rust implementation", () => {
  assert.equal(canonicalJson({ b: 1, a: { d: [1, 2, { z: true, y: null }], c: "x" } }), '{"a":{"c":"x","d":[1,2,{"y":null,"z":true}]},"b":1}');
  assert.equal(canonicalDigest("abc").length, 64);
});

test("node identity: bad signatures and replayed challenges are rejected", async () => {
  const ctx = await startManager();
  const { app } = ctx;
  const { identity } = await enrolledNode(ctx, "gw-a");
  const ch = (await app.inject({ method: "POST", url: "/api/v1/nodes/self/session/challenge", payload: { nodeId: "gw-a" } })).json();
  const other = new NodeIdentity();
  const forged = await app.inject({ method: "POST", url: "/api/v1/nodes/self/session", payload: { nodeId: "gw-a", nonce: ch.nonce, signature: other.sign(sessionSigningInput("gw-a", ch.nonce)) } });
  assert.equal(forged.statusCode, 401);
  const replay = await app.inject({ method: "POST", url: "/api/v1/nodes/self/session", payload: { nodeId: "gw-a", nonce: ch.nonce, signature: identity.sign(sessionSigningInput("gw-a", ch.nonce)) } });
  assert.equal(replay.statusCode, 401, "challenge consumed by the failed attempt");
  const anon = await app.inject({ method: "GET", url: "/api/v1/nodes/self/config" });
  assert.equal(anon.statusCode, 401);
  await app.close();
});

test("publish → config pull with ETag → ACKs → unchanged republish → rollback", async () => {
  const ctx = await startManager();
  const { app, s } = ctx;
  const { auth } = await enrolledNode(ctx, "gw-a");

  const none = await app.inject({ method: "GET", url: "/api/v1/nodes/self/config", headers: auth });
  assert.equal(none.statusCode, 204, "nothing published yet");

  // Enabling a package the node does not have yet is a warning, not a blocker (D20: installed later by the updater
  // or by hand).
  const docWithWg = { schemaVersion: 1, nodes: { "gw-a": { networks: { lan: ["10.42.10.0/24"] }, services: { wireguard: { enabled: true, instances: [] } } } } };
  const pending = s.config.preview(docWithWg).problems;
  assert.ok(pending.length && pending.every((p) => p.severity === "warning"), JSON.stringify(pending));
  assert.match(pending[0]!.message, /has not reported its packages yet; these settings apply once meduza-np-wireguard is installed/);

  const doc = { schemaVersion: 1, nodes: { "gw-a": { displayName: "Gateway A", networks: { lan: ["10.42.10.0/24"] } } } };
  const pub = s.config.publish(doc, "test", { message: "first" });
  assert.equal(pub.revision, "rev-000001");
  assert.deepEqual(pub.nodes, [{ nodeId: "gw-a", nodeRevision: "rev-000001", changed: true }]);

  const cfg = await app.inject({ method: "GET", url: "/api/v1/nodes/self/config", headers: auth });
  assert.equal(cfg.statusCode, 200, cfg.body);
  const body = cfg.json();
  assert.equal(body.config.revision, "rev-000001");
  assert.equal(body.meta.configDigest, canonicalDigest(body.config), "digest covers exactly the delivered config");
  assert.deepEqual(body.config.networks.lan, ["10.42.10.0/24"]);

  const etag = cfg.headers.etag as string;
  const notModified = await app.inject({ method: "GET", url: "/api/v1/nodes/self/config", headers: { ...auth, "if-none-match": etag } });
  assert.equal(notModified.statusCode, 304);

  const ack = { revision: "rev-000001", configDigest: body.meta.configDigest, bootId: "b1" };
  for (const stage of ["received", "validated", "applied"]) {
    const r = await app.inject({ method: "POST", url: "/api/v1/nodes/self/acks", headers: auth, payload: { ...ack, stage } });
    assert.equal(r.statusCode, 200, r.body);
  }
  const dup = await app.inject({ method: "POST", url: "/api/v1/nodes/self/acks", headers: auth, payload: { ...ack, stage: "applied" } });
  assert.equal(dup.json().duplicate, true, "ACKs are idempotent");
  const badDigest = await app.inject({ method: "POST", url: "/api/v1/nodes/self/acks", headers: auth, payload: { ...ack, configDigest: "0".repeat(64), stage: "applied", bootId: "b2" } });
  assert.equal(badDigest.statusCode, 409);
  assert.equal(s.config.publishResults("rev-000001")[0]!.stage, "applied");

  // Same content again: node keeps its revision, no restart-worthy change.
  const again = s.config.publish(doc, "test");
  assert.equal(again.revision, "rev-000002");
  assert.deepEqual(again.nodes, [{ nodeId: "gw-a", nodeRevision: "rev-000001", changed: false }]);

  const changed = s.config.publish({ ...doc, nodes: { "gw-a": { networks: { lan: ["10.42.20.0/24"] } } } }, "test");
  assert.equal(changed.nodes[0]!.nodeRevision, "rev-000003");
  const back = s.config.rollback("rev-000001", "test");
  assert.equal(back.revision, "rev-000004");
  assert.equal(back.nodes[0]!.nodeRevision, "rev-000004", "rollback is a new revision, not a rewrite");
  const old = await app.inject({ method: "GET", url: "/api/v1/nodes/self/config?revision=rev-000001", headers: auth });
  assert.equal(old.json().config.networks.lan[0], "10.42.10.0/24", "published snapshots stay immutable");
  await app.close();
});

test("configuration status: successful ACK clears same-revision failures without losing ACK history", async (t) => {
  const ctx = await startManager();
  t.after(() => ctx.app.close());
  const { s } = ctx;
  await enrolledNode(ctx, "gw-a");
  s.config.publish({ schemaVersion: 1, nodes: { "gw-a": {} } }, "test");
  const desired = s.config.desired("gw-a")!;
  const ack = { revision: desired.revision, configDigest: desired.digest, bootId: "b1" };
  s.nodes.ack("gw-a", { ...ack, stage: "failed", errorCode: "process-failed", message: "old failure" });
  assert.equal(overview(s).nodes[0]!.inSync, false);
  s.nodes.ack("gw-a", { ...ack, stage: "applied" });
  const state = s.nodes.syncState("gw-a")!;
  assert.equal(state.applied_revision, desired.revision);
  assert.equal(state.failed_revision, null);
  assert.equal(state.last_error, null);
  assert.equal(overview(s).nodes[0]!.inSync, true);
  assert.equal(s.db.all("SELECT * FROM acks WHERE node_id = ? AND stage = 'failed'", "gw-a").length, 1, "history is retained");
  for (const stage of ["validated", "received"]) s.nodes.ack("gw-a", { ...ack, stage });
  assert.equal(s.config.publishResults(desired.revision)[0]!.stage, "applied", "late intermediate ACKs cannot regress terminal status");
  assert.equal(s.nodes.ack("gw-a", { ...ack, stage: "applied" }).duplicate, true);
});

test("configuration status: older success and intermediate ACKs cannot erase a newer failure", async (t) => {
  const ctx = await startManager();
  t.after(() => ctx.app.close());
  const { s } = ctx;
  await enrolledNode(ctx, "gw-a");
  s.config.publish({ schemaVersion: 1, nodes: { "gw-a": { networks: { lan: ["192.0.2.0/24"] } } } }, "test");
  const old = s.config.desired("gw-a")!;
  s.config.publish({ schemaVersion: 1, nodes: { "gw-a": { networks: { lan: ["198.51.100.0/24"] } } } }, "test");
  const next = s.config.desired("gw-a")!;
  const newer = { revision: next.revision, configDigest: next.digest, bootId: "b1" };
  s.nodes.ack("gw-a", { ...newer, stage: "failed", errorCode: "apply", message: "new failure" });
  const previous = { revision: old.revision, configDigest: old.digest, bootId: "b1" };
  for (const stage of ["received", "validated", "applied"]) s.nodes.ack("gw-a", { ...previous, stage });
  s.nodes.ack("gw-a", { ...newer, stage: "received" });
  assert.equal(s.nodes.syncState("gw-a")!.failed_revision, next.revision);
  assert.equal(s.nodes.syncState("gw-a")!.last_error, "apply: new failure");
  assert.equal(s.config.publishResults(next.revision)[0]!.stage, "failed");
  assert.equal(overview(s).nodes[0]!.inSync, false);
  s.nodes.ack("gw-a", { ...newer, stage: "applied" });
  s.nodes.ack("gw-a", { ...previous, stage: "rejected", errorCode: "old", message: "late rejection" });
  assert.equal(s.nodes.syncState("gw-a")!.failed_revision, null);
  assert.equal(s.nodes.syncState("gw-a")!.last_error, null);
  assert.equal(overview(s).nodes[0]!.inSync, true);
});

test("configuration status: in-sync reports heal legacy failures despite an unrelated jobs HTTP 429", async (t) => {
  const ctx = await startManager();
  t.after(() => ctx.app.close());
  const { app, s } = ctx;
  const { auth } = await enrolledNode(ctx, "gw-a");
  s.config.publish({ schemaVersion: 1, nodes: { "gw-a": {} } }, "test");
  const desired = s.config.desired("gw-a")!;
  const ack = { revision: desired.revision, configDigest: desired.digest, bootId: "b1" };
  s.nodes.ack("gw-a", { ...ack, stage: "failed", message: "legacy failure" });
  s.nodes.ack("gw-a", { ...ack, stage: "applied" });
  // Simulate rows persisted by the previous Manager; the original applied ACK is already deduplicated.
  s.db.run("UPDATE node_sync SET failed_revision = ?, last_error = ? WHERE node_id = ?", desired.revision, "legacy failure", "gw-a");
  s.db.run("UPDATE publish_results SET stage = 'failed', message = 'legacy failure' WHERE node_id = ?", "gw-a");
  const transportError = "manager unavailable: HTTP 429 on /api/v1/nodes/self/jobs";
  const synced = {
    ...report("gw-a", "b1", 2),
    sync: { ...report("gw-a", "b1", 2).sync, desiredRevision: desired.revision, appliedRevision: desired.revision,
      lastError: transportError, backoff: { failureLevel: 2, windowSeconds: 8, lastFailure: "HTTP 429 on /api/v1/nodes/self/jobs", probing: false } },
  };
  const post = (r: unknown) => app.inject({ method: "POST", url: "/api/v1/nodes/self/reports", headers: auth, payload: r as Record<string, unknown> });
  await post({ ...synced, sequence: 1, sync: { ...synced.sync, desiredRevision: "rev-999999", appliedRevision: "rev-999999" } });
  assert.equal(s.nodes.syncState("gw-a")!.failed_revision, desired.revision, "unpublished snapshots cannot reconcile success");
  const response = await post(synced);
  assert.equal(response.statusCode, 200, response.body);
  assert.equal(response.json().accepted, true);
  assert.equal(s.nodes.syncState("gw-a")!.failed_revision, null);
  assert.equal(s.nodes.syncState("gw-a")!.last_error, null);
  assert.equal(s.config.publishResults(desired.revision)[0]!.stage, "applied");
  const node = overview(s).nodes[0]!;
  assert.equal(node.failedRevision, null);
  assert.equal(node.inSync, true);
  assert.equal(node.sync.lastError, transportError, "transport diagnostics remain available");
  assert.equal(node.sync.backoff.failureLevel, 2);
  assert.equal((await post({ ...synced, sequence: 1 })).json().accepted, false);

  // A genuine failed reapply at the exact same revision must remain visible.
  s.nodes.ack("gw-a", { ...ack, bootId: "b2", stage: "failed", errorCode: "apply", message: "real new failure" });
  await post({ ...synced, bootId: "b2", sequence: 1, sync: { ...synced.sync, state: "apply-failed", lastError: "real new failure" } });
  assert.equal(overview(s).nodes[0]!.failedRevision, desired.revision);
  assert.equal(overview(s).nodes[0]!.inSync, false);
  await post({ ...synced, bootId: "b2", sequence: 2, sync: { ...synced.sync, recoveryError: "partial restore" } });
  assert.equal(s.nodes.syncState("gw-a")!.failed_revision, desired.revision, "partial recovery is not full success");
  await post({ ...synced, bootId: "b2", sequence: 3 });
  assert.equal(overview(s).nodes[0]!.inSync, true);
  s.nodes.ack("gw-a", { ...ack, bootId: "b1", stage: "rejected", message: "old boot" });
  assert.equal(s.nodes.syncState("gw-a")!.failed_revision, null, "known old-boot ACKs only enter history");
  await post({ ...synced, bootId: "b2", sequence: 4, sync: { ...synced.sync, state: "apply-failed", lastError: "failed without ACK" } });
  assert.equal(overview(s).nodes[0]!.failedRevision, desired.revision, "a current failed report also counts when its ACK is lost");
  assert.equal(overview(s).nodes[0]!.lastError, "failed without ACK");
  assert.equal(overview(s).nodes[0]!.inSync, false);
  s.db.run("UPDATE node_sync SET failed_revision = ?, last_error = 'restore failed' WHERE node_id = ?", desired.revision, "gw-a");
  await post({ ...synced, bootId: "b2", sequence: 5, sync: { ...synced.sync, applyStage: "degraded: restore-error" } });
  assert.equal(overview(s).nodes[0]!.sync.applyStage, "degraded: restore-error");
  assert.equal(s.nodes.syncState("gw-a")!.failed_revision, desired.revision);
  assert.equal(overview(s).nodes[0]!.inSync, false);
  await post({ ...synced, bootId: "b2", sequence: 6 });
  assert.equal(overview(s).nodes[0]!.inSync, true);
});

test("reports: stale sequences and old boots never overwrite newer state", async () => {
  const ctx = await startManager();
  const { app, s } = ctx;
  const { auth } = await enrolledNode(ctx, "gw-a");
  const post = (r: unknown) => app.inject({ method: "POST", url: "/api/v1/nodes/self/reports", headers: auth, payload: r as Record<string, unknown> });
  assert.equal((await post(report("gw-a", "boot-1", 5))).json().accepted, true);
  assert.equal((await post(report("gw-a", "boot-1", 4))).json().accepted, false);
  assert.equal((await post(report("gw-a", "boot-2", 1))).json().accepted, true, "new boot restarts sequences");
  assert.equal((await post(report("gw-a", "boot-1", 99))).json().accepted, false, "late report from a previous boot");
  const spoof = await post(report("gw-b", "boot-1", 1));
  assert.equal(spoof.statusCode, 403);
  const invalid = await post({ nodeId: "gw-a" });
  assert.equal(invalid.statusCode, 422);
  assert.equal(s.nodes.latestReport("gw-a")!.report.bootId, "boot-2");
  await app.close();
});

test("secrets reach only the nodes in scope; snapshots carry them encrypted at rest", async () => {
  const ctx = await startManager();
  const { app, s } = ctx;
  const a = await enrolledNode(ctx, "gw-a");
  await enrolledNode(ctx, "gw-b");
  for (const n of ["gw-a", "gw-b"]) {
    const r = await app.inject({ method: "POST", url: "/api/v1/nodes/self/reports", headers: n === "gw-a" ? a.auth : (await enrolledNode(ctx, `${n}-x`)).auth, payload: report(n === "gw-a" ? n : `${n}-x`, "b", 1, [{ name: "meduza-np-mihomo-pkg", provides: ["mihomo.binary"] }]) });
    assert.equal(r.statusCode, 200, r.body);
  }
  s.secrets.put({ id: "proxy-secret", kind: "text", value: "s3cret-value", nodeScope: ["gw-a"] });
  const svc = { "mihomo-pkg": { mode: "manual", apiSecretRef: "proxy-secret" } };
  const ok = s.config.publish({ schemaVersion: 1, nodes: { "gw-a": { services: svc } } }, "test");
  assert.equal(ok.nodes[0]!.changed, true);
  const cfg = (await app.inject({ method: "GET", url: "/api/v1/nodes/self/config", headers: a.auth })).json();
  assert.equal(cfg.config.secrets["proxy-secret"].value, "s3cret-value");
  const row = s.db.get<{ config: string }>("SELECT config FROM node_snapshots WHERE node_id = 'gw-a'")!;
  assert.ok(!row.config.includes("s3cret-value"), "stored snapshot JSON has no plaintext secret");
  assert.throws(() => s.config.publish({ schemaVersion: 1, nodes: { "gw-b-x": { services: svc } } }, "test"), /validation problem/, "secret not shared with gw-b-x");
  await app.close();
});

test("WireGuard and OpenVPN are external-only: own public key filled, managed-node references rejected", async () => {
  const ctx = await startManager();
  const { app, s } = ctx;
  const pkgs = [
    { name: "meduza-np-wireguard", provides: ["wireguard.interfaces", "wireguard.keygen"] },
    { name: "meduza-np-openvpn", provides: ["openvpn.site2site", "openvpn.access"] },
  ];
  for (const n of ["gw-a", "gw-b"]) {
    const { auth } = await enrolledNode(ctx, n);
    const r = await app.inject({ method: "POST", url: "/api/v1/nodes/self/reports", headers: auth, payload: report(n, "b", 1, pkgs) });
    assert.equal(r.statusCode, 200, r.body);
  }
  const { generateWireGuardKeyPair } = await import("../src/keys.js");
  const ka = generateWireGuardKeyPair();
  const ext = generateWireGuardKeyPair();
  s.secrets.put({ id: "wg-gw-a", kind: "wireguard-private-key", value: ka.privateKey, publicValue: ka.publicKey, nodeScope: ["gw-a"] });
  const external = { id: "partner", publicKey: ext.publicKey, endpoint: "partner.example.net:51820", allowedIps: ["192.0.2.0/24"] };
  const wg = (peers: unknown[]) => ({ wireguard: { instances: [{ id: "uplink", privateKeyRef: "wg-gw-a", addresses: ["10.9.0.1/24"], listenPort: 51820, peers }] } });
  const doc = { schemaVersion: 1, nodes: { "gw-a": { services: wg([external]) }, "gw-b": {} } };
  const pub = s.config.publish(doc, "test");
  const snap = s.config.snapshot("gw-a", pub.revision)!;
  const iface = (snap.config.services as any).wireguard.instances[0];
  assert.equal(iface.publicKey, ka.publicKey, "own public key shown/exported");
  assert.deepEqual(iface.peers[0], external, "external peer passed through unchanged");
  assert.deepEqual(Object.keys(snap.config.secrets as object), ["wg-gw-a"], "only the node's own private key is delivered");

  // A WireGuard peer that points at another managed node is a mesh link: refused with the rule, not a schema error.
  const meshy = s.config.preview({ ...doc, nodes: { ...doc.nodes, "gw-a": { services: wg([{ id: "gw-b", node: "gw-b", instance: "uplink" }]) } } });
  assert.ok(meshy.problems.some((p) => /external connections only.*Tinc mesh/.test(p.message)), JSON.stringify(meshy.problems));
  // Same for an OpenVPN Site2Site client referencing a managed server.
  const ovpn = { openvpn: { instances: [{ id: "s2s", kind: "site2site", role: "client", peer: { node: "gw-b", instance: "srv" } }] } };
  const meshyOvpn = s.config.preview({ ...doc, nodes: { ...doc.nodes, "gw-a": { services: ovpn } } });
  assert.ok(meshyOvpn.problems.some((p) => p.path.endsWith("/peer") && /external connections only/.test(p.message)), JSON.stringify(meshyOvpn.problems));
  // Access renegotiation below the plugin's minimum is refused at publish, not only by the node.
  const access = (renegSeconds: number) => ({ openvpn: { instances: [{ id: "acc", kind: "access", tls: { caRef: "ca", certRef: "c", keyRef: "k" }, access: { pool: "10.98.0.0/24", renegSeconds } }] } });
  const reneg = (n: number) => s.config.preview({ ...doc, nodes: { ...doc.nodes, "gw-a": { services: access(n) } } }).problems.filter((p) => p.path.endsWith("/renegSeconds"));
  assert.equal(reneg(60).length, 1, JSON.stringify(reneg(60)));
  assert.deepEqual([reneg(0), reneg(300)], [[], []]);
  await app.close();
});

test("Tinc mesh members receive each other's public material with node-held keys", async () => {
  const ctx = await startManager();
  const { app, s } = ctx;
  const pkg = [{ name: "meduza-np-tinc", provides: ["tinc.mesh", "tinc.keygen"] }];
  for (const n of ["gw-a", "gw-b", "gw_b"]) {
    const { auth } = await enrolledNode(ctx, n);
    await app.inject({ method: "POST", url: "/api/v1/nodes/self/reports", headers: auth, payload: report(n, "b", 1, pkg) });
  }
  const pem = (x: string) => `-----BEGIN RSA PUBLIC KEY-----\nMEgCQQ${x}\n-----END RSA PUBLIC KEY-----`;
  s.secrets.putNodeHeld({ id: "tinc-gw-a", kind: "tinc-rsa-private", node: "gw-a", publicValue: pem("A") });
  s.secrets.putNodeHeld({ id: "tinc-gw-b", kind: "tinc-rsa-private", node: "gw-b", publicValue: pem("B") });
  // One mesh per node (D22): services.tinc is the member itself.
  const inst = (node: string, addr: string, pub?: string[]) => ({ tinc: { privateKeyRef: `tinc-${node}`, address: addr, ...(pub ? { publicAddresses: pub } : {}) } });
  // the list shape of earlier versions is still understood
  const legacy = (node: string, addr: string, pub?: string[]) => ({ tinc: { instances: [{ id: "mesh", privateKeyRef: `tinc-${node}`, address: addr, ...(pub ? { publicAddresses: pub } : {}) }] } });
  const doc = {
    schemaVersion: 1,
    nodes: { "gw-a": { services: inst("gw-a", "10.42.1.1/24", ["gw-a.example.test"]) }, "gw-b": { services: inst("gw-b", "10.42.1.2/24", ["gw-b.example.test"]) } },
  };
  const pub = s.config.publish(doc, "test");
  const a = (s.config.snapshot("gw-a", pub.revision)!.config.services as any).tinc;
  assert.equal(a.publicKey, pem("A"));
  assert.equal(a.instances, undefined, "compiled to the single-mesh shape");
  assert.deepEqual(a.hosts, [{ name: "gw_b", publicKey: pem("B"), addresses: ["gw-b.example.test"], port: 655, subnets: ["10.42.1.2/32"], node: "gw-b" }]);
  assert.equal(s.config.snapshot("gw-a", pub.revision)!.config.secrets, undefined, "node-held keys are not delivered");

  // gw_b would normalize to the same tinc name as gw-b: reported instead of silently merged.
  s.secrets.putNodeHeld({ id: "tinc-gw_b", kind: "tinc-rsa-private", node: "gw_b", publicValue: pem("C") });
  const clash = s.config.preview({ ...doc, nodes: { ...doc.nodes, gw_b: { services: legacy("gw_b", "10.42.1.3/24", ["gw_b.example.test"]) } } });
  assert.ok(clash.problems.some((p) => p.message.includes("collides")), JSON.stringify(clash.problems));

  // The internal mesh runs without NAT between static members: they need directly reachable addresses.
  const noAddr = s.config.preview({ ...doc, nodes: { ...doc.nodes, "gw-b": { services: inst("gw-b", "10.42.1.2/24") } } });
  assert.ok(noAddr.problems.some((p) => p.path.endsWith("/publicAddresses") && /no NAT/.test(p.message)), JSON.stringify(noAddr.problems));
  // D27: a roaming member sits behind NAT and only dials out; it needs no publicAddresses, and the others list it
  // as a host without an address (so nobody tries to connect to it).
  const roaming = s.config.preview({ ...doc, nodes: { ...doc.nodes, "gw-b": { behavior: "roaming" as const, services: inst("gw-b", "10.42.1.2/24") } } });
  assert.deepEqual(roaming.problems.filter((p) => p.node === "gw-b" && p.severity !== "warning"), [], JSON.stringify(roaming.problems));
  const alone = s.config.preview({ ...doc, nodes: { "gw-b": { behavior: "roaming" as const, services: inst("gw-b", "10.42.1.2/24") } } });
  assert.ok(alone.problems.some((p) => p.node === "gw-b" && /no member with publicAddresses/.test(p.message)), JSON.stringify(alone.problems));
  await app.close();
});

test("FRR: iBGP+BFD over Tinc, eBGP over WireGuard/OpenVPN, Anycast over OSPF, roaming peers", async () => {
  const ctx = await startManager();
  const { app, s } = ctx;
  const pkg = [
    { name: "meduza-np-frr", provides: ["frr.bgp", "frr.bfd", "frr.anycast-ospf"] },
    { name: "meduza-np-tinc", provides: ["tinc.mesh", "tinc.keygen"] },
    { name: "meduza-np-wireguard", provides: ["wireguard.interfaces", "wireguard.keygen"] },
  ];
  const pem = (x: string) => `-----BEGIN RSA PUBLIC KEY-----
MEgCQQ${x}
-----END RSA PUBLIC KEY-----`;
  for (const n of ["gw-a", "gw-b", "gw-c", "gw-r"]) {
    const { auth } = await enrolledNode(ctx, n);
    await app.inject({ method: "POST", url: "/api/v1/nodes/self/reports", headers: auth, payload: report(n, "b", 1, pkg) });
    s.secrets.putNodeHeld({ id: `tinc-${n}`, kind: "tinc-rsa-private", node: n, publicValue: pem(n.slice(-1).toUpperCase()) });
  }
  const { generateWireGuardKeyPair } = await import("../src/keys.js");
  const k = generateWireGuardKeyPair();
  s.secrets.put({ id: "wg-ext", kind: "wireguard-private-key", value: k.privateKey, publicValue: k.publicKey, nodeScope: ["gw-a"] });
  const kr = generateWireGuardKeyPair();
  s.secrets.put({ id: "wg-r", kind: "wireguard-private-key", value: kr.privateKey, publicValue: kr.publicKey, nodeScope: ["gw-r"] });
  const tinc = (n: string, ip: string) => ({ privateKeyRef: `tinc-${n}`, address: `${ip}/24`, publicAddresses: [`${n}.example.test`] });
  const frr = (ip: string, extra: Record<string, unknown> = {}) => ({
    meshInternal: true, peeringAddress: ip, bfd: { profiles: [{ id: "mesh" }] }, meshDefaults: { bfdProfile: "mesh" }, ...extra,
  });
  const ebgp = { id: "branch", address: "10.98.0.2", remoteAsn: 65020, transport: "wireguard/ext" };
  const wg = { instances: [{ id: "ext", privateKeyRef: "wg-ext", addresses: ["10.98.0.1/24"], listenPort: 51830, peers: [] }] };
  const wgR = { instances: [{ id: "gw", privateKeyRef: "wg-r", addresses: ["10.98.0.2/24"], peers: [] }] };
  const anycast = { pool: "10.255.0.0/24", services: [{ id: "dns", prefix: "10.255.0.4/30", health: { kind: "dns" as const, port: 53, name: "health.test" }, metric: 20 }] };
  const doc = {
    schemaVersion: 1,
    routing: { internalAsn: 65010, anycast },
    nodes: {
      "gw-a": { routerId: "10.42.1.1", services: { tinc: tinc("gw-a", "10.42.1.1"), wireguard: wg, frr: frr("10.42.1.1", { externalNeighbors: [ebgp] }) } },
      "gw-b": { routerId: "10.42.1.2", services: { tinc: tinc("gw-b", "10.42.1.2"), frr: frr("10.42.1.2", { anycastServices: ["dns"] }) } },
      "gw-c": { routerId: "10.42.1.3", services: { tinc: tinc("gw-c", "10.42.1.3"), frr: frr("10.42.1.3", { meshInternal: false }) } },
      "gw-r": { behavior: "roaming" as const, routerId: "10.98.0.2", services: { wireguard: wgR, frr: { localAsn: 65020, externalNeighbors: [{ id: "hq", address: "10.98.0.1", remoteAsn: 65010, transport: "wireguard/gw" }] } } },
    },
  };
  const pub = s.config.publish(doc, "test");
  const a = (s.config.snapshot("gw-a", pub.revision)!.config.services as any).frr;
  // D25: the iBGP neighbors are always all other Tinc members; gw-c's written meshInternal: false does not opt out.
  assert.deepEqual(a.internalNeighbors, [
    { id: "gw-b", node: "gw-b", address: "10.42.1.2", updateSource: "10.42.1.1", bfdProfile: "mesh" },
    { id: "gw-c", node: "gw-c", address: "10.42.1.3", updateSource: "10.42.1.1", bfdProfile: "mesh" },
  ], "every other mesh member, mesh BFD applied");
  assert.equal(a.externalNeighbors[0].roaming, true, "the peer is a roaming node of the cluster");
  // Anycast: every mesh member is in the OSPF domain over its Tinc device; gw-b also runs the dns service.
  assert.deepEqual(a.anycast, { enabled: true, pool: "10.255.0.0/24", area: "0.0.0.0", interfaces: [{ name: "tnc-mesh", bfdProfile: "mesh" }], instances: [] });
  const b = (s.config.snapshot("gw-b", pub.revision)!.config.services as any).frr;
  assert.deepEqual(b.anycast.instances, [{ id: "dns", prefixes: ["10.255.0.4/30"], service: "dns", health: { kind: "dns", server: "127.0.0.1", port: 53, name: "health.test" }, metric: 20 }]);
  assert.equal(b.anycastServices, undefined, "compiled away");
  const r = (s.config.snapshot("gw-r", pub.revision)!.config.services as any).frr;
  assert.equal(r.anycast, undefined, "a roaming node (no Tinc) is outside the Anycast domain");

  const variant = (node: string, frrExtra: Record<string, unknown>) =>
    s.config.preview({ ...doc, nodes: { ...doc.nodes, [node]: { ...doc.nodes[node as "gw-a"], services: { ...doc.nodes[node as "gw-a"].services, frr: { ...doc.nodes[node as "gw-a"].services.frr, ...frrExtra } } } } }).problems;
  assert.ok(variant("gw-c", { localAsn: 65999, meshInternal: true }).some((p) => p.message.includes("internal ASN 65010")));
  assert.ok(variant("gw-b", { peeringAddress: "192.0.2.9" }).some((p) => /iBGP runs over the internal Tinc mesh/.test(p.message)), "peering must use the Tinc address");
  // Nothing to type in: without peeringAddress (and without localAsn) a mesh member peers on its Tinc address in the
  // internal ASN.
  assert.deepEqual(variant("gw-b", { peeringAddress: undefined, localAsn: undefined }).filter((p) => p.severity !== "warning"), [], "defaults from Tinc + Global settings");
  assert.ok(variant("gw-b", { internalNeighbors: [{ id: "x", address: "192.0.2.7", updateSource: "10.42.1.2" }] }).some((p) => /only them/.test(p.message)), "no iBGP neighbor outside the mesh");
  assert.ok(variant("gw-b", { internalNeighbors: [{ id: "r", node: "gw-r", address: "10.98.0.2", updateSource: "10.42.1.2" }] }).some((p) => /only them/.test(p.message)), "a node outside the mesh is no iBGP neighbor");
  assert.ok(variant("gw-b", { enabled: false }).some((p) => /cannot be disabled on a member/.test(p.message)));
  const tuned = s.config.publish({ ...doc, nodes: { ...doc.nodes, "gw-a": { ...doc.nodes["gw-a"], services: { ...doc.nodes["gw-a"].services, frr: { ...doc.nodes["gw-a"].services.frr, internalNeighbors: [{ id: "b", node: "gw-b", address: "10.42.1.2", updateSource: "10.42.1.1", weight: 7 }] } } } } } as any, "tuned");
  const tunedA = (s.config.snapshot("gw-a", tuned.revision)!.config.services as any).frr.internalNeighbors;
  // D27: a roaming member is in the mesh too: no BFD, short timers, last-resort transit.
  const roamB = s.config.publish({ ...doc, nodes: { ...doc.nodes, "gw-b": { ...doc.nodes["gw-b"], behavior: "roaming", services: { tinc: doc.nodes["gw-b"].services.tinc } } } } as any, "roaming member");
  const toRoaming = (s.config.snapshot("gw-a", roamB.revision)!.config.services as any).frr.internalNeighbors.find((n: any) => n.node === "gw-b");
  assert.deepEqual([toRoaming.roaming, toRoaming.timers, toRoaming.bfdProfile], [true, { keepalive: 3, hold: 9 }, undefined]);
  const fromRoaming = (s.config.snapshot("gw-b", roamB.revision)!.config.services as any).frr;
  assert.ok(fromRoaming.internalNeighbors.every((n: any) => n.timers?.hold === 9 && !n.bfdProfile && !n.roaming), "the roaming member's own sessions");
  assert.equal(fromRoaming.anycast, undefined, "a roaming member stays outside the Anycast (OSPF) domain");
  assert.deepEqual((s.config.snapshot("gw-a", roamB.revision)!.config.services as any).tinc.hosts.find((h: any) => h.node === "gw-b").addresses, ["gw-b.example.test"]);
  assert.deepEqual(tunedA.map((n: any) => [n.node, n.address, n.weight]), [["gw-b", "10.42.1.2", 7], ["gw-c", "10.42.1.3", undefined]], "a written entry only tunes a member's session");
  assert.ok(variant("gw-a", { externalNeighbors: [{ ...ebgp, transport: "tinc/mesh" }] }).some((p) => /WireGuard or OpenVPN/.test(p.message)), "no eBGP over Tinc");
  assert.ok(variant("gw-a", { externalNeighbors: [{ ...ebgp, address: "10.97.0.2" }] }).some((p) => /is not on wireguard\/ext/.test(p.message)), "neighbor must be on the tunnel");
  assert.ok(variant("gw-a", { externalNeighbors: [{ ...ebgp, transport: "wireguard/missing" }] }).some((p) => /not an enabled wireguard instance/.test(p.message)));
  assert.ok(variant("gw-a", { externalNeighbors: [{ ...ebgp, bfdProfile: "mesh" }] }).some((p) => /roaming node/.test(p.message)), "no BFD towards a roaming node");
  assert.ok(variant("gw-r", { externalNeighbors: [{ id: "hq", address: "10.98.0.1", remoteAsn: 65010, transport: "wireguard/gw", bfdProfile: "x" }], bfd: { profiles: [{ id: "x" }] } }).some((p) => /roaming node/.test(p.message)), "no BFD on a roaming node");
  assert.ok(variant("gw-r", { anycastServices: ["dns"] }).some((p) => /roaming node is not part of the Anycast/.test(p.message)));
  assert.ok(variant("gw-b", { anycastServices: ["web"] }).some((p) => /no Anycast service web/.test(p.message)));
  const catalogProblems = (svc: { id: string; prefix: string; health: { kind: "tcp"; port: number } }) => s.config.preview({ ...doc, routing: { ...doc.routing, anycast: { ...anycast, services: [...anycast.services, svc] } } }).problems;
  assert.ok(catalogProblems({ id: "web", prefix: "10.255.0.8/29", health: { kind: "tcp", port: 80 } }).some((p) => /uses a \/30/.test(p.message)));
  assert.ok(catalogProblems({ id: "web", prefix: "10.254.0.8/30", health: { kind: "tcp", port: 80 } }).some((p) => /outside the pool/.test(p.message)));
  assert.ok(catalogProblems({ id: "web", prefix: "10.255.0.4/30", health: { kind: "tcp", port: 80 } }).some((p) => /used by another service/.test(p.message)));
  await app.close();
});

test("FRR from the other settings (D24): iBGP from Tinc, eBGP from WireGuard peers, BFD/ECMP from the cluster", async () => {
  const ctx = await startManager();
  const { app, s } = ctx;
  const pkg = [
    { name: "meduza-np-frr", provides: ["frr.bgp", "frr.bfd", "frr.anycast-ospf"] },
    { name: "meduza-np-tinc", provides: ["tinc.mesh", "tinc.keygen"] },
    { name: "meduza-np-wireguard", provides: ["wireguard.interfaces", "wireguard.keygen"] },
    { name: "meduza-np-openvpn", provides: ["openvpn.site2site"] },
  ];
  const pem = (x: string) => `-----BEGIN RSA PUBLIC KEY-----
MEgCQQ${x}
-----END RSA PUBLIC KEY-----`;
  const auths: Record<string, Record<string, string>> = {};
  for (const n of ["gw-a", "gw-b", "gw-c", "gw-r"]) {
    const { auth } = await enrolledNode(ctx, n);
    auths[n] = auth;
    await app.inject({ method: "POST", url: "/api/v1/nodes/self/reports", headers: auth, payload: report(n, "b", 1, pkg) });
    s.secrets.putNodeHeld({ id: `tinc-${n}`, kind: "tinc-rsa-private", node: n, publicValue: pem(n.slice(-1).toUpperCase()) });
  }
  const { generateWireGuardKeyPair } = await import("../src/keys.js");
  const k = generateWireGuardKeyPair();
  s.secrets.put({ id: "wg-ext", kind: "wireguard-private-key", value: k.privateKey, publicValue: k.publicKey, nodeScope: ["gw-a"] });
  const kr = generateWireGuardKeyPair();
  s.secrets.put({ id: "wg-r", kind: "wireguard-private-key", value: kr.privateKey, publicValue: kr.publicKey, nodeScope: ["gw-r"] });
  const tinc = (n: string, ip: string) => ({ privateKeyRef: `tinc-${n}`, address: `${ip}/24`, publicAddresses: [`${n}.example.test`] });
  const toBranch = { remoteAsn: 65020, keepalive: 3, hold: 9, accept: ["10.64.0.0/24"], exportInternal: true };
  const wg = (bgp: Record<string, unknown> | undefined, allowedIps = ["10.98.0.2/32", "10.64.0.0/24"]) => ({
    instances: [{ id: "ext", privateKeyRef: "wg-ext", addresses: ["10.98.0.1/24"], listenPort: 51830, peers: [{ id: "roam", publicKey: kr.publicKey, allowedIps, ...(bgp ? { bgp } : {}) }] }],
  });
  const wgR = (bgp: Record<string, unknown>) => ({
    instances: [{ id: "gw", privateKeyRef: "wg-r", addresses: ["10.98.0.2/24"], peers: [{ id: "hq", publicKey: k.publicKey, endpoint: "gw-a.example.test:51830", allowedIps: ["10.98.0.1/32", "10.61.0.0/24"], bgp }] }],
  });
  const anycast = { pool: "10.255.0.0/24", services: [{ id: "dns", prefix: "10.255.0.4/30", health: { kind: "tcp" as const, port: 53 } }] };
  const doc = {
    schemaVersion: 1,
    routing: { internalAsn: 65010, anycast, bfd: { transmitInterval: 200 }, maximumPaths: 4 },
    nodes: {
      "gw-a": { routerId: "10.42.1.1", routing: { anycast: ["dns"] }, services: { tinc: tinc("gw-a", "10.42.1.1"), wireguard: wg(toBranch) } },
      "gw-b": { routerId: "10.42.1.2", services: { tinc: tinc("gw-b", "10.42.1.2") } },
      "gw-c": { routerId: "10.42.1.3" },
      "gw-r": { behavior: "roaming" as const, routerId: "10.98.0.2", routing: { asn: 65020 }, services: { wireguard: wgR({ remoteAsn: 65010, exportLan: true }) } },
    },
  } as const;
  const pub = s.config.publish(structuredClone(doc) as any, "test");
  const svc = (n: string) => s.config.snapshot(n, pub.revision)!.config.services as any;
  const a = svc("gw-a").frr;
  assert.equal(a.localAsn, undefined, "mesh members are in the internal AS");
  assert.equal(a.maximumPaths, 4);
  assert.deepEqual(a.internalNeighbors, [{ id: "gw-b", node: "gw-b", address: "10.42.1.2", updateSource: "10.42.1.1", bfdProfile: "default" }], "iBGP to the other mesh members with the cluster BFD timers");
  assert.deepEqual(a.bfd.profiles, [{ id: "default", transmitInterval: 200, receiveInterval: 300, detectMultiplier: 3 }]);
  assert.deepEqual(a.externalNeighbors, [{
    id: "ext-roam", address: "10.98.0.2", remoteAsn: 65020, transport: "wireguard/ext",
    importPolicy: { allowDefault: false, prefixes: [{ prefix: "10.64.0.0/24" }] },
    exportPolicy: { lan: true, internal: true, mappings: false, accessPools: false, defaultRoute: false, extra: [] },
    noTransit: true, noForward: true, timers: { keepalive: 3, hold: 9 }, roaming: true,
  }], "the peer's session, its address taken from the peer's host route");
  assert.equal(a.anycast.instances[0].id, "dns", "Anycast selection from the node's routing");
  assert.equal(svc("gw-a").wireguard.instances[0].peers[0].bgp, undefined, "the session is not passed to the WireGuard plugin");
  assert.equal(svc("gw-b").frr.meshInternal, true, "a Tinc member runs FRR without any FRR settings");
  assert.equal(svc("gw-c").frr, undefined, "no routing implied, no FRR");
  const r = svc("gw-r").frr;
  assert.equal(r.localAsn, 65020);
  assert.equal(r.meshInternal, false);
  assert.equal(r.meshDefaults, undefined, "a roaming node gets no BFD");
  assert.equal(r.externalNeighbors[0].id, "gw-hq");
  assert.equal(r.externalNeighbors[0].address, "10.98.0.1");

  const problems = (patch: (d: any) => void) => {
    const d = structuredClone(doc) as any;
    patch(d);
    return s.config.preview(d).problems.filter((p) => p.severity !== "warning");
  };
  assert.deepEqual(problems(() => {}), []);
  assert.ok(problems((d) => (d.nodes["gw-a"].services.wireguard = wg({ ...toBranch, bfd: true }))).some((p) => p.path.endsWith("/peers/0/bgp/bfd")), "no BFD towards a roaming node");
  assert.ok(problems((d) => (d.nodes["gw-a"].services.wireguard = wg(toBranch, ["10.64.0.0/24"]))).some((p) => p.path.endsWith("/peers/0/bgp/address")), "an address is needed");
  assert.ok(problems((d) => (d.nodes["gw-a"].services.wireguard = wg({ ...toBranch, address: "10.97.0.2" }))).some((p) => /is not on wireguard\/ext/.test(p.message)));
  assert.ok(problems((d) => (d.nodes["gw-b"].routing = { asn: 65999 })).some((p) => p.path === "/nodes/gw-b/routing/asn"), "Tinc members are in the internal AS");
  assert.ok(problems((d) => (d.nodes["gw-b"].routing = { anycast: ["web"] })).some((p) => p.path === "/nodes/gw-b/routing/anycast/0"));
  assert.ok(problems((d) => (d.routing.bfd = { detectMultiplier: 1 })).some((p) => p.path === "/routing/bfd/detectMultiplier"));
  // Mesh BFD can be switched off cluster-wide.
  const offDoc = { ...structuredClone(doc), routing: { ...doc.routing, bfd: { enabled: false } } } as any;
  // plus an unmanaged external peer (AS 65100) on gw-a's interface, for the topology below
  offDoc.nodes["gw-a"].services.wireguard.instances[0].peers.push({ id: "isp", publicKey: generateWireGuardKeyPair().publicKey, allowedIps: ["10.98.0.9/32"], bgp: { remoteAsn: 65100 } });
  const off = s.config.publish(offDoc, "no mesh BFD");
  const offA = (s.config.snapshot("gw-a", off.revision)!.config.services as any).frr;
  assert.equal(offA.meshDefaults, undefined);
  assert.equal(offA.bfd, undefined, "no profile when nothing uses it");

  // Topology (D26): every node is a vertex with its AS (the console clusters by AS), an unmanaged router we peer with
  // is a vertex in its AS, ASes behind it (from its AS paths) are AS vertices; links come from the compiled
  // neighbors, their state from the nodes' FRR reports.
  const bgp = (id: string, address: string, bgpState: string, extra: Record<string, unknown> = {}) => ({ id: `frr/main/bgp-${id}`, kind: "bgp-neighbor", state: bgpState === "Established" ? "up" : "down", address, bgpState, ...extra });
  const send = (n: string, instances: unknown[]) => app.inject({ method: "POST", url: "/api/v1/nodes/self/reports", headers: auths[n], payload: { ...report(n, "b", 2, pkg), instances } });
  assert.equal((await send("gw-a", [bgp("gw-b", "10.42.1.2", "Established", { prefixesReceived: 1, prefixesSent: 2, routes: ["10.62.0.0/24"] }), bgp("ext-roam", "10.98.0.2", "Active"), bgp("ext-isp", "10.98.0.9", "Established", { routerId: "192.0.2.1", hostname: "isp-edge-bgpd", asPaths: [[65100, 65200, 65300], [65100], [65100, 65010]] })])).statusCode, 200);
  assert.equal((await send("gw-b", [bgp("gw-a", "10.42.1.1", "Established")])).statusCode, 200);
  const topo = (await app.inject({ method: "GET", url: "/api/v1/topology", headers: await adminHeaders(app) })).json();
  assert.equal(topo.internalAsn, 65010);
  assert.deepEqual(
    topo.vertices.map((v: any) => v.id).sort(),
    ["as:65200", "as:65300", "node:gw-a", "node:gw-b", "node:gw-c", "node:gw-r", "peer:65100:192.0.2.1"],
    "the router we peer with is a vertex, ASes behind it are AS vertices",
  );
  const isp = topo.vertices.find((v: any) => v.id === "peer:65100:192.0.2.1");
  assert.deepEqual([isp.kind, isp.label, isp.asn, isp.address], ["peer", "isp-edge", 65100, "10.98.0.9"], "named by its BGP hostname");
  const asn = (id: string) => topo.vertices.find((v: any) => v.id === id).asn;
  assert.deepEqual([asn("node:gw-a"), asn("node:gw-b"), asn("node:gw-r"), asn("node:gw-c")], [65010, 65010, 65020, undefined], "nodes carry their AS");
  const byId = new Map(topo.links.map((l: any) => [l.id, l]));
  const ib: any = byId.get("ibgp:node:gw-a|node:gw-b");
  assert.equal(ib.state, "up", "both sides report Established");
  assert.deepEqual(ib.sessions.find((x: any) => x.node === "gw-a").routes, ["10.62.0.0/24"]);
  const branchLink: any = byId.get("ebgp:node:gw-a|node:gw-r");
  assert.equal(branchLink.state, "down");
  assert.deepEqual(branchLink.sessions.map((x: any) => x.node).sort(), ["gw-a", "gw-r"], "both ends managed: one link with both sides' sessions");
  assert.equal((byId.get("ebgp:node:gw-a|peer:65100:192.0.2.1") as any).sessions[0].remoteAs, 65100, "an unmanaged peer ends at its router");
  assert.deepEqual(
    [...byId.keys()].filter((k) => String(k).startsWith("path:")).sort(),
    ["path:as:65200|as:65300", "path:peer:65100:192.0.2.1|as:65200"],
    "AS path hops in order; a path back into a managed AS stops there",
  );
  assert.deepEqual((byId.get("path:peer:65100:192.0.2.1|as:65200") as any).via, ["gw-a"]);
  assert.ok(![...byId.keys()].some((k) => String(k).includes("as:65010") || String(k).startsWith("provides")));
  const edge = (id: string) => topo.vertices.find((v: any) => v.id === id).edge === true;
  assert.deepEqual([edge("node:gw-a"), edge("node:gw-b"), edge("node:gw-r")], [true, false, true], "nodes with eBGP sessions are edge routers");
  assert.equal([...byId.keys()].filter((k) => String(k).startsWith("ibgp:")).length, 1, "gw-c (no Tinc) has no iBGP link");
  await app.close();
});

test("joining the mesh from the console alone (D28): pending key, install, generation, distribution", async () => {
  const ctx = await startManager();
  const { app, s } = ctx;
  const tincPkg = { name: "meduza-np-tinc", provides: ["tinc.mesh", "tinc.keygen"] };
  const frrPkg = { name: "meduza-np-frr", provides: ["frr.bgp", "frr.bfd", "frr.anycast-ospf"] };
  const pem = (x: string) => `-----BEGIN RSA PUBLIC KEY-----\nMEgCQQ${x}\n-----END RSA PUBLIC KEY-----`;
  const a = await enrolledNode(ctx, "gw-a");
  const b = await enrolledNode(ctx, "gw-b");
  let seq = 1;
  const send = (auth: Record<string, string>, node: string, pkgs: Array<{ name: string; provides: string[] }>) =>
    app.inject({ method: "POST", url: "/api/v1/nodes/self/reports", headers: auth, payload: report(node, "b", seq++, pkgs) });
  await send(a.auth, "gw-a", [tincPkg, frrPkg]);
  await send(b.auth, "gw-b", [frrPkg]); // gw-b has no Tinc package yet
  s.secrets.putNodeHeld({ id: "tinc-gw-a", kind: "tinc-rsa-private", node: "gw-a", publicValue: pem("A") });
  const admin = await adminHeaders(app);

  // 1. Generate on gw-b: the package is missing, so the key is only recorded as pending.
  const req = await app.inject({ method: "POST", url: "/api/v1/nodes/gw-b/keys", headers: admin, payload: { secretId: "tinc-gw-b", keyKind: "tinc-rsa-private", package: "meduza-np-tinc" } });
  assert.deepEqual(req.json(), { pending: true, jobId: null });
  assert.equal(s.nodes.jobs("gw-b").length, 0, "no job before the package is there");

  // 2. The configuration applies anyway: gw-b receives its Tinc settings (so its updater installs the package),
  // gw-a does not list gw-b yet, and that is a warning only.
  const tinc = (n: string, ip: string, pub: string) => ({ privateKeyRef: `tinc-${n}`, address: `${ip}/24`, publicAddresses: [pub] });
  const doc = {
    schemaVersion: 1,
    nodes: { "gw-a": { services: { tinc: tinc("gw-a", "10.42.1.1", "gw-a.example.test") } }, "gw-b": { services: { tinc: tinc("gw-b", "10.42.1.2", "gw-b.example.test") } } },
  };
  const first = s.config.publish(doc as any, "alice");
  const preview = s.config.preview(doc as any);
  assert.ok(preview.problems.some((p) => p.severity === "warning" && /joins once its tinc key is generated/.test(p.message)), JSON.stringify(preview.problems));
  const hostsOf = (node: string) => ((s.config.snapshot(node, s.config.desired(node)!.revision)!.config.services as any).tinc.hosts as any[]).map((h) => h.node);
  assert.deepEqual(hostsOf("gw-a"), [], "gw-b joins once its key exists");
  assert.equal((s.config.snapshot("gw-b", first.revision)!.config.services as any).tinc.privateKeyRef, "tinc-gw-b");

  // 3. gw-b reports the package (its updater installed it): the generation job is queued by the manager.
  await send(b.auth, "gw-b", [frrPkg, tincPkg]);
  const jobs = () => s.nodes.jobs("gw-b").filter((j) => j.kind === "generate-keypair");
  assert.equal(jobs().length, 1);
  assert.equal(jobs()[0]!.created_by, "system");
  // A report while the job is on its way does not queue another one.
  await send(b.auth, "gw-b", [frrPkg, tincPkg]);
  assert.equal(jobs().length, 1);

  // 4. The first attempt ran too early (package not ready): it is retried.
  const result = (id: string, body: Record<string, unknown>) => app.inject({ method: "POST", url: `/api/v1/nodes/self/jobs/${id}/result`, headers: b.auth, payload: body });
  assert.equal((await result(String(jobs()[0]!.id), { ok: false, errorCode: "not-installed", message: "meduza-np-tinc is not registered" })).statusCode, 200);
  assert.equal(jobs().length, 2, "queued again");

  // 5. The key arrives: recorded, the request ends, and a new revision distributes it to gw-a.
  const retry = jobs().find((j) => j.status !== "failed")!;
  assert.equal((await result(String(retry.id), { ok: true, result: { publicKey: pem("B") } })).statusCode, 200);
  assert.equal(s.secrets.meta("tinc-gw-b")!.publicValue, pem("B"));
  assert.deepEqual(s.nodeKeys.pending("gw-b"), []);
  const current = s.config.currentDocument();
  assert.notEqual(current.revision, first.revision, "published again by the manager");
  assert.deepEqual(current.document, doc, "the document itself is unchanged");
  assert.deepEqual(hostsOf("gw-a"), ["gw-b"], "gw-a now lists gw-b");

  // Keys already generated are not requested again; secrets of other kinds or holders are refused.
  assert.deepEqual((await app.inject({ method: "POST", url: "/api/v1/nodes/gw-b/keys", headers: admin, payload: { secretId: "tinc-gw-b", keyKind: "tinc-rsa-private", package: "meduza-np-tinc" } })).json(), { pending: false, jobId: null });
  assert.equal((await app.inject({ method: "POST", url: "/api/v1/nodes/gw-b/keys", headers: admin, payload: { secretId: "tinc-gw-a", keyKind: "tinc-rsa-private", package: "meduza-np-tinc" } })).statusCode, 400);
  assert.equal((await app.inject({ method: "POST", url: "/api/v1/nodes/gw-b/keys", headers: admin, payload: { secretId: "x", keyKind: "text", package: "meduza-np-tinc" } })).statusCode, 400);
  await app.close();
});

test("Mihomo subscriptions (D29): global list and default, node override, refresh jobs", async () => {
  const ctx = await startManager();
  const { app, s } = ctx;
  const nodes: Record<string, Record<string, string>> = {};
  for (const n of ["gw-a", "gw-b", "gw-c"]) nodes[n] = (await enrolledNode(ctx, n)).auth;
  s.secrets.put({ id: "mihomo-sub-main", kind: "text", value: "https://sub.example.test/main?token=1", nodeScope: ["*"] });
  s.secrets.put({ id: "mihomo-sub-alt", kind: "text", value: "https://sub.example.test/alt?token=2", nodeScope: ["*"] });
  for (const n of ["gw-a", "gw-b"]) s.secrets.put({ id: `api-${n}`, kind: "text", value: `secret-${n}`, nodeScope: [n] });
  const mihomo = (n: string, extra: Record<string, unknown> = {}) => ({ mode: "proxy", api: { secretRef: `api-${n}` }, ...extra });
  const doc = {
    schemaVersion: 1,
    mihomo: {
      subscriptions: [{ id: "main", name: "Main", urlRef: "mihomo-sub-main" }, { id: "alt", urlRef: "mihomo-sub-alt" }],
      default: "main",
      refreshSeconds: 600,
    },
    nodes: {
      "gw-a": { services: { mihomo: mihomo("gw-a") } },
      "gw-b": { services: { mihomo: mihomo("gw-b", { subscription: "alt" }) } },
      "gw-c": {},
    },
  };
  const pub = s.config.publish(structuredClone(doc) as any, "alice");
  const snap = (n: string) => s.config.snapshot(n, pub.revision)!;
  const a = (snap("gw-a").config.services as any).mihomo;
  assert.deepEqual([a.subscriptions, a.activeSubscription, a.subscription], [[{ id: "main", name: "Main", urlRef: "mihomo-sub-main", refreshSeconds: 600 }], "main", undefined], "the default");
  const delivered = (await app.inject({ method: "GET", url: "/api/v1/nodes/self/config", headers: nodes["gw-a"] })).json().config.secrets;
  assert.equal(delivered["mihomo-sub-main"].value, "https://sub.example.test/main?token=1", "the chosen URL is delivered (scope *)");
  assert.equal(delivered["mihomo-sub-alt"], undefined, "only the chosen one");
  const b = (snap("gw-b").config.services as any).mihomo;
  assert.deepEqual([b.subscriptions[0].id, b.activeSubscription], ["alt", "alt"], "the node's override");

  const problems = (patch: (d: any) => void) => {
    const d = structuredClone(doc) as any;
    patch(d);
    return s.config.preview(d).problems.filter((p) => p.severity !== "warning").map((p) => p.path);
  };
  assert.deepEqual(problems(() => {}), []);
  assert.ok(problems((d) => (d.mihomo.default = "gone")).includes("/mihomo/default"));
  assert.ok(problems((d) => (d.nodes["gw-b"].services.mihomo.subscription = "gone")).includes("/nodes/gw-b/services/mihomo/subscription"));
  assert.ok(problems((d) => (d.mihomo.refreshSeconds = 30)).includes("/mihomo/refreshSeconds"));
  assert.ok(problems((d) => (d.mihomo.subscriptions[1].id = "main")).includes("/mihomo/subscriptions/1/id"));

  // Refresh: all nodes running Mihomo, or the users of one subscription.
  const admin = await adminHeaders(app);
  const all = (await app.inject({ method: "POST", url: "/api/v1/mihomo/refresh", headers: admin, payload: {} })).json();
  assert.deepEqual(all.jobs.map((j: any) => j.node), ["gw-a", "gw-b"]);
  const alt = (await app.inject({ method: "POST", url: "/api/v1/mihomo/refresh", headers: admin, payload: { subscription: "alt" } })).json();
  assert.deepEqual(alt.jobs.map((j: any) => j.node), ["gw-b"]);
  const job = s.nodes.jobs("gw-b").find((j) => j.kind === "run-action")!;
  assert.deepEqual([job.package, JSON.parse(String(job.params)).action], ["meduza-np-mihomo", "refresh-subscription"]);
  await app.close();
});

test("WebSocket push: hello/welcome, live config.available, cursor resume", async () => {
  const ctx = await startManager();
  const { app, s } = ctx;
  const { auth } = await enrolledNode(ctx, "gw-a");
  await app.listen({ port: 0, host: "127.0.0.1" });
  const port = (app.server.address() as { port: number }).port;

  const unauth = new WebSocket(`ws://127.0.0.1:${port}/api/v1/nodes/self/stream`);
  const code = await new Promise<number>((resolve) => unauth.on("unexpected-response", (_req, res) => resolve(res.statusCode ?? 0)));
  assert.equal(code, 401);

  const ws = new WebSocket(`ws://127.0.0.1:${port}/api/v1/nodes/self/stream`, { headers: auth });
  const messages: Array<Record<string, unknown>> = [];
  const next = () => new Promise<Record<string, unknown>>((resolve) => {
    if (messages.length) return resolve(messages.shift()!);
    ws.once("message", () => resolve(messages.shift()!));
  });
  ws.on("message", (d) => messages.push(JSON.parse(String(d))));
  await new Promise((r) => ws.on("open", r));
  ws.send(JSON.stringify({ helloVersion: 1 }));
  const welcome = await next();
  assert.equal(welcome.desiredRevision, null);

  s.config.publish({ schemaVersion: 1, nodes: { "gw-a": {} } }, "test");
  const ev = await next();
  assert.equal(ev.type, "config.available");
  assert.equal(ev.revision, "rev-000001");
  ws.close();

  // Reconnect with the cursor: nothing missed → no replay; with an older cursor the event is replayed.
  const ws2 = new WebSocket(`ws://127.0.0.1:${port}/api/v1/nodes/self/stream`, { headers: auth });
  const got: Array<Record<string, unknown>> = [];
  ws2.on("message", (d) => got.push(JSON.parse(String(d))));
  await new Promise((r) => ws2.on("open", r));
  ws2.send(JSON.stringify({ helloVersion: 1, lastEventId: "0" }));
  await new Promise((r) => setTimeout(r, 200));
  assert.equal(got[0]!.desiredRevision, "rev-000001");
  assert.equal(got[0]!.cursorValid, true);
  assert.equal(got[1]!.type, "config.available");
  ws2.close();

  const ws3 = new WebSocket(`ws://127.0.0.1:${port}/api/v1/nodes/self/stream`, { headers: auth });
  const got3: Array<Record<string, unknown>> = [];
  ws3.on("message", (d) => got3.push(JSON.parse(String(d))));
  await new Promise((r) => ws3.on("open", r));
  ws3.send(JSON.stringify({ helloVersion: 1, lastEventId: "999" }));
  await new Promise((r) => setTimeout(r, 200));
  assert.equal(got3[0]!.cursorValid, false, "unknown cursor forces a full HTTPS fetch");
  ws3.close();
  await app.close();
});

test("overload answers 429 with Retry-After instead of queueing", async () => {
  const ctx = await startManager({ PER_NODE_REQUESTS_PER_MINUTE: "4" });
  const { app } = ctx;
  const { auth } = await enrolledNode(ctx, "gw-a");
  let limited = null;
  for (let i = 0; i < 10 && !limited; i++) {
    const r = await app.inject({ method: "GET", url: "/api/v1/nodes/self/config", headers: auth });
    if (r.statusCode === 429) limited = r;
  }
  assert.ok(limited, "rate limit reached");
  assert.ok(Number(limited.headers["retry-after"]) >= 1);
  await app.close();
});

test("DNS loops on a node are refused before publishing (front end, backends, upstreams, nameservers)", async () => {
  const ctx = await startManager();
  const { s } = ctx;
  await enrolledNode(ctx, "gw-a");
  s.secrets.put({ id: "api-gw-a", kind: "text", value: "secret", nodeScope: ["gw-a"] });
  // the lab's front end on lab-b: dnsmasq in front of MosDNS and Mihomo DNS, fallback on another host
  const doc = {
    schemaVersion: 1,
    nodes: {
      "gw-a": {
        services: {
          dnsmasq: {
            listenAddresses: ["10.62.0.12"],
            fallbackServers: ["10.50.0.30#5353"],
          },
          mosdns: { listen: "127.0.0.1", port: 1153, localUpstreams: ["udp://10.50.0.30:53"] },
          mihomo: { mode: "proxy", api: { secretRef: "api-gw-a" }, dns: { enabled: true, listen: "127.0.0.1:1053", nameservers: ["udp://10.50.0.30:5354"], defaultNameservers: ["10.50.0.30"] } },
        },
      },
    },
  };
  const problems = (patch: (svc: any) => void) => {
    const d = structuredClone(doc) as any;
    patch(d.nodes["gw-a"].services);
    return s.config.preview(d).problems.filter((p) => p.severity !== "warning").map((p) => p.path);
  };
  assert.deepEqual(problems(() => {}), []);
  const at = (p: string) => `/nodes/gw-a/services/${p}`;
  assert.deepEqual(problems((v) => (v.mosdns.localUpstreams = ["udp://10.62.0.12"])), [at("mosdns/localUpstreams/0")], "the lab's loop");
  assert.deepEqual(problems((v) => (v.mosdns.localUpstreams = ["tls://10.62.0.12"])), [], "DoT does not reach the plain listener");
  assert.deepEqual(problems((v) => (v.dnsmasq.fallbackServers = ["10.62.0.12#53"])), [at("dnsmasq/fallbackServers/0")]);
  assert.deepEqual(problems((v) => (v.mihomo.dns.nameservers = ["10.62.0.12"])), [at("mihomo/dns/nameservers/0")]);
  assert.deepEqual(problems((v) => (v.mihomo.dns.defaultNameservers = ["10.62.0.12"])), [at("mihomo/dns/defaultNameservers/0")]);
  for (const field of ["fallback", "proxyServerNameservers", "directNameservers"]) {
    assert.deepEqual(problems((v) => (v.mihomo.dns[field] = ["tcp://10.62.0.12:53#DIRECT"])), [at(`mihomo/dns/${field}/0`)]);
  }
  for (const field of ["nameserverPolicy", "proxyServerNameserverPolicy"]) {
    assert.deepEqual(problems((v) => (v.mihomo.dns[field] = { "+.example/test": "10.62.0.12" })), [at(`mihomo/dns/${field}/+.example~1test`)]);
    assert.deepEqual(problems((v) => (v.mihomo.dns[field] = { "+.example/test": ["127.0.0.1:1053"] })), [at(`mihomo/dns/${field}/+.example~1test`)]);
  }
  assert.deepEqual(problems((v) => {
    delete v.dnsmasq;
    v.mihomo.dns.listen = "0.0.0.0:1053";
    v.mihomo.dns.nameservers = ["tcp://localhost:1053"];
  }), [at("mihomo/dns/nameservers/0")], "self-loop check works without dnsmasq");
  assert.deepEqual(problems((v) => {
    v.dnsmasq.enabled = false;
    v.mihomo.dns.nameservers = ["10.62.0.12"];
  }), [], "disabled front end does not create a loop");
  assert.deepEqual(problems((v) => (v.mihomo.dns.respectRules = true)), [at("mihomo/dns/proxyServerNameservers")]);
  assert.deepEqual(problems((v) => {
    v.mihomo.dns.nameservers = [];
    v.mihomo.dns.respectRules = true;
  }), [], "missing node upstreams inherit subscription DNS; validate its proxy resolvers after download");
  assert.deepEqual(problems((v) => {
    v.mihomo.dns.respectRules = true;
    v.mihomo.dns.proxyServerNameservers = ["119.29.29.29"];
  }), []);
  assert.deepEqual(problems((v) => (v.mihomo.dns.enabled = false, v.mihomo.dns.nameservers = ["10.62.0.12"])), [], "Mihomo DNS off");
  // a wildcard front end is reached over loopback too
  assert.deepEqual(problems((v) => { v.dnsmasq.listenAddresses = []; v.mosdns.localUpstreams = ["udp://127.0.0.1:53"]; }), [at("mosdns/localUpstreams/0")], "empty listeners mean all local interfaces");
  assert.deepEqual(problems((v) => { delete v.dnsmasq.listenAddresses; v.mosdns.localUpstreams = ["udp://127.0.0.1:53"]; }), [at("mosdns/localUpstreams/0")], "omitted listeners also mean all local interfaces");
  assert.deepEqual(
    problems((v) => ((v.dnsmasq.listenAddresses = ["0.0.0.0"]), (v.mosdns.localUpstreams = ["udp://127.0.0.1:53"]))),
    [at("mosdns/localUpstreams/0")],
  );
  assert.deepEqual(problems((v) => ((v.dnsmasq.port = 1153), (v.dnsmasq.listenAddresses = ["127.0.0.1"]))).sort(), [at("mosdns/port")]);
  assert.deepEqual(problems((v) => {
    v.dnsmasq.backends = [{id:"obsolete", kind:"mihomo", address:"10.62.0.12", port:53}];
    delete v.dnsmasq.fallbackServers;
  }), [], "legacy manual backend addresses are discarded and fallback has defaults");
});

test("diagnostics job results stay valid JSON: an oversized support bundle becomes a marker", async () => {
  const ctx = await startManager();
  const { app, s } = ctx;
  const admin = await adminHeaders(app);
  const b = await enrolledNode(ctx, "gw-a");
  const ask = async () => {
    const r = await app.inject({ method: "POST", url: "/api/v1/nodes/gw-a/jobs", headers: admin, payload: { kind: "collect-diagnostics" } });
    assert.equal(r.statusCode, 200, r.body);
    return String(r.json().jobId ?? r.json().id);
  };
  const post = (id: string, result: unknown) =>
    app.inject({ method: "POST", url: `/api/v1/nodes/self/jobs/${id}/result`, headers: b.auth, payload: { ok: true, result } });
  const stored = (id: string) => JSON.parse(String(s.nodes.jobs("gw-a").find((j) => j.id === id)!.result));

  const small = await ask();
  const bundle = { kind: "meduza-np-support-bundle", formatVersion: 1, logs: ["planner started"] };
  assert.equal((await post(small, bundle)).statusCode, 200);
  assert.deepEqual(stored(small), bundle);

  const big = await ask();
  const huge = { kind: "meduza-np-support-bundle", logs: ["x".repeat(300_000)] };
  assert.equal((await post(big, huge)).statusCode, 200);
  assert.deepEqual(stored(big), { truncated: true, bytes: JSON.stringify(huge).length });
  await app.close();
});
