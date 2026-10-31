import { test } from "node:test";
import assert from "node:assert/strict";
import { escapeFilterValue, fillFilter, validateLdapSettings, DEFAULT_LDAP } from "../src/access.js";
import { adminHeaders, startManager } from "./helpers.js";

test("LDAP filter values are RFC 4515 escaped", () => {
  assert.equal(escapeFilterValue("a*b(c)\\d\u0000"), "a\\2ab\\28c\\29\\5cd\\00");
  assert.equal(fillFilter("(&(objectClass=inetOrgPerson)(uid={username}))", { username: "*)(uid=*" }), "(&(objectClass=inetOrgPerson)(uid=\\2a\\29\\28uid=\\2a))");
  assert.equal(escapeFilterValue("Zoë"), "Zo\\c3\\ab");
});

test("LDAP settings refuse clear-text password transport", () => {
  const base = { ...DEFAULT_LDAP, baseDn: "dc=example,dc=test", bindDn: "cn=svc,dc=example,dc=test", groupBaseDn: "ou=groups,dc=example,dc=test" };
  assert.deepEqual(validateLdapSettings({ ...base, url: "ldaps://ldap.example.test" }), []);
  assert.ok(validateLdapSettings({ ...base, url: "ldap://ldap.example.test" }).some((p) => p.includes("StartTLS")));
  assert.deepEqual(validateLdapSettings({ ...base, url: "ldap://ldap.example.test", startTls: true }), []);
  assert.ok(validateLdapSettings({ ...base, url: "ldaps://x", userFilter: "(uid=*)" }).some((p) => p.includes("{username}")));
});

test("Access authentication denies by default", async () => {
  const { app, s } = await startManager();
  s.nodes.createNode({ id: "gw-a" }, "test");
  const req = { requestId: "r1", instanceId: "access-main", username: "alice", password: "pw" };
  assert.equal((await s.access.authenticate("gw-a", req)).reasonCode, "instance-not-allowed", "no published Access instance");
  assert.equal((await s.access.authenticate("gw-a", { ...req, password: "" })).reasonCode, "invalid-request");
  const log = s.access.recentLog();
  assert.equal(log.length, 2);
  assert.ok(!JSON.stringify(log).includes("pw"), "passwords never logged");
  await app.close();
});

test("settings saved with the earlier misspelt default user filter are repaired", async () => {
  const { app, s } = await startManager();
  const stored = { ...DEFAULT_LDAP, url: "ldaps://ldap.example.test", userFilter: "(&(objectClass=inetOrPerson)(uid={username}))" };
  s.db.setSetting("ldap", JSON.stringify(stored));
  assert.equal(s.access.settings()!.userFilter, "(&(objectClass=inetOrgPerson)(uid={username}))");
  s.db.setSetting("ldap", JSON.stringify({ ...stored, userFilter: "(uid={username})" }));
  assert.equal(s.access.settings()!.userFilter, "(uid={username})", "custom filters are kept");
  await app.close();
});

test("operators see online Access clients and can end a session (D39)", async () => {
  const { app, s } = await startManager();
  try {
    s.nodes.createNode({ id: "gw-a" }, "test");
    s.config.publish({ schemaVersion: 1, nodes: { "gw-a": { services: { openvpn: { instances: [{ id: "access-main", kind: "access", access: { pool: "10.88.0.0/24" } }] } } } } }, "test");
    // An unreachable directory: everything checked before the LDAP lookup is decided anyway.
    s.db.setSetting("ldap", JSON.stringify({ ...DEFAULT_LDAP, url: "ldaps://127.0.0.1:1", baseDn: "dc=x", bindDn: "cn=svc,dc=x", groupBaseDn: "dc=x", connectTimeoutMs: 200 }));
    const reauth = { requestId: "r", instanceId: "access-main", username: "alice", password: "", reauth: true };
    assert.equal((await s.access.authenticate("gw-a", reauth)).reasonCode, "no-session");
    const login = () => s.db.run("INSERT INTO access_log (at, node_id, instance_id, username, decision, reason_code, duration_ms) VALUES (?, 'gw-a', 'access-main', 'alice', 'allow', 'ok', 1)", new Date().toISOString());
    login();
    assert.equal((await s.access.authenticate("gw-a", reauth)).reasonCode, "ldap-unavailable", "session found, then the directory is asked");
    s.access.endSession("gw-a", "access-main", "alice", "test");
    assert.equal((await s.access.authenticate("gw-a", reauth)).reasonCode, "disconnected");
    assert.equal((await s.access.authenticate("gw-a", { ...reauth, username: "bob" })).reasonCode, "no-session", "other users unaffected");
    login();
    assert.equal((await s.access.authenticate("gw-a", reauth)).reasonCode, "ldap-unavailable", "a new password login starts a new session");
    assert.throws(() => s.access.endSession("gw-a", "other", "alice", "test"), /not an enabled Access instance/);

    const client = (inst: string, user: string) => ({ id: `openvpn/${inst}/${user}`, kind: "openvpn-client", state: "up", username: user, commonName: user, realAddress: "203.0.113.7:5000", virtualAddress: "10.88.0.2", rxBytes: 5, txBytes: 6, connectedSince: "2026-10-24T10:00:00Z", clientId: 3 });
    const sessions = s.access.sessions([{ nodeId: "gw-a", receivedAt: "2026-10-24T10:05:00Z", report: { instances: [client("access-main", "alice"), client("gone", "bob"), { id: "openvpn/access-main", kind: "openvpn-access", state: "up" }] } }]);
    assert.deepEqual(sessions.map((x) => [x.nodeId, x.instanceId, x.username, x.clientId, x.reportedAt]), [["gw-a", "access-main", "alice", 3, "2026-10-24T10:05:00Z"]], "only clients of enabled Access instances");

    const headers = await adminHeaders(app);
    const r = await app.inject({ method: "POST", url: "/api/v1/access/sessions/disconnect", headers, payload: { node: "gw-a", instance: "access-main", username: "alice" } });
    assert.equal(r.statusCode, 200, r.body);
    const job = s.nodes.jobs("gw-a").find((j) => j.id === r.json().jobId)!;
    assert.deepEqual([job.kind, job.package, job.instance, JSON.parse(String(job.params))], ["run-action", "meduza-np-openvpn", "openvpn/access-main", { action: "disconnect-client", username: "alice" }]);
    assert.equal((await s.access.authenticate("gw-a", reauth)).reasonCode, "disconnected");
    const after = s.access.sessions([{ nodeId: "gw-a", receivedAt: "2026-10-24T10:05:00Z", report: { instances: [client("access-main", "alice")] } }]);
    assert.equal(after[0]!.disconnecting, true, "the report predates the disconnect");
    const fresh = { ...client("access-main", "alice"), observedAt: new Date(Date.now() + 1000).toISOString() };
    assert.equal(s.access.sessions([{ nodeId: "gw-a", receivedAt: "x", report: { instances: [fresh] } }])[0]!.disconnecting, undefined, "reconnected since");
    const bad = await app.inject({ method: "POST", url: "/api/v1/access/sessions/disconnect", headers, payload: { node: "gw-a", instance: "nope", username: "alice" } });
    assert.equal(bad.statusCode, 404);
    assert.equal((await app.inject({ method: "GET", url: "/api/v1/access/sessions", headers })).statusCode, 200);
  } finally {
    await app.close();
  }
});
