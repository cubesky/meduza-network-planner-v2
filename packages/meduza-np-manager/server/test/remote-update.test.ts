import { test } from "node:test";
import assert from "node:assert/strict";
import { adminHeaders, startManager, NodeIdentity } from "./helpers.js";
import { sessionSigningInput } from "../src/nodes.js";

test("remote upgrades are authorized, audited, bounded, delivered and acknowledged separately from completion", async () => {
  const { app, s } = await startManager();
  try {
    const headers = await adminHeaders(app);
    await app.inject({ method: "POST", url: "/api/v1/nodes", headers, payload: { id: "gw-a" } });
    const identity = new NodeIdentity();
    const { token } = s.nodes.createEnrollmentToken("gw-a", "test");
    await app.inject({ method: "POST", url: "/api/v1/nodes/enroll", payload: { enrollmentToken: token, publicKey: identity.publicKeyB64() } });
    const challenge = (await app.inject({ method: "POST", url: "/api/v1/nodes/self/session/challenge", payload: { nodeId: "gw-a" } })).json();
    const session = await app.inject({ method: "POST", url: "/api/v1/nodes/self/session", payload: {
      nodeId: "gw-a", nonce: challenge.nonce, signature: identity.sign(sessionSigningInput("gw-a", challenge.nonce)),
    } });
    const nodeHeaders = { authorization: `Bearer ${session.json().token}` };
    const url = "/api/v1/nodes/gw-a/jobs";
    assert.equal((await app.inject({ method: "POST", url, headers, payload: { kind: "update-apply" } })).json().error, "update-unsupported");
    const update = { remoteControl: true, phase: "done", active: false, installedGroupVersion: "v0.1.7", availableGroupVersion: "v0.1.8" };
    const reported = await app.inject({ method: "POST", url: "/api/v1/nodes/self/reports", headers: nodeHeaders, payload: {
      reportVersion: 1, nodeId: "gw-a", bootId: "boot-1", sequence: 1, observedAt: new Date().toISOString(),
      sync: { mode: "push", transport: "websocket", behavior: "static", state: "in-sync", backoff: { failureLevel: 0, windowSeconds: 0, probing: false } },
      packages: [], features: [], instances: [], update,
    } });
    assert.equal(reported.statusCode, 200, reported.body);
    assert.equal((await app.inject({ method: "POST", url, payload: { kind: "update-apply" } })).statusCode, 403);
    assert.equal((await app.inject({ method: "POST", url, headers: { cookie: headers.cookie! }, payload: { kind: "update-apply" } })).statusCode, 403);
    s.db.run("UPDATE accounts SET role = 'viewer' WHERE username = 'alice'");
    assert.equal((await app.inject({ method: "POST", url, headers, payload: { kind: "update-apply" } })).statusCode, 403);
    s.db.run("UPDATE accounts SET role = 'operator' WHERE username = 'alice'");
    for (const extra of [{ params: { method: "planner.restart" } }, { params: { source: "http://unsafe" } }, { package: "meduza-np-cli" }, { instance: "x" }, { params: [] }]) {
      const rejected = await app.inject({ method: "POST", url, headers, payload: { kind: "update-apply", ...extra } });
      assert.equal(rejected.statusCode, 400, rejected.body);
    }
    const created = await app.inject({ method: "POST", url, headers, payload: { kind: "update-apply" } });
    assert.equal(created.statusCode, 200, created.body);
    const id = created.json().jobId as string;
    const duplicate = await app.inject({ method: "POST", url, headers, payload: { kind: "update-apply" } });
    assert.equal(duplicate.json().jobId, id, "repeated clicks must not create multiple pending upgrades");
    assert.equal((await app.inject({ method: "POST", url, headers, payload: { kind: "update-check" } })).json().error, "update-pending");
    assert.equal(s.db.get<{ n: number }>("SELECT COUNT(*) AS n FROM audit WHERE action = 'job.create' AND target = ?", id)?.n, 1);
    const pending = (await app.inject({ method: "GET", url: "/api/v1/nodes/self/jobs", headers: nodeHeaders })).json();
    assert.equal(pending.length, 1);
    assert.equal(pending[0].jobId, id);
    assert.equal(pending[0].kind, "update-apply");
    assert.deepEqual(pending[0].params, {});
    assert.ok(Date.parse(pending[0].expiresAt) - Date.now() <= 600_000);
    const ack = await app.inject({ method: "POST", url: `/api/v1/nodes/self/jobs/${id}/result`, headers: nodeHeaders, payload: { ok: true, result: { scheduled: true } } });
    assert.equal(ack.statusCode, 200, ack.body);
    let detail = (await app.inject({ method: "GET", url: "/api/v1/nodes/gw-a", headers })).json();
    assert.equal(detail.jobs[0].status, "succeeded");
    assert.deepEqual(detail.update, update, "request acknowledgement must not fabricate an installed group or completion report");
    const check = await app.inject({ method: "POST", url, headers, payload: { kind: "update-check" } });
    const checkId = check.json().jobId;
    assert.equal(check.statusCode, 200, check.body);
    await app.inject({ method: "POST", url: `/api/v1/nodes/self/jobs/${checkId}/result`, headers: nodeHeaders, payload: { ok: false, errorCode: "updater-unavailable", message: "updater.sock missing" } });
    detail = (await app.inject({ method: "GET", url: "/api/v1/nodes/gw-a", headers })).json();
    const failed = detail.jobs.find((j: { id: string }) => j.id === checkId);
    assert.equal(failed.status, "failed");
    assert.equal(failed.error_code, "updater-unavailable");
    assert.equal(failed.message, "updater.sock missing");
    s.db.run("UPDATE nodes SET revoked_at = ? WHERE id = 'gw-a'", new Date().toISOString());
    assert.equal((await app.inject({ method: "POST", url, headers, payload: { kind: "update-apply" } })).statusCode, 404);
  } finally { await app.close(); }
});
