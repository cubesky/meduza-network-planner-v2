import { test } from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { DashboardError } from "../src/dashboard.js";
import type { SessionInfo } from "../src/auth.js";
import { startManager } from "./helpers.js";

const sha = (s: string) => createHash("sha256").update(s).digest("hex");

function session(s: Awaited<ReturnType<typeof startManager>>["s"], username: string, role: "viewer" | "operator" | "admin"): SessionInfo {
  const id = `acct-${username}`;
  s.db.run("INSERT INTO accounts (id, username, display_name, role, webauthn_user_id, created_at) VALUES (?, ?, ?, ?, ?, ?)", id, username, username, role, `w-${username}`, new Date().toISOString());
  return { sessionId: `sess-${username}`, accountId: id, username, displayName: username, role, csrfToken: "x", lastUvAt: new Date().toISOString(), settings: {} };
}

const code = (redirect: string) => new URL(redirect).searchParams.get("code")!;

test("dashboard sign-in: node-bound, nonce-bound, single use, live role", async () => {
  const { app, s } = await startManager();
  s.nodes.createNode({ id: "gw-a" }, "test");
  s.nodes.createNode({ id: "gw-b" }, "test");
  const alice = session(s, "alice", "operator");
  const ret = "https://gw-a.example.test:8443/auth/callback";

  const begin = s.dashboard.begin("gw-a", { nonceHash: sha("nonce-1"), returnUrl: ret });
  assert.match(begin.authorizeUrl, /^https:\/\/manager\.example\.test\/#\/dashboard-login\//);
  assert.equal(s.dashboard.view(begin.id).state, "pending");
  const { redirect } = s.dashboard.approve(begin.id, alice);
  assert.ok(redirect.startsWith(`${ret}?code=`) && redirect.includes(`state=${begin.id}`));

  // another node cannot redeem it, and a failed attempt burns the login
  assert.throws(() => s.dashboard.exchange("gw-b", { id: begin.id, code: code(redirect), nonce: "nonce-1" }), DashboardError);
  assert.throws(() => s.dashboard.exchange("gw-a", { id: begin.id, code: code(redirect), nonce: "nonce-1" }), /invalid-code|invalid, expired/);

  // happy path, then replay refused
  const b2 = s.dashboard.begin("gw-a", { nonceHash: sha("nonce-2"), returnUrl: ret });
  const r2 = s.dashboard.approve(b2.id, alice).redirect;
  assert.throws(() => s.dashboard.exchange("gw-a", { id: b2.id, code: code(r2), nonce: "wrong" }), DashboardError);
  const b3 = s.dashboard.begin("gw-a", { nonceHash: sha("nonce-3"), returnUrl: ret });
  const r3 = s.dashboard.approve(b3.id, alice).redirect;
  // role changed after approval: the current role is returned
  s.db.run("UPDATE accounts SET role = 'viewer' WHERE id = ?", alice.accountId);
  const ok = s.dashboard.exchange("gw-a", { id: b3.id, code: code(r3), nonce: "nonce-3" });
  assert.equal(ok.user, "alice");
  assert.equal(ok.role, "viewer");
  assert.throws(() => s.dashboard.exchange("gw-a", { id: b3.id, code: code(r3), nonce: "nonce-3" }), DashboardError, "single use");
  assert.throws(() => s.dashboard.approve(b3.id, alice), /used/);

  // disabled accounts cannot redeem
  const b4 = s.dashboard.begin("gw-a", { nonceHash: sha("n4"), returnUrl: ret });
  const r4 = s.dashboard.approve(b4.id, alice).redirect;
  s.db.run("UPDATE accounts SET disabled = 1 WHERE id = ?", alice.accountId);
  assert.throws(() => s.dashboard.exchange("gw-a", { id: b4.id, code: code(r4), nonce: "n4" }), /disabled/);

  const audit = s.audit.list(50) as { action: string; target: string | null; detail: { reason?: string } }[];
  assert.ok(audit.some((e) => e.action === "dashboard.login"));
  // every refused redemption is audited with its reason
  const refused = audit.filter((e) => e.action === "dashboard.login.refused").map((e) => `${e.target} ${e.detail.reason}`);
  assert.deepEqual(refused.sort(), ["gw-a account-disabled", "gw-a invalid-code", "gw-a used", "gw-a used", "gw-b other-node"].sort());

  // a deny names the node
  const b5 = s.dashboard.begin("gw-b", { nonceHash: sha("n5"), returnUrl: ret });
  s.dashboard.deny(b5.id, alice);
  assert.equal(s.audit.list(1)[0]?.target, "gw-b");
  await app.close();
});

test("dashboard sign-in: return URL rules", async () => {
  const { app, s } = await startManager();
  s.nodes.createNode({ id: "gw-a" }, "test");
  const n = sha("x");
  for (const bad of ["http://gw-a.example.test/auth/callback", "https://evil.test/other", "https://x.test/auth/callback?next=/", "javascript:alert(1)", "not a url"]) {
    assert.throws(() => s.dashboard.begin("gw-a", { nonceHash: n, returnUrl: bad }), DashboardError, bad);
  }
  assert.ok(s.dashboard.begin("gw-a", { nonceHash: n, returnUrl: "http://127.0.0.1:8443/auth/callback" }).id, "loopback http allowed");
  assert.throws(() => s.dashboard.begin("gw-a", { nonceHash: "short", returnUrl: "https://a.test/auth/callback" }), DashboardError);
  await app.close();
});
