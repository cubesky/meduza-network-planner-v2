// OpenVPN Access against a live OpenLDAP (AGENT.md §6.5, §9.3). Run by `tools/access-ldap-e2e.sh`, which starts
// OpenLDAP with its own CA (ldaps + StartTLS, memberOf overlay, read-only service account) and sets the
// MEDUZA_NP_LDAP_TEST_* variables. Skipped without them, unless MEDUZA_NP_LDAP_TEST_REQUIRED is set (CI).
//
// 1. The manager's AccessService through the node route: allow/deny matrix, filter injection, ambiguous users,
//    StartTLS, memberOf, certificate verification, unreachable server, per-user throttling, password-less
//    renegotiation re-checks (D38: session binding, group removal, lifetime).
// 2. The whole hook chain on Linux: `openvpn-auth` (via-file) → planner Access socket → manager → LDAP, plus the
//    client-connect hook (session-timeout) and token renegotiations.

import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomBytes } from "node:crypto";
import { Client, Attribute, Change } from "ldapts";
import { startManager, NodeIdentity, REPO_ROOT } from "./helpers.js";
import { sessionSigningInput } from "../src/nodes.js";
import { DEFAULT_LDAP, type LdapSettings } from "../src/access.js";

const env = (k: string) => process.env[`MEDUZA_NP_LDAP_TEST_${k}`] || undefined;
const LDAPS = env("LDAPS"); // ldaps://localhost:1636
const STARTTLS = env("STARTTLS"); // ldap://localhost:1389
const CA_FILE = env("CA");
const BASE = env("BASE") ?? "dc=example,dc=test";
const ADMIN_DN = env("ADMIN_DN") ?? `cn=admin,${BASE}`;
const ADMIN_PASSWORD = env("ADMIN_PASSWORD");
const SVC_DN = env("SVC_DN") ?? `cn=readonly,${BASE}`;
const SVC_PASSWORD = env("SVC_PASSWORD");
const configured = !!(LDAPS && STARTTLS && CA_FILE && ADMIN_PASSWORD && SVC_PASSWORD);

function skipUnlessConfigured(t: { skip: (m: string) => void }): boolean {
  if (configured) return false;
  assert.ok(!env("REQUIRED"), "MEDUZA_NP_LDAP_TEST_REQUIRED is set but the LDAP test environment is incomplete");
  t.skip("no live LDAP (run tools/access-ldap-e2e.sh)");
  return true;
}

const PASSWORDS = { alice: "Alice-pw-1!", bob: "Bob-pw-2!", carol: "Carol-pw-3!", dup: "Dup-pw-4!" };

async function ldapAdmin<T>(fn: (c: Client) => Promise<T>): Promise<T> {
  const c = new Client({ url: LDAPS!, tlsOptions: { ca: readFileSync(CA_FILE!, "utf8"), servername: new URL(LDAPS!).hostname } });
  await c.bind(ADMIN_DN, ADMIN_PASSWORD!);
  try {
    return await fn(c);
  } finally {
    await c.unbind();
  }
}

/** Seeds a fresh subtree per run (no cleanup needed between runs): people, two group styles, a duplicate uid. */
async function seed(): Promise<{ runDn: string; groupsDn: string }> {
  const ca = readFileSync(CA_FILE!, "utf8");
  const c = new Client({ url: LDAPS!, tlsOptions: { ca, servername: new URL(LDAPS!).hostname } });
  await c.bind(ADMIN_DN, ADMIN_PASSWORD!);
  const runDn = `ou=run-${randomBytes(4).toString("hex")},${BASE}`;
  const people = `ou=people,${runDn}`;
  const other = `ou=other,${runDn}`;
  const groupsDn = `ou=groups,${runDn}`;
  try {
    await c.add(runDn, { objectClass: ["organizationalUnit"], ou: runDn.slice(3, runDn.indexOf(",")) });
    for (const [dn, ou] of [[people, "people"], [other, "other"], [groupsDn, "groups"]] as const) await c.add(dn, { objectClass: ["organizationalUnit"], ou });
    const user = (ou: string, uid: string, pw: string) =>
      c.add(`uid=${uid},${ou}`, { objectClass: ["inetOrgPerson"], uid, cn: uid, sn: uid, userPassword: pw });
    await user(people, "alice", PASSWORDS.alice);
    await user(people, "bob", PASSWORDS.bob);
    await user(people, "carol", PASSWORDS.carol);
    await user(people, "dup", PASSWORDS.dup);
    await user(other, "dup", PASSWORDS.dup);
    // groupOfNames for the group-search mode, groupOfUniqueNames for the memberOf overlay.
    await c.add(`cn=vpn-users,${groupsDn}`, { objectClass: ["groupOfNames"], cn: "vpn-users", member: [`uid=alice,${people}`, `uid=dup,${people}`] });
    await c.add(`cn=vpn-unique,${groupsDn}`, { objectClass: ["groupOfUniqueNames"], cn: "vpn-unique", uniqueMember: [`uid=carol,${people}`] });
    // The memberOf overlay updates on modify as well; touch the group so carol's memberOf is certainly present.
    await c.modify(`cn=vpn-unique,${groupsDn}`, new Change({ operation: "add", modification: new Attribute({ type: "description", values: ["vpn"] }) }));
  } finally {
    await c.unbind();
  }
  return { runDn, groupsDn };
}

async function enrolledNode(ctx: Awaited<ReturnType<typeof startManager>>, nodeId: string) {
  const { app, s } = ctx;
  s.nodes.createNode({ id: nodeId, behavior: "static" }, "test");
  const { token } = s.nodes.createEnrollmentToken(nodeId, "test");
  const identity = new NodeIdentity();
  const enroll = await app.inject({ method: "POST", url: "/api/v1/nodes/enroll", payload: { enrollmentToken: token, publicKey: identity.publicKeyB64(), hostname: nodeId, arch: "x86_64", plannerApi: 1 } });
  assert.equal(enroll.statusCode, 200, enroll.body);
  const ch = (await app.inject({ method: "POST", url: "/api/v1/nodes/self/session/challenge", payload: { nodeId } })).json();
  const sess = await app.inject({ method: "POST", url: "/api/v1/nodes/self/session", payload: { nodeId, nonce: ch.nonce, signature: identity.sign(sessionSigningInput(nodeId, ch.nonce)) } });
  assert.equal(sess.statusCode, 200, sess.body);
  return { authorization: `Bearer ${sess.json().token}` };
}

const accessInstance = { id: "access-main", kind: "access", access: { pool: "10.88.0.0/24" } };
const accessDoc = (node: string) => ({ schemaVersion: 1, nodes: { [node]: { services: { openvpn: { instances: [accessInstance] } } } } });

function settings(runDn: string, groupsDn: string, over: Partial<LdapSettings> = {}): LdapSettings {
  return {
    ...DEFAULT_LDAP,
    url: LDAPS!,
    baseDn: runDn,
    bindDn: SVC_DN,
    groupBaseDn: groupsDn,
    caPem: readFileSync(CA_FILE!, "utf8"),
    timeoutMs: 3000,
    ...over,
  };
}

test("Access authenticates against a live OpenLDAP", { timeout: 120_000 }, async (t) => {
  if (skipUnlessConfigured(t)) return;
  const { runDn, groupsDn } = await seed();
  const ctx = await startManager();
  const { app, s } = ctx;
  try {
    const auth = await enrolledNode(ctx, "gw-ldap");
    s.secrets.put({ id: "ldap-bind-password", kind: "ldap-bind-password", value: SVC_PASSWORD! });
    const pub = s.config.publish(accessDoc("gw-ldap"), "test");
    assert.ok(pub.revision);
    s.access.savePolicy({ id: "vpn", name: "VPN users", enabled: true, groups: ["vpn-users"], nodes: [], instances: ["access-main"], sessionSeconds: 3600, priority: 10 }, "test");
    s.access.savePolicy({ id: "unique", name: "memberOf", enabled: true, groups: ["vpn-unique"], nodes: ["gw-ldap"], instances: [], sessionSeconds: 1800, priority: 20 }, "test");

    let n = 0;
    const ask = async (username: string, password: string, instanceId = "access-main") => {
      const r = await app.inject({ method: "POST", url: "/api/v1/nodes/self/access-auth", headers: auth, payload: { requestId: `r${++n}`, instanceId, username, password } });
      assert.equal(r.statusCode, 200, r.body);
      return r.json() as { decision: string; reasonCode: string; validForSeconds: number; groups?: string[]; policyId?: string };
    };
    const use = (over: Partial<LdapSettings> = {}) => assert.deepEqual(s.access.saveSettings(settings(runDn, groupsDn, over), "test"), []);

    // ldaps + group search.
    use();
    const ok = await ask("alice", PASSWORDS.alice);
    assert.deepEqual([ok.decision, ok.reasonCode, ok.policyId, ok.validForSeconds, ok.groups], ["allow", "ok", "vpn", 3600, ["vpn-users"]]);
    assert.equal((await ask("alice", "wrong")).reasonCode, "bad-credentials");
    assert.equal((await ask("bob", PASSWORDS.bob)).reasonCode, "not-authorized", "valid password, no granting group");
    assert.equal((await ask("nobody", "x")).reasonCode, "bad-credentials", "unknown users look like a wrong password");
    assert.equal((await ask("*", "x")).reasonCode, "bad-credentials", "wildcard is escaped, not a match-all");
    assert.equal((await ask("alice)(uid=*", "x")).reasonCode, "bad-credentials", "filter injection is escaped");
    assert.equal((await ask("dup", PASSWORDS.dup)).reasonCode, "not-authorized", "two entries with one uid are refused");
    assert.equal((await ask("alice", PASSWORDS.alice, "other-instance")).reasonCode, "instance-not-allowed");
    const diag = await s.access.test("alice");
    assert.ok(diag.ok && diag.userDn === `uid=alice,ou=people,${runDn}` && diag.groups?.includes("vpn-users"), JSON.stringify(diag));

    // Renegotiation of a token-verified session (D38): no password, bound to the user's last password login here.
    const reauth = async (username: string, password = "") => {
      const r = await app.inject({ method: "POST", url: "/api/v1/nodes/self/access-auth", headers: auth, payload: { requestId: `r${++n}`, instanceId: "access-main", username, password, reauth: true } });
      assert.equal(r.statusCode, 200, r.body);
      return r.json() as { decision: string; reasonCode: string; validForSeconds: number; policyId?: string };
    };
    const re = await reauth("alice");
    assert.deepEqual([re.decision, re.reasonCode, re.policyId], ["allow", "ok-reauth", "vpn"], JSON.stringify(re));
    assert.ok(re.validForSeconds > 3500 && re.validForSeconds <= 3600, `remaining lifetime ${re.validForSeconds}`);
    assert.equal((await reauth("carol")).reasonCode, "no-session", "never logged in with a password on this instance");
    assert.equal((await reauth("alice", PASSWORDS.alice)).reasonCode, "invalid-request", "a renegotiation carries no password");
    // Removed from the granting group: the next renegotiation ends the session; back in the group, it continues.
    const member = new Change({ operation: "delete", modification: new Attribute({ type: "member", values: [`uid=alice,ou=people,${runDn}`] }) });
    await ldapAdmin((c) => c.modify(`cn=vpn-users,${groupsDn}`, member));
    assert.equal((await reauth("alice")).reasonCode, "not-authorized", "removed from vpn-users");
    await ldapAdmin((c) => c.modify(`cn=vpn-users,${groupsDn}`, new Change({ operation: "add", modification: member.modification })));
    assert.equal((await reauth("alice")).reasonCode, "ok-reauth");
    // Ended by an operator (D39): the token no longer resumes it; a password login starts a new one.
    s.access.endSession("gw-ldap", "access-main", "alice", "test");
    assert.equal((await reauth("alice")).reasonCode, "disconnected");
    assert.equal((await ask("alice", PASSWORDS.alice)).reasonCode, "ok");
    assert.equal((await reauth("alice")).reasonCode, "ok-reauth");
    // The session started at the password login; once older than the policy's lifetime it is over.
    s.db.run("UPDATE access_log SET at = ? WHERE id = (SELECT MAX(id) FROM access_log WHERE username = 'alice' AND reason_code = 'ok')", new Date(Date.now() - 7200_000).toISOString());
    assert.equal((await reauth("alice")).reasonCode, "session-expired");

    // StartTLS on ldap://.
    use({ url: STARTTLS!, startTls: true });
    assert.equal((await ask("alice", PASSWORDS.alice)).decision, "allow", "StartTLS");
    assert.equal((await ask("alice", "wrong")).reasonCode, "bad-credentials");

    // memberOf overlay instead of a group search.
    use({ useMemberOf: true, groupBaseDn: "", groupFilter: "" });
    const carol = await ask("carol", PASSWORDS.carol);
    assert.deepEqual([carol.decision, carol.policyId, carol.validForSeconds], ["allow", "unique", 1800], JSON.stringify(carol));
    assert.equal((await ask("alice", PASSWORDS.alice)).reasonCode, "not-authorized", "alice is only in the groupOfNames group");

    // Certificate verification: without the private CA the server certificate is not trusted.
    use({ caPem: undefined });
    assert.equal((await ask("alice", PASSWORDS.alice)).reasonCode, "ldap-unavailable");
    assert.equal((await s.access.test()).ok, false);
    // A wrong service password is an LDAP problem, not the user's.
    s.secrets.put({ id: "ldap-bind-password", kind: "ldap-bind-password", value: "not-the-password" });
    use();
    assert.equal((await ask("alice", PASSWORDS.alice)).reasonCode, "ldap-unavailable");
    s.secrets.put({ id: "ldap-bind-password", kind: "ldap-bind-password", value: SVC_PASSWORD! });

    // Unreachable server: denied quickly, well inside the hook budget.
    use({ url: "ldaps://localhost:1", connectTimeoutMs: 1000 });
    const started = Date.now();
    assert.equal((await ask("alice", PASSWORDS.alice)).reasonCode, "ldap-unavailable");
    assert.ok(Date.now() - started < 8000, `took ${Date.now() - started} ms`);

    // Five wrong passwords throttle that user only.
    use();
    for (let i = 0; i < 5; i++) assert.equal((await ask("bob", "wrong")).reasonCode, "bad-credentials");
    assert.equal((await ask("bob", PASSWORDS.bob)).reasonCode, "rate-limited");
    assert.equal((await ask("alice", PASSWORDS.alice)).decision, "allow", "other users are not affected");

    const log = s.access.recentLog(500);
    assert.ok(log.length >= 20 && log.every((r) => r.node_id === "gw-ldap"));
    const dump = JSON.stringify(log);
    for (const pw of Object.values(PASSWORDS)) assert.ok(!dump.includes(pw), "passwords never logged");
  } finally {
    await app.close();
  }
});

async function until<T>(what: string, fn: () => T | undefined | null | false, timeoutMs = 60_000): Promise<T> {
  const start = Date.now();
  for (;;) {
    const v = fn();
    if (v) return v as T;
    if (Date.now() - start > timeoutMs) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 250));
  }
}

test("openvpn-auth hook → planner Access socket → manager → LDAP", { timeout: 180_000 }, async (t) => {
  if (skipUnlessConfigured(t)) return;
  const planner = join(REPO_ROOT, "target", "debug", "meduza-np-planner");
  const hook = join(REPO_ROOT, "target", "debug", "openvpn-auth");
  if (process.platform === "win32" || !existsSync(planner) || !existsSync(hook)) {
    assert.ok(!env("REQUIRED"), `planner/openvpn-auth binaries missing: ${planner}, ${hook}`);
    t.skip("needs Linux and `cargo build -p meduza-np-planner -p meduza-np-openvpn`");
    return;
  }
  const { runDn, groupsDn } = await seed();
  const { app, s } = await startManager();
  await app.listen({ port: 0, host: "127.0.0.1" });
  const port = (app.server.address() as { port: number }).port;
  s.nodes.createNode({ id: "gw-hook", behavior: "static" }, "test");
  const { token } = s.nodes.createEnrollmentToken("gw-hook", "test");
  s.secrets.put({ id: "ldap-bind-password", kind: "ldap-bind-password", value: SVC_PASSWORD! });
  assert.deepEqual(s.access.saveSettings(settings(runDn, groupsDn), "test"), []);
  s.access.savePolicy({ id: "vpn", name: "VPN users", enabled: true, groups: ["vpn-users"], nodes: ["gw-hook"], instances: [], sessionSeconds: 3600, priority: 10 }, "test");

  const root = mkdtempSync(join(tmpdir(), "mnp-access-"));
  const pkgs = join(root, "packages");
  mkdirSync(join(pkgs, "meduza-np-planner"), { recursive: true });
  cpSync(join(REPO_ROOT, "packages/meduza-np-planner/manifest.json"), join(pkgs, "meduza-np-planner/manifest.json"));
  const logs: string[] = [];
  const child = spawn(planner, ["run"], {
    env: {
      ...process.env,
      MEDUZA_NP_ETC_DIR: join(root, "etc"),
      MEDUZA_NP_STATE_DIR: join(root, "state"),
      MEDUZA_NP_RUN_DIR: join(root, "run"),
      MEDUZA_NP_PACKAGES_DIR: pkgs,
      MEDUZA_NP_LIBEXEC_DIR: join(root, "libexec"),
      MEDUZA_NP_ENROLL_URL: `http://127.0.0.1:${port}`,
      MEDUZA_NP_ENROLL_TOKEN: token,
      MEDUZA_NP_ENROLL_INSECURE: "1",
      MEDUZA_NP_DRY_RUN_NETWORK: "1",
      MEDUZA_NP_BACKOFF_SPREAD_SECONDS: "0",
      MEDUZA_NP_LOG: "debug",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stderr.on("data", (d) => logs.push(String(d)));
  child.stdout.on("data", (d) => logs.push(String(d)));
  try {
    await until("enrollment", () => s.nodes.get("gw-hook")?.public_key);
    await until("push channel", () => s.hub.connected("gw-hook"));
    const socket = join(root, "run", "access.sock");
    await until("access socket", () => existsSync(socket));

    // Asynchronous on purpose: the manager answering the planner runs in this process.
    const hookRun = (username: string, password: string, instance = "access-main", extraEnv: Record<string, string> = {}, connect = false) =>
      new Promise<{ code: number | null; stderr: string; file: string }>((resolve) => {
        const file = join(root, `cred-${randomBytes(3).toString("hex")}`);
        writeFileSync(file, connect ? "" : `${username}\n${password}\n`, { mode: 0o600 });
        const args = connect ? ["--instance", instance, "--connect", file] : ["--instance", instance, file];
        const h = spawn(hook, args, {
          env: { ...process.env, MEDUZA_NP_ACCESS_SOCKET: socket, untrusted_ip: "198.51.100.7", untrusted_port: "40000", ...extraEnv },
          stdio: ["ignore", "ignore", "pipe"],
        });
        let stderr = "";
        h.stderr.on("data", (d) => (stderr += String(d)));
        const timer = setTimeout(() => h.kill(), 15_000);
        h.on("close", (code) => {
          clearTimeout(timer);
          resolve({ code, stderr, file });
        });
      });
    // client-connect for the same connection (OpenVPN sets trusted_* and the username as common_name).
    const connectRun = (username: string, port = "40000") =>
      hookRun(username, "", "access-main", { common_name: username, trusted_ip: "198.51.100.7", trusted_port: port }, true);

    // Not an applied Access instance on this node yet: the planner answers without asking the manager.
    const early = await hookRun("alice", PASSWORDS.alice);
    assert.equal(early.code, 1);
    assert.match(early.stderr, /instance-not-allowed/);

    // The node's desired configuration names the Access instance (the manager checks this), and the planner's
    // applied state has it. Running OpenVPN itself is out of scope here (no openvpn binary/package in this
    // planner tree), so the applied record is written the way the engine writes it after a successful apply.
    s.config.publish(accessDoc("gw-hook"), "test");
    await new Promise((r) => setTimeout(r, 3000));
    const appliedFile = join(root, "state", "applied.json");
    const applied = existsSync(appliedFile) ? (JSON.parse(readFileSync(appliedFile, "utf8")) as { instances?: Record<string, unknown> }) : {};
    applied.instances = { ...(applied.instances ?? {}), "openvpn/access-main": { package: "meduza-np-planner", planDigest: "test", processes: {} } };
    writeFileSync(appliedFile, JSON.stringify(applied));

    const allow = await hookRun("alice", PASSWORDS.alice);
    assert.equal(allow.code, 0, allow.stderr);
    // The connect hook turns the granted lifetime into the client's session-timeout, once, for that connection only.
    assert.equal((await connectRun("alice", "40001")).code, 1, "another connection of alice was not authorized");
    const conn = await connectRun("alice");
    assert.equal(conn.code, 0, conn.stderr);
    assert.equal(readFileSync(conn.file, "utf8"), "session-timeout 3600\n");
    assert.match((await connectRun("alice")).stderr, /no-session/, "the grant is used up");
    // Renegotiation with a valid token: password-less re-check; the token itself goes nowhere.
    const token = "SESS_ID_AT_0123456789abcdefTOKEN";
    const renego = await hookRun("alice", token, "access-main", { session_state: "Authenticated" });
    assert.equal(renego.code, 0, renego.stderr);
    assert.match((await hookRun("alice", token, "access-main", { session_state: "Expired" })).stderr, /expired/);
    assert.match((await hookRun("alice", token, "access-main", { session_state: "Invalid" })).stderr, /invalid auth token/);
    assert.equal(s.access.recentLog()[0]!.reason_code, "ok-reauth", "expired/invalid tokens never reach the manager");
    assert.ok(!logs.join("").includes(token), "the token is never logged");
    const wrong = await hookRun("alice", "wrong");
    assert.equal(wrong.code, 1);
    assert.match(wrong.stderr, /bad-credentials/);
    const bob = await hookRun("bob", PASSWORDS.bob);
    assert.equal(bob.code, 1);
    assert.match(bob.stderr, /not-authorized/);
    assert.match((await hookRun("alice", PASSWORDS.alice, "other")).stderr, /instance-not-allowed/);

    const log = s.access.recentLog();
    assert.deepEqual(log.slice(0, 4).map((r) => [r.node_id, r.username, r.reason_code]).reverse(), [
      ["gw-hook", "alice", "ok"],
      ["gw-hook", "alice", "ok-reauth"],
      ["gw-hook", "alice", "bad-credentials"],
      ["gw-hook", "bob", "not-authorized"],
    ]);
    const all = logs.join("");
    for (const pw of Object.values(PASSWORDS)) assert.ok(!all.includes(pw), "planner never logs passwords");
  } catch (e) {
    console.error(logs.join(""));
    throw e;
  } finally {
    child.kill();
    await app.close();
  }
});
