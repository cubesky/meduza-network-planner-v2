// OpenVPN Access authentication through the existing OpenLDAP (AGENT.md §6.5, §9.3).
// LDAP is used only here: never for manager logins, never reachable from nodes.

import { Client, InvalidCredentialsError, type ClientOptions } from "ldapts";
import type { ConnectionOptions } from "node:tls";
import type { Db } from "./db.js";
import { nowIso } from "./db.js";
import type { SecretStore } from "./secrets.js";
import type { Audit } from "./audit.js";
import type { ConfigService } from "./publish.js";

export interface LdapSettings {
  /** `ldaps://host:636` or `ldap://host:389` with `startTls: true`. Plain LDAP binds are refused. */
  url: string;
  startTls: boolean;
  baseDn: string;
  bindDn: string;
  /** Secret id holding the service bind password (kind `ldap-bind-password`). */
  bindPasswordRef: string;
  /** `{username}` is replaced by the RFC 4515 escaped login name. */
  userFilter: string;
  /** Either groups found with `groupFilter` under `groupBaseDn` (`{dn}` / `{username}` escaped) or `memberOf` values. */
  groupBaseDn?: string;
  groupFilter?: string;
  groupNameAttribute: string;
  useMemberOf: boolean;
  caPem?: string;
  /** Production default true. Only an admin may disable it, and the UI marks it as insecure. */
  verifyCertificate: boolean;
  connectTimeoutMs: number;
  timeoutMs: number;
}

export interface AccessSession {
  nodeId: string;
  instanceId: string;
  username: string;
  commonName: string;
  realAddress: string;
  virtualAddress: string;
  rxBytes: number;
  txBytes: number;
  connectedSince: string;
  /** OpenVPN management client id; absent from old OpenVPN status files. */
  clientId?: number;
  reportedAt: string;
  /** An operator disconnected this user after the report this row comes from. */
  disconnecting?: boolean;
}

export const DEFAULT_LDAP: LdapSettings = {
  url: "",
  startTls: false,
  baseDn: "",
  bindDn: "",
  bindPasswordRef: "ldap-bind-password",
  userFilter: "(&(objectClass=inetOrgPerson)(uid={username}))",
  groupBaseDn: "",
  groupFilter: "(&(objectClass=groupOfNames)(member={dn}))",
  groupNameAttribute: "cn",
  useMemberOf: false,
  verifyCertificate: true,
  connectTimeoutMs: 3000,
  timeoutMs: 5000,
};

const MISSPELT_DEFAULT_USER_FILTER = "(&(objectClass=inetOrPerson)(uid={username}))";

export interface AccessPolicy {
  id: string;
  name: string;
  enabled: boolean;
  /** LDAP group names (or DNs) that grant access. Empty never matches. */
  groups: string[];
  /** Node ids; empty = all nodes. */
  nodes: string[];
  /** Access instance ids; empty = all instances. */
  instances: string[];
  sessionSeconds: number;
  priority: number;
}

export interface AccessAuthRequest {
  requestId: string;
  instanceId: string;
  username: string;
  /** Empty for `reauth`. */
  password: string;
  /** Renegotiation of a session OpenVPN verified with its HMAC auth token: no password (D38). */
  reauth?: boolean;
  session?: Record<string, unknown>;
}

export interface AccessAuthResponse {
  requestId: string;
  decision: "allow" | "deny";
  reasonCode: string;
  validForSeconds: number;
  groups?: string[];
  policyId?: string;
}

/** RFC 4515 filter value escaping. */
export function escapeFilterValue(value: string): string {
  let out = "";
  for (const byte of Buffer.from(value, "utf8")) {
    const c = String.fromCharCode(byte);
    if (c === "\\" || c === "*" || c === "(" || c === ")" || byte === 0 || byte >= 0x80) out += `\\${byte.toString(16).padStart(2, "0")}`;
    else out += c;
  }
  return out;
}

export function fillFilter(template: string, vars: Record<string, string>): string {
  return template.replace(/\{(username|dn)\}/g, (_, k: string) => escapeFilterValue(vars[k] ?? ""));
}

export function validateLdapSettings(s: LdapSettings): string[] {
  const problems: string[] = [];
  let url: URL | null = null;
  try {
    url = new URL(s.url);
  } catch {
    problems.push("url must be ldaps://host[:port] or ldap://host[:port]");
  }
  if (url && url.protocol !== "ldaps:" && url.protocol !== "ldap:") problems.push("url scheme must be ldaps or ldap");
  if (url?.protocol === "ldap:" && !s.startTls) problems.push("plain ldap:// requires StartTLS; passwords are never sent in clear text");
  if (url?.protocol === "ldaps:" && s.startTls) problems.push("StartTLS is not used with ldaps://");
  if (!s.baseDn) problems.push("baseDn is required");
  if (!s.bindDn) problems.push("bindDn is required");
  if (!s.userFilter.includes("{username}")) problems.push("userFilter must contain {username}");
  if (!s.useMemberOf && (!s.groupBaseDn || !s.groupFilter)) problems.push("groupBaseDn and groupFilter are required unless memberOf is used");
  if (s.timeoutMs < 500 || s.timeoutMs > 10_000) problems.push("timeoutMs must be 500..10000");
  return problems;
}

class Semaphore {
  private active = 0;
  constructor(private readonly max: number) {}
  tryAcquire(): (() => void) | null {
    if (this.active >= this.max) return null;
    this.active++;
    let done = false;
    return () => {
      if (!done) {
        done = true;
        this.active--;
      }
    };
  }
}

const TOTAL_DEADLINE_MS = 8000; // below OpenVPN's hook wait and the §9.3 10 s budget

export class AccessService {
  private readonly slots = new Semaphore(8);
  private readonly failures = new Map<string, { count: number; until: number }>();

  constructor(
    private readonly db: Db,
    private readonly secrets: SecretStore,
    private readonly config: ConfigService,
    private readonly audit: Audit,
  ) {}

  settings(): LdapSettings | null {
    const raw = this.db.setting("ldap");
    if (!raw) return null;
    const s = { ...DEFAULT_LDAP, ...(JSON.parse(raw) as Partial<LdapSettings>) };
    // Earlier versions shipped a misspelt default object class that matches no entry; settings saved with it unchanged
    // get the corrected default.
    if (s.userFilter === MISSPELT_DEFAULT_USER_FILTER) s.userFilter = DEFAULT_LDAP.userFilter;
    return s;
  }

  saveSettings(s: LdapSettings, actor: string): string[] {
    const problems = validateLdapSettings(s);
    if (problems.length) return problems;
    this.db.setSetting("ldap", JSON.stringify(s));
    this.audit.record(actor, "access.ldap.update", null, { ...s, caPem: s.caPem ? "<pem>" : undefined });
    return [];
  }

  policies(): AccessPolicy[] {
    return this.db.all<Record<string, unknown>>("SELECT * FROM access_policies ORDER BY priority, id").map((r) => ({
      id: String(r.id),
      name: String(r.name),
      enabled: !!r.enabled,
      groups: JSON.parse(String(r.groups)) as string[],
      nodes: JSON.parse(String(r.nodes)) as string[],
      instances: JSON.parse(String(r.instances)) as string[],
      sessionSeconds: Number(r.session_seconds),
      priority: Number(r.priority),
    }));
  }

  savePolicy(p: AccessPolicy, actor: string): void {
    if (!/^[a-z0-9][a-z0-9_-]{0,62}$/.test(p.id)) throw new Error("invalid policy id");
    if (!p.groups.length) throw new Error("a policy needs at least one group");
    this.db.run(
      `INSERT INTO access_policies (id, name, enabled, groups, nodes, instances, session_seconds, priority) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT(id) DO UPDATE SET name = excluded.name, enabled = excluded.enabled, groups = excluded.groups, nodes = excluded.nodes,
         instances = excluded.instances, session_seconds = excluded.session_seconds, priority = excluded.priority`,
      p.id, p.name, p.enabled ? 1 : 0, JSON.stringify(p.groups), JSON.stringify(p.nodes), JSON.stringify(p.instances),
      Math.max(60, Math.min(7 * 86400, p.sessionSeconds)), p.priority,
    );
    this.audit.record(actor, "access.policy.save", p.id, p);
  }

  deletePolicy(id: string, actor: string): void {
    this.db.run("DELETE FROM access_policies WHERE id = ?", id);
    this.audit.record(actor, "access.policy.delete", id);
  }

  private tlsOptions(s: LdapSettings): ConnectionOptions {
    const host = new URL(s.url).hostname;
    return { servername: host, rejectUnauthorized: s.verifyCertificate, ...(s.caPem ? { ca: s.caPem } : {}), minVersion: "TLSv1.2" };
  }

  private async connect(s: LdapSettings): Promise<Client> {
    const opts: ClientOptions = { url: s.url, timeout: s.timeoutMs, connectTimeout: s.connectTimeoutMs, strictDN: true };
    if (s.url.startsWith("ldaps:")) opts.tlsOptions = this.tlsOptions(s);
    const client = new Client(opts);
    if (s.url.startsWith("ldap:")) {
      // StartTLS must succeed; there is no fallback to clear text.
      await client.startTLS(this.tlsOptions(s));
    }
    return client;
  }

  private async withService<T>(s: LdapSettings, fn: (c: Client) => Promise<T>): Promise<T> {
    const client = await this.connect(s);
    try {
      await client.bind(s.bindDn, this.secrets.reveal(s.bindPasswordRef));
      return await fn(client);
    } finally {
      await client.unbind().catch(() => undefined);
    }
  }

  private async findUser(c: Client, s: LdapSettings, username: string): Promise<{ dn: string; memberOf: string[] } | { error: string }> {
    const { searchEntries } = await c.search(s.baseDn, {
      scope: "sub",
      filter: fillFilter(s.userFilter, { username }),
      attributes: ["dn", "memberOf"],
      sizeLimit: 2,
    });
    if (searchEntries.length === 0) return { error: "bad-credentials" };
    if (searchEntries.length > 1) return { error: "ambiguous-user" };
    const entry = searchEntries[0]!;
    const memberOf = entry.memberOf === undefined ? [] : (Array.isArray(entry.memberOf) ? entry.memberOf : [entry.memberOf]).map(String);
    return { dn: entry.dn, memberOf };
  }

  private async groupsOf(c: Client, s: LdapSettings, user: { dn: string; memberOf: string[] }, username: string): Promise<string[]> {
    if (s.useMemberOf) {
      return user.memberOf.flatMap((dn) => [dn, /^cn=([^,]+)/i.exec(dn)?.[1] ?? dn]);
    }
    const { searchEntries } = await c.search(s.groupBaseDn!, {
      scope: "sub",
      filter: fillFilter(s.groupFilter!, { dn: user.dn, username }),
      attributes: [s.groupNameAttribute],
      sizeLimit: 500,
    });
    return searchEntries.flatMap((e) => {
      const name = e[s.groupNameAttribute];
      return [e.dn, ...(name === undefined ? [] : Array.isArray(name) ? name.map(String) : [String(name)])];
    });
  }

  /** Is `instanceId` an enabled Access instance in the node's desired configuration? */
  private instanceAllowed(nodeId: string, instanceId: string): boolean {
    const desired = this.config.desired(nodeId);
    if (!desired) return false;
    const snap = this.config.snapshot(nodeId, desired.revision);
    const openvpn = (snap?.config.services as Record<string, unknown> | undefined)?.openvpn as { instances?: Array<Record<string, unknown>> } | undefined;
    return !!openvpn?.instances?.some((i) => i.id === instanceId && i.kind === "access" && i.enabled !== false);
  }

  async authenticate(nodeId: string, req: AccessAuthRequest): Promise<AccessAuthResponse> {
    const started = Date.now();
    const deny = (reasonCode: string): AccessAuthResponse => ({ requestId: req.requestId, decision: "deny", reasonCode, validForSeconds: 0 });
    const finish = (r: AccessAuthResponse) => {
      this.db.run("INSERT INTO access_log (at, node_id, instance_id, username, decision, reason_code, duration_ms) VALUES (?, ?, ?, ?, ?, ?, ?)",
        nowIso(), nodeId, String(req.instanceId ?? "").slice(0, 64), String(req.username ?? "").slice(0, 128), r.decision, r.reasonCode, Date.now() - started);
      this.db.run("DELETE FROM access_log WHERE id <= (SELECT MAX(id) FROM access_log) - 100000");
      return r;
    };

    if (typeof req.username !== "string" || !req.username || req.username.length > 256) return finish(deny("invalid-request"));
    if (req.reauth === true) {
      // The node never sends the token (or any password) for a renegotiation.
      if (req.password) return finish(deny("invalid-request"));
    } else if (typeof req.password !== "string" || !req.password || req.password.length > 1024) {
      return finish(deny("invalid-request"));
    }
    if (!this.instanceAllowed(nodeId, req.instanceId)) return finish(deny("instance-not-allowed"));
    const s = this.settings();
    if (!s) return finish(deny("ldap-unavailable"));
    if (req.reauth === true) {
      const release = this.slots.tryAcquire();
      if (!release) return finish(deny("rate-limited"));
      try {
        return finish(
          await Promise.race([
            this.reauthorize(nodeId, req, s),
            new Promise<AccessAuthResponse>((resolve) => setTimeout(() => resolve(deny("timeout")), TOTAL_DEADLINE_MS).unref()),
          ]),
        );
      } finally {
        release();
      }
    }

    // Simple per-user throttle after repeated failures (does not affect other users or the manager).
    const key = `${req.username.toLowerCase()}`;
    const f = this.failures.get(key);
    if (f && f.until > Date.now()) return finish(deny("rate-limited"));

    const release = this.slots.tryAcquire();
    if (!release) return finish(deny("rate-limited"));
    try {
      const result = await Promise.race([
        this.authenticateLdap(nodeId, req, s),
        new Promise<AccessAuthResponse>((resolve) => setTimeout(() => resolve(deny("timeout")), TOTAL_DEADLINE_MS).unref()),
      ]);
      if (result.reasonCode === "bad-credentials") {
        const cur = this.failures.get(key) ?? { count: 0, until: 0 };
        cur.count++;
        if (cur.count >= 5) cur.until = Date.now() + Math.min(15 * 60_000, 2 ** (cur.count - 5) * 30_000);
        this.failures.set(key, cur);
        if (this.failures.size > 50_000) this.failures.clear();
      } else if (result.decision === "allow") {
        this.failures.delete(key);
      }
      return finish(result);
    } finally {
      release();
    }
  }

  /** Service-bind lookup of the user and their groups. Throws when LDAP cannot be used. */
  private lookup(s: LdapSettings, username: string): Promise<{ error: string } | { user: { dn: string; memberOf: string[] }; groups: string[] }> {
    return this.withService(s, async (c) => {
      const u = await this.findUser(c, s, username);
      if ("error" in u) return u;
      return { user: u, groups: await this.groupsOf(c, s, u, username) };
    });
  }

  /** First enabled policy (by priority) granting one of `groups` on this node and instance. */
  private matchPolicy(groups: string[], nodeId: string, instanceId: string): { policy: AccessPolicy; matched: string[] } | undefined {
    const lowered = new Set(groups.map((g) => g.toLowerCase()));
    const policy = this.policies().find(
      (p) =>
        p.enabled &&
        p.groups.some((g) => lowered.has(g.toLowerCase())) &&
        (p.nodes.length === 0 || p.nodes.includes(nodeId)) &&
        (p.instances.length === 0 || p.instances.includes(instanceId)),
    );
    return policy && { policy, matched: policy.groups.filter((g) => lowered.has(g.toLowerCase())) };
  }

  /**
   * Password-less re-check of a session OpenVPN verified with its auth token (D38): the account must still be found
   * with the service bind, its groups must still match an enabled policy for this node and instance, and the session,
   * started by the user's last password login here, must be within that policy's lifetime.
   */
  private async reauthorize(nodeId: string, req: AccessAuthRequest, s: LdapSettings): Promise<AccessAuthResponse> {
    const deny = (reasonCode: string): AccessAuthResponse => ({ requestId: req.requestId, decision: "deny", reasonCode, validForSeconds: 0 });
    // The latest password login, unless an operator ended the session after it (D39).
    const login = this.db.get<{ at: string; reason_code: string }>(
      "SELECT at, reason_code FROM access_log WHERE node_id = ? AND instance_id = ? AND username = ? AND ((decision = 'allow' AND reason_code = 'ok') OR (decision = 'end' AND reason_code = 'disconnected')) ORDER BY id DESC LIMIT 1",
      nodeId, req.instanceId, req.username,
    );
    if (!login) return deny("no-session");
    if (login.reason_code === "disconnected") return deny("disconnected");
    let found: Awaited<ReturnType<AccessService["lookup"]>>;
    try {
      found = await this.lookup(s, req.username);
    } catch {
      return deny("ldap-unavailable");
    }
    if ("error" in found) return deny(found.error === "ambiguous-user" ? "not-authorized" : "user-disabled");
    const m = this.matchPolicy(found.groups, nodeId, req.instanceId);
    if (!m) return deny("not-authorized");
    const remaining = Math.floor(m.policy.sessionSeconds - (Date.now() - Date.parse(login.at)) / 1000);
    if (remaining <= 0) return deny("session-expired");
    return { requestId: req.requestId, decision: "allow", reasonCode: "ok-reauth", validForSeconds: remaining, groups: m.matched, policyId: m.policy.id };
  }

  private async authenticateLdap(nodeId: string, req: AccessAuthRequest, s: LdapSettings): Promise<AccessAuthResponse> {
    const deny = (reasonCode: string): AccessAuthResponse => ({ requestId: req.requestId, decision: "deny", reasonCode, validForSeconds: 0 });
    let user: { dn: string; memberOf: string[] };
    let groups: string[];
    try {
      const found = await this.lookup(s, req.username);
      if ("error" in found) return deny(found.error === "ambiguous-user" ? "not-authorized" : "bad-credentials");
      user = found.user;
      groups = found.groups;
    } catch {
      return deny("ldap-unavailable");
    }
    // Verify the password by binding as the user (never trust the service account alone).
    const client = await this.connect(s).catch(() => null);
    if (!client) return deny("ldap-unavailable");
    try {
      await client.bind(user.dn, req.password);
    } catch (e) {
      return deny(e instanceof InvalidCredentialsError ? "bad-credentials" : "ldap-unavailable");
    } finally {
      await client.unbind().catch(() => undefined);
    }
    const m = this.matchPolicy(groups, nodeId, req.instanceId);
    if (!m) return deny("not-authorized");
    return { requestId: req.requestId, decision: "allow", reasonCode: "ok", validForSeconds: m.policy.sessionSeconds, groups: m.matched, policyId: m.policy.id };
  }

  /** UI diagnostics: connect + service bind, optionally look up a user and their groups (no user bind). */
  async test(username?: string): Promise<{ ok: boolean; message: string; userDn?: string; groups?: string[] }> {
    const s = this.settings();
    if (!s) return { ok: false, message: "LDAP is not configured" };
    try {
      return await this.withService(s, async (c) => {
        if (!username) return { ok: true, message: "connected and bound with the service account" };
        const u = await this.findUser(c, s, username);
        if ("error" in u) return { ok: false, message: u.error === "ambiguous-user" ? "more than one entry matches" : "user not found" };
        return { ok: true, message: "user found", userDn: u.dn, groups: await this.groupsOf(c, s, u, username) };
      });
    } catch (e) {
      return { ok: false, message: (e as Error).message };
    }
  }

  /**
   * Online Access clients as the nodes last reported them (`openvpn-client` statuses of enabled Access instances),
   * newest report first per node. A report is at most one probe interval old; `reportedAt` says when.
   */
  sessions(reports: Array<{ nodeId: string; report: Record<string, unknown>; receivedAt: string }>): AccessSession[] {
    const out: AccessSession[] = [];
    for (const { nodeId, report, receivedAt } of reports) {
      for (const st of (report.instances ?? []) as Array<Record<string, unknown>>) {
        if (st.kind !== "openvpn-client" || typeof st.id !== "string") continue;
        const instanceId = st.id.split("/")[1] ?? "";
        if (!this.instanceAllowed(nodeId, instanceId)) continue;
        const str = (k: string) => (typeof st[k] === "string" ? (st[k] as string) : "");
        out.push({
          nodeId,
          instanceId,
          username: str("username") || str("commonName"),
          commonName: str("commonName"),
          realAddress: str("realAddress"),
          virtualAddress: str("virtualAddress"),
          rxBytes: typeof st.rxBytes === "number" ? st.rxBytes : 0,
          txBytes: typeof st.txBytes === "number" ? st.txBytes : 0,
          connectedSince: str("connectedSince"),
          clientId: typeof st.clientId === "number" ? st.clientId : undefined,
          reportedAt: typeof st.observedAt === "string" ? st.observedAt : receivedAt,
        });
        const row = out[out.length - 1]!;
        // Disconnected after this report: the node's next report (status file + probe interval) drops the row.
        const ended = this.db.get<{ at: string }>(
          "SELECT at FROM access_log WHERE node_id = ? AND instance_id = ? AND username = ? AND decision = 'end' ORDER BY id DESC LIMIT 1",
          nodeId, instanceId, row.username,
        );
        if (ended && Date.parse(ended.at) >= Date.parse(row.reportedAt)) row.disconnecting = true;
      }
    }
    return out.sort((a, b) => a.nodeId.localeCompare(b.nodeId) || a.instanceId.localeCompare(b.instanceId) || a.username.localeCompare(b.username));
  }

  /**
   * Ends a user's Access sessions on one node and instance (D39). The log row makes every later token renegotiation
   * or token reconnect of that user there fail (`disconnected`) until they log in with their password again; the
   * caller then asks the node to kill the live clients (`disconnect-client` action). Throws for unknown instances.
   */
  endSession(nodeId: string, instanceId: string, username: string, actor: string): void {
    if (!username || username.length > 256) throw new Error("username required");
    if (!this.instanceAllowed(nodeId, instanceId)) throw new Error(`${nodeId}/${instanceId} is not an enabled Access instance`);
    this.db.run("INSERT INTO access_log (at, node_id, instance_id, username, decision, reason_code, duration_ms) VALUES (?, ?, ?, ?, 'end', 'disconnected', 0)",
      nowIso(), nodeId, instanceId, username);
    this.audit.record(actor, "access.session.disconnect", `${nodeId}/${instanceId}`, { username });
  }

  recentLog(limit = 200) {
    return this.db.all<Record<string, unknown>>("SELECT * FROM access_log ORDER BY id DESC LIMIT ?", limit);
  }
}
