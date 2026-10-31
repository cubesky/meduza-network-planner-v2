// Direct editing (D21): apply publishes the edited document unless someone published in between; features for
// packages a node does not have yet only warn (D20).

import { test } from "node:test";
import assert from "node:assert/strict";
import { adminHeaders, startManager } from "./helpers.js";

test("config apply: publishes with the base revision, refuses a stale one, warnings never block", async () => {
  const { app } = await startManager();
  const headers = await adminHeaders(app);
  assert.equal((await app.inject({ method: "POST", url: "/api/v1/nodes", headers, payload: { id: "gw-a" } })).statusCode, 200);
  const doc = { schemaVersion: 1, nodes: { "gw-a": { networks: { lan: ["10.42.10.0/24"] } } } };

  const first = await app.inject({ method: "POST", url: "/api/v1/config/apply", headers, payload: { document: doc, baseRevision: null, message: "first" } });
  assert.equal(first.statusCode, 200, first.body);
  assert.equal(first.json().revision, "rev-000001");

  // Someone else's console still based on nothing: refused, nothing published.
  const stale = await app.inject({ method: "POST", url: "/api/v1/config/apply", headers, payload: { document: doc, baseRevision: null } });
  assert.equal(stale.statusCode, 409);
  assert.equal(stale.json().revision, "rev-000001");

  // FRR is not installed on gw-a (it never reported): a warning, and the revision is published anyway.
  const withFrr = { schemaVersion: 1, nodes: { "gw-a": { networks: { lan: ["10.42.10.0/24"] }, routerId: "10.42.0.1", services: { frr: { localAsn: 65010 } } } } };
  const preview = (await app.inject({ method: "POST", url: "/api/v1/validate", headers, payload: { document: withFrr } })).json();
  assert.ok(preview.problems.length && preview.problems.every((p: { severity?: string }) => p.severity === "warning"), JSON.stringify(preview.problems));
  const second = await app.inject({ method: "POST", url: "/api/v1/config/apply", headers, payload: { document: withFrr, baseRevision: "rev-000001" } });
  assert.equal(second.statusCode, 200, second.body);
  assert.equal(second.json().revision, "rev-000002");

  // Errors still block.
  const bad = { schemaVersion: 1, nodes: { "gw-a": { services: { nope: {} } } } };
  const refused = await app.inject({ method: "POST", url: "/api/v1/config/apply", headers, payload: { document: bad, baseRevision: "rev-000002" } });
  assert.equal(refused.statusCode, 422);
  await app.close();
});

test("staged changes: per-account draft, stale apply refused, rebase keeps mine and takes theirs", async () => {
  const { app, s } = await startManager();
  const headers = await adminHeaders(app);
  for (const id of ["gw-a", "gw-b"]) await app.inject({ method: "POST", url: "/api/v1/nodes", headers, payload: { id } });
  const doc = { schemaVersion: 1, nodes: { "gw-a": { networks: { lan: ["10.42.10.0/24"] } }, "gw-b": { networks: { lan: ["10.42.20.0/24"] } } } };
  s.config.publish(doc, "someone");

  // nothing staged yet
  assert.equal((await app.inject({ method: "GET", url: "/api/v1/drafts/mine", headers })).json(), null);
  // stage a change on gw-a (LuCI "Save")
  const mine = { ...doc, nodes: { ...doc.nodes, "gw-a": { displayName: "Gateway A", networks: { lan: ["10.42.10.0/24"] } } } };
  const d = (await app.inject({ method: "POST", url: "/api/v1/drafts", headers, payload: { document: mine } })).json();
  const staged = (await app.inject({ method: "GET", url: "/api/v1/drafts/mine", headers })).json();
  assert.equal(staged.id, d.id);
  assert.equal(staged.baseRevision, "rev-000001");

  // someone else applies a change to gw-b meanwhile: applying the stale stage is refused…
  s.config.publish({ ...doc, nodes: { ...doc.nodes, "gw-b": { networks: { lan: ["10.42.21.0/24"] } } } }, "other");
  assert.equal((await app.inject({ method: "POST", url: `/api/v1/drafts/${d.id}/publish`, headers, payload: {} })).statusCode, 409);
  // …rebase keeps my gw-a and takes their gw-b, without conflicts
  const rb = (await app.inject({ method: "POST", url: `/api/v1/drafts/${d.id}/rebase`, headers, payload: {} })).json();
  assert.deepEqual(rb, { conflicts: [], baseRevision: "rev-000002" });
  const after = (await app.inject({ method: "GET", url: "/api/v1/drafts/mine", headers })).json().document;
  assert.equal(after.nodes["gw-a"].displayName, "Gateway A");
  assert.deepEqual(after.nodes["gw-b"].networks.lan, ["10.42.21.0/24"]);
  const pub = await app.inject({ method: "POST", url: `/api/v1/drafts/${d.id}/publish`, headers, payload: { message: "rename" } });
  assert.equal(pub.statusCode, 200, pub.body);
  assert.equal((await app.inject({ method: "GET", url: "/api/v1/drafts/mine", headers })).json(), null, "applied stage is gone");

  // both changed the same node: reported as a conflict, mine kept for review
  const d2 = (await app.inject({ method: "POST", url: "/api/v1/drafts", headers, payload: { document: { ...after, nodes: { ...after.nodes, "gw-b": { displayName: "B mine" } } } } })).json();
  s.config.publish({ ...after, nodes: { ...after.nodes, "gw-b": { displayName: "B theirs" } } }, "other");
  const rb2 = (await app.inject({ method: "POST", url: `/api/v1/drafts/${d2.id}/rebase`, headers, payload: {} })).json();
  assert.deepEqual(rb2.conflicts, ["/nodes/gw-b"]);
  await app.close();
});
