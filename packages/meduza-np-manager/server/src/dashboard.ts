// Node dashboard sign-in through the manager identity (AGENT.md §12.3: "dashboard 的 HTTP 身份校验由 manager 身份 …
// 提供"). Flow:
//   1. dashboard → planner → POST /nodes/self/dashboard/login {nonceHash, returnUrl}  (node-authenticated)
//      → pending login id + authorize URL on this manager
//   2. the browser opens the authorize page; a signed-in manager user sees node + return URL and approves
//      → one-time code, browser is sent to returnUrl?code=…&state=<id>
//   3. dashboard → planner → POST /nodes/self/dashboard/exchange {id, code, nonce}  (same node only)
//      → {user, role, expiresAt}; the code is single use, bound to the node and to the dashboard's nonce.

import { createHash, randomBytes, timingSafeEqual } from "node:crypto";

import type { Audit } from "./audit.js";
import type { Role, SessionInfo } from "./auth.js";
import type { Db } from "./db.js";

const LOGIN_TTL_MS = 5 * 60_000;
const SESSION_TTL_MS = 8 * 3600_000;
const MAX_PENDING_PER_NODE = 16;

export class DashboardError extends Error {
  constructor(readonly status: number, readonly code: string, message: string) {
    super(message);
  }
}

const sha = (s: string) => createHash("sha256").update(s).digest("hex");
const now = () => Date.now();

function validReturnUrl(raw: string): URL {
  let u: URL;
  try {
    u = new URL(raw);
  } catch {
    throw new DashboardError(400, "invalid-return-url", "returnUrl must be an absolute URL");
  }
  const loopback = ["127.0.0.1", "localhost", "[::1]"].includes(u.hostname);
  if (u.protocol !== "https:" && !(u.protocol === "http:" && loopback)) {
    throw new DashboardError(400, "invalid-return-url", "returnUrl must use https (http only for loopback)");
  }
  if (u.pathname !== "/auth/callback" || u.search || u.hash || u.username || u.password) {
    throw new DashboardError(400, "invalid-return-url", "returnUrl must be <dashboard origin>/auth/callback");
  }
  return u;
}

export interface PendingLoginView {
  id: string;
  nodeId: string;
  nodeName: string;
  returnUrl: string;
  expiresAt: string;
  state: "pending" | "approved" | "used" | "expired";
}

export class DashboardService {
  constructor(private readonly db: Db, private readonly audit: Audit, private readonly publicOrigin: string) {}

  begin(nodeId: string, body: { nonceHash?: unknown; returnUrl?: unknown }) {
    const nonceHash = String(body?.nonceHash ?? "");
    if (!/^[0-9a-f]{64}$/.test(nonceHash)) throw new DashboardError(400, "invalid-request", "nonceHash must be a sha256 hex digest");
    const returnUrl = validReturnUrl(String(body?.returnUrl ?? "")).toString();
    this.db.run("DELETE FROM dashboard_logins WHERE expires_at < ?", now());
    const pending = this.db.get<{ n: number }>("SELECT COUNT(*) AS n FROM dashboard_logins WHERE node_id = ? AND used = 0", nodeId)?.n ?? 0;
    if (pending >= MAX_PENDING_PER_NODE) throw new DashboardError(429, "rate-limited", "too many pending dashboard logins for this node");
    const id = randomBytes(18).toString("base64url");
    this.db.run(
      "INSERT INTO dashboard_logins (id, node_id, nonce_hash, return_url, expires_at, used) VALUES (?, ?, ?, ?, ?, 0)",
      id, nodeId, nonceHash, returnUrl, now() + LOGIN_TTL_MS,
    );
    return { id, authorizeUrl: `${this.publicOrigin}/#/dashboard-login/${id}`, expiresInSeconds: LOGIN_TTL_MS / 1000 };
  }

  view(id: string): PendingLoginView {
    const r = this.db.get<{ id: string; node_id: string; return_url: string; expires_at: number; used: number; code_hash: string | null; name: string | null }>(
      "SELECT l.id, l.node_id, l.return_url, l.expires_at, l.used, l.code_hash, n.display_name AS name FROM dashboard_logins l LEFT JOIN nodes n ON n.id = l.node_id WHERE l.id = ?",
      id,
    );
    if (!r) throw new DashboardError(404, "not-found", "unknown or expired dashboard login");
    const state = r.used ? "used" : r.expires_at < now() ? "expired" : r.code_hash ? "approved" : "pending";
    return { id: r.id, nodeId: r.node_id, nodeName: r.name ?? r.node_id, returnUrl: r.return_url, expiresAt: new Date(r.expires_at).toISOString(), state };
  }

  approve(id: string, session: SessionInfo) {
    const v = this.view(id);
    if (v.state !== "pending") throw new DashboardError(409, "not-pending", `login is ${v.state}`);
    const code = randomBytes(24).toString("base64url");
    this.db.run(
      "UPDATE dashboard_logins SET code_hash = ?, account_id = ?, username = ?, role = ? WHERE id = ? AND code_hash IS NULL",
      sha(code), session.accountId, session.username, session.role, id,
    );
    this.audit.record(session.username, "dashboard.login.approve", v.nodeId, { loginId: id, returnUrl: v.returnUrl });
    const u = new URL(v.returnUrl);
    u.searchParams.set("code", code);
    u.searchParams.set("state", id);
    return { redirect: u.toString() };
  }

  deny(id: string, session: SessionInfo) {
    const node = this.db.get<{ node_id: string }>("SELECT node_id FROM dashboard_logins WHERE id = ?", id)?.node_id ?? null;
    this.db.run("UPDATE dashboard_logins SET used = 1 WHERE id = ?", id);
    this.audit.record(session.username, "dashboard.login.deny", node, { loginId: id });
    return { ok: true };
  }

  exchange(nodeId: string, body: { id?: unknown; code?: unknown; nonce?: unknown }) {
    const id = String(body?.id ?? "");
    const code = String(body?.code ?? "");
    const nonce = String(body?.nonce ?? "");
    const r = this.db.get<{ node_id: string; nonce_hash: string; code_hash: string | null; expires_at: number; used: number; username: string; role: Role; account_id: string }>(
      "SELECT node_id, nonce_hash, code_hash, expires_at, used, username, role, account_id FROM dashboard_logins WHERE id = ?",
      id,
    );
    const eq = (a: string, b: string) => a.length === b.length && timingSafeEqual(Buffer.from(a), Buffer.from(b));
    const valid = r && !r.used && r.expires_at >= now() && r.node_id === nodeId && r.code_hash && eq(r.code_hash, sha(code)) && eq(r.nonce_hash, sha(nonce));
    if (!valid) {
      if (r) {
        this.db.run("UPDATE dashboard_logins SET used = 1 WHERE id = ?", id); // one attempt per login
        // a failed redemption of a known login is worth an operator's attention (wrong node, replay, guessed code)
        const reason = r.used ? "used" : r.expires_at < now() ? "expired" : r.node_id !== nodeId ? "other-node" : !r.code_hash ? "not-approved" : "invalid-code";
        this.audit.record(`node:${nodeId}`, "dashboard.login.refused", nodeId, { loginId: id, reason, approvedBy: r.username ?? null });
      }
      throw new DashboardError(403, "invalid-code", "dashboard login code invalid, expired or already used");
    }
    // the account may have been disabled since approval
    const acct = this.db.get<{ disabled: number; role: Role }>("SELECT disabled, role FROM accounts WHERE id = ?", r.account_id);
    this.db.run("UPDATE dashboard_logins SET used = 1 WHERE id = ?", id);
    if (!acct || acct.disabled) {
      this.audit.record(`node:${nodeId}`, "dashboard.login.refused", nodeId, { loginId: id, reason: "account-disabled", approvedBy: r.username });
      throw new DashboardError(403, "account-disabled", "account disabled");
    }
    this.audit.record(r.username, "dashboard.login", nodeId, { loginId: id, role: acct.role });
    return { user: r.username, role: acct.role, expiresAt: new Date(now() + SESSION_TTL_MS).toISOString() };
  }
}
