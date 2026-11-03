// HTTP application: routes, authentication hooks, error mapping (AGENT.md §6, §7.3, §14.4.3).

import Fastify, { type FastifyInstance, type FastifyReply, type FastifyRequest } from "fastify";
import cookie from "@fastify/cookie";
import websocket from "@fastify/websocket";
import fastifyStatic from "@fastify/static";
import { existsSync } from "node:fs";
import { join } from "node:path";
import type { ManagerConfig } from "./config.js";
import type { Db } from "./db.js";
import { Audit } from "./audit.js";
import { SecretStore, SECRET_KINDS, type SecretKind } from "./secrets.js";
import { PackageCatalog } from "./packages.js";
import { ConfigService, ValidationFailed } from "./publish.js";
import { AuthError, AuthService, roleAtLeast, type Role, type SessionInfo } from "./auth.js";
import { NodeApiError, NodeService } from "./nodes.js";
import { NodeTrafficLimiter, Overloaded } from "./limits.js";
import { StreamHub } from "./stream.js";
import { AccessService, type AccessPolicy, type LdapSettings } from "./access.js";
import { generateWireGuardKeyPair, generateWireGuardPsk, parseWireGuardKey, wireGuardPublicKey } from "./keys.js";
import { redact } from "./audit.js";
import { revisionId, type ClusterDocument } from "./cluster.js";
import { overview, nodeDetail } from "./views.js";
import { topology } from "./topology.js";
import { NodeKeys } from "./nodekeys.js";
import { mihomoSubscriptionOf } from "./compilers/mihomo.js";
import { applyImport, previewImport, type ImportOptions } from "./importer/apply.js";
import { Pki, type KeyAlgorithm } from "./pki.js";
import { DashboardError, DashboardService } from "./dashboard.js";

declare module "fastify" {
  interface FastifyRequest {
    session: SessionInfo | null;
    nodeId: string | null;
  }
}

export interface Services {
  cfg: ManagerConfig;
  db: Db;
  audit: Audit;
  secrets: SecretStore;
  catalog: PackageCatalog;
  config: ConfigService;
  auth: AuthService;
  nodes: NodeService;
  limiter: NodeTrafficLimiter;
  hub: StreamHub;
  access: AccessService;
  pki: Pki;
  dashboard: DashboardService;
  nodeKeys: NodeKeys;
  ready: () => boolean;
  shuttingDown: () => boolean;
}

export function createServices(cfg: ManagerConfig, db: Db, schemasDir: string): Omit<Services, "ready" | "shuttingDown"> {
  const audit = new Audit(db);
  const secrets = new SecretStore(db, cfg.secretEncryptionKey);
  const catalog = new PackageCatalog(cfg.packagesDir, schemasDir);
  const config = new ConfigService(db, secrets, catalog, audit);
  const auth = new AuthService(db, cfg, audit);
  const nodes = new NodeService(db, cfg.sessionSecret, config, audit);
  const limiter = new NodeTrafficLimiter(cfg.limits);
  const hub = new StreamHub(config);
  const access = new AccessService(db, secrets, config, audit);
  const pki = new Pki(db, secrets, audit);
  void pki.repairCrlLabels().catch(() => undefined);
  const dashboard = new DashboardService(db, audit, cfg.publicOrigin);
  const nodeKeys = new NodeKeys(db, secrets, nodes, config, audit);
  config.onConfigAvailable((node, revision, digest, seq) => hub.notify(node, { seq, type: "config.available", revision, digest }));
  nodes.onJob((node, jobId, seq) => hub.notify(node, { seq, type: "job.available", jobId }));
  nodes.onRevoke((node) => hub.disconnect(node, "node revoked"));
  return { cfg, db, audit, secrets, catalog, config, auth, nodes, limiter, hub, access, pki, dashboard, nodeKeys };
}

const SESSION_COOKIE_SECURE = "__Host-mnp_session";
const SESSION_COOKIE_DEV = "mnp_session";

export async function buildApp(s: Services): Promise<FastifyInstance> {
  const { cfg } = s;
  const app = Fastify({
    logger: { level: process.env.LOG_LEVEL ?? "info", redact: ["req.headers.authorization", "req.headers.cookie", "req.headers['x-csrf-token']"] },
    trustProxy: cfg.trustedProxies.length ? cfg.trustedProxies : false,
    bodyLimit: cfg.limits.maxRequestBodyBytes,
  });
  await app.register(cookie);
  await app.register(websocket, { options: { maxPayload: 64 * 1024 } });
  // Bodyless POST/DELETE requests with a JSON content type are treated as `{}` instead of a 400.
  app.removeContentTypeParser("application/json");
  app.addContentTypeParser("application/json", { parseAs: "string" }, (_req, body, done) => {
    const text = String(body);
    if (text.trim() === "") return done(null, {});
    try {
      done(null, JSON.parse(text));
    } catch {
      const err = new Error("request body is not valid JSON") as Error & { statusCode: number };
      err.statusCode = 400;
      done(err, undefined);
    }
  });

  const secureCookies = cfg.publicOrigin.startsWith("https:");
  const cookieName = secureCookies ? SESSION_COOKIE_SECURE : SESSION_COOKIE_DEV;

  app.decorateRequest("session", null);
  app.decorateRequest("nodeId", null);

  app.addHook("onSend", async (_req, reply, payload) => {
    reply.header("X-Content-Type-Options", "nosniff");
    reply.header("Referrer-Policy", "no-referrer");
    reply.header("X-Frame-Options", "DENY");
    reply.header("Cross-Origin-Opener-Policy", "same-origin");
    reply.header("Content-Security-Policy", "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'");
    if (secureCookies) reply.header("Strict-Transport-Security", "max-age=31536000");
    return payload;
  });

  app.setErrorHandler((err: Error, req, reply) => {
    if (err instanceof AuthError || err instanceof NodeApiError || err instanceof DashboardError) return reply.code(err.status).send({ error: err.code, message: err.message });
    if (err instanceof Overloaded) return reply.code(err.status).header("Retry-After", String(err.retryAfterSeconds)).send({ error: "overloaded", message: err.reason });
    if (err instanceof ValidationFailed) return reply.code(422).send({ error: "invalid-config", message: err.message, problems: err.problems });
    const status = (err as { statusCode?: number }).statusCode;
    if (status && status >= 400 && status < 500) return reply.code(status).send({ error: "bad-request", message: err.message });
    req.log.error({ err }, "request failed");
    return reply.code(500).send({ error: "internal", message: "internal error" });
  });

  // ---- health ------------------------------------------------------------------------------------------

  app.get("/healthz", async () => ({ status: "ok" }));
  app.get("/readyz", async (_req, reply) => {
    if (!s.ready() || s.shuttingDown()) return reply.code(503).send({ status: s.shuttingDown() ? "stopping" : "starting" });
    try {
      s.db.get("SELECT 1 AS ok");
    } catch {
      return reply.code(503).send({ status: "database-unavailable" });
    }
    return { status: "ready", streams: s.hub.size };
  });

  // ---- human session handling --------------------------------------------------------------------------

  const originOk = (req: FastifyRequest) => {
    const origin = req.headers.origin;
    return origin === cfg.publicOrigin;
  };

  app.addHook("preHandler", async (req) => {
    if (!req.url.startsWith("/api/v1/") || req.url.startsWith("/api/v1/nodes/self") || req.url === "/api/v1/nodes/enroll") return;
    req.session = s.auth.resolveSession(req.cookies[cookieName]);
    const mutating = req.method !== "GET" && req.method !== "HEAD" && req.method !== "OPTIONS";
    if (mutating) {
      if (!originOk(req)) throw new AuthError(403, "bad-origin", "request origin not allowed");
      // Pre-session ceremony endpoints are protected by Origin + single-use challenges; everything else by CSRF.
      if (req.session && req.headers["x-csrf-token"] !== req.session.csrfToken) throw new AuthError(403, "bad-csrf", "missing or invalid CSRF token");
    }
  });

  const requireRole = (req: FastifyRequest, role: Role): SessionInfo => {
    if (!req.session) throw new AuthError(401, "unauthorized", "sign in required");
    if (!roleAtLeast(req.session.role, role)) throw new AuthError(403, "forbidden", `requires role ${role}`);
    return req.session;
  };

  const setSessionCookie = (reply: FastifyReply, token: string) =>
    reply.setCookie(cookieName, token, { path: "/", httpOnly: true, secure: secureCookies, sameSite: "strict", maxAge: cfg.sessionTtlSeconds });

  // ---- auth --------------------------------------------------------------------------------------------

  app.get("/api/v1/auth/session", async (req) => ({
    authenticated: !!req.session,
    bootstrap: s.auth.bootstrapState(),
    ...(req.session
      ? {
          user: { id: req.session.accountId, username: req.session.username, displayName: req.session.displayName, role: req.session.role, settings: req.session.settings },
          csrfToken: req.session.csrfToken,
        }
      : {}),
  }));

  // Personal settings of the signed-in account (any role; no re-verification — nothing security-relevant).
  app.put<{ Body: { locale?: string | null } }>("/api/v1/account/settings", async (req) => {
    const session = requireRole(req, "viewer");
    return { settings: s.auth.updateSettings(session, { locale: req.body?.locale }) };
  });

  app.post<{ Body: { bootstrapToken?: string; username?: string; displayName?: string; inviteToken?: string } }>(
    "/api/v1/auth/passkeys/register/options",
    async (req) => {
      const b = req.body ?? {};
      if (b.bootstrapToken || b.inviteToken) {
        return s.auth.registrationOptions({ bootstrapToken: b.bootstrapToken, username: b.username, displayName: b.displayName, inviteToken: b.inviteToken });
      }
      return s.auth.registrationOptions({ session: requireRole(req, "viewer") });
    },
  );

  app.post<{ Body: { challengeId: string; response: never; name?: string } }>("/api/v1/auth/passkeys/register/verify", async (req, reply) => {
    const r = await s.auth.registrationVerify({ ...req.body, ip: req.ip, userAgent: req.headers["user-agent"] });
    if (r.session) setSessionCookie(reply, r.session.token);
    return { ok: true, signedIn: !!r.session };
  });

  app.post("/api/v1/auth/passkeys/authenticate/options", async (req) => s.auth.authenticationOptions());

  app.post<{ Body: { challengeId: string; response: never } }>("/api/v1/auth/passkeys/authenticate/verify", async (req, reply) => {
    const r = await s.auth.authenticationVerify({ ...req.body, ip: req.ip, userAgent: req.headers["user-agent"] });
    if (r.session) setSessionCookie(reply, r.session.token);
    return { ok: true };
  });

  app.post("/api/v1/auth/reauth/options", async (req) => s.auth.authenticationOptions(requireRole(req, "viewer")));
  app.post<{ Body: { challengeId: string; response: never } }>("/api/v1/auth/reauth/verify", async (req) => {
    await s.auth.authenticationVerify({ ...req.body, session: requireRole(req, "viewer") });
    return { ok: true };
  });

  app.post("/api/v1/auth/logout", async (req, reply) => {
    if (req.session) s.auth.logout(req.session);
    reply.clearCookie(cookieName, { path: "/" });
    return { ok: true };
  });

  app.get("/api/v1/auth/passkeys", async (req) => s.auth.listPasskeys(requireRole(req, "viewer").accountId));
  app.patch<{ Params: { id: string }; Body: { name: string } }>("/api/v1/auth/passkeys/:id", async (req) => {
    s.auth.renamePasskey(req.params.id, req.body.name, requireRole(req, "viewer"));
    return { ok: true };
  });
  app.delete<{ Params: { id: string } }>("/api/v1/auth/passkeys/:id", async (req) => {
    s.auth.revokePasskey(req.params.id, requireRole(req, "viewer"));
    return { ok: true };
  });

  // ---- accounts (admin) --------------------------------------------------------------------------------

  app.get("/api/v1/accounts", async (req) => {
    requireRole(req, "admin");
    return s.auth.listAccounts();
  });
  app.post<{ Body: { username: string; displayName?: string; role: Role } }>("/api/v1/accounts", async (req) => {
    const actor = requireRole(req, "admin");
    s.auth.requireRecentUv(actor);
    return s.auth.inviteAccount(req.body, actor);
  });
  app.post<{ Params: { id: string } }>("/api/v1/accounts/:id/invite", async (req) => {
    const actor = requireRole(req, "admin");
    s.auth.requireRecentUv(actor);
    return s.auth.newInvite(req.params.id, actor);
  });
  app.patch<{ Params: { id: string }; Body: { role?: Role; disabled?: boolean; displayName?: string } }>("/api/v1/accounts/:id", async (req) => {
    const actor = requireRole(req, "admin");
    s.auth.requireRecentUv(actor);
    s.auth.updateAccount(req.params.id, req.body, actor);
    return { ok: true };
  });
  app.get<{ Params: { id: string } }>("/api/v1/accounts/:id/passkeys", async (req) => {
    requireRole(req, "admin");
    return s.auth.listPasskeys(req.params.id);
  });
  app.post<{ Params: { id: string } }>("/api/v1/accounts/:id/revoke-sessions", async (req) => {
    const actor = requireRole(req, "admin");
    return { revoked: s.auth.revokeSessions(req.params.id, actor.username) };
  });

  // ---- overview, nodes (human) -------------------------------------------------------------------------

  app.get("/api/v1/overview", async (req) => {
    requireRole(req, "viewer");
    return overview(s);
  });

  app.get("/api/v1/nodes", async (req) => {
    requireRole(req, "viewer");
    return overview(s).nodes;
  });

  app.get("/api/v1/topology", async (req) => {
    requireRole(req, "viewer");
    return topology(s);
  });

  app.post<{ Body: { id: string; displayName?: string; behavior?: "static" | "roaming"; tags?: string[] } }>("/api/v1/nodes", async (req) => {
    const actor = requireRole(req, "operator");
    s.nodes.createNode(req.body, actor.username);
    return { ok: true };
  });

  app.get<{ Params: { id: string } }>("/api/v1/nodes/:id", async (req, reply) => {
    requireRole(req, "viewer");
    const d = nodeDetail(s, req.params.id);
    return d ?? reply.code(404).send({ error: "not-found" });
  });

  app.get<{ Params: { id: string }; Querystring: { revision?: string } }>("/api/v1/nodes/:id/config", async (req, reply) => {
    requireRole(req, "viewer");
    const rev = req.query.revision ?? s.config.desired(req.params.id)?.revision;
    const snap = rev ? s.config.snapshot(req.params.id, rev) : undefined;
    if (!snap) return reply.code(404).send({ error: "not-found" });
    return { revision: rev, digest: snap.digest, config: redact(snap.config) };
  });

  app.get<{ Params: { id: string } }>("/api/v1/nodes/:id/history", async (req) => {
    requireRole(req, "viewer");
    return s.nodes.history(req.params.id);
  });

  app.post<{ Params: { id: string }; Body: { ttlSeconds?: number } }>("/api/v1/nodes/:id/enrollment-tokens", async (req) => {
    const actor = requireRole(req, "admin");
    return s.nodes.createEnrollmentToken(req.params.id, actor.username, Math.min(Math.max(req.body?.ttlSeconds ?? 86400, 300), 7 * 86400));
  });

  app.post<{ Params: { id: string } }>("/api/v1/nodes/:id/revoke", async (req) => {
    const actor = requireRole(req, "admin");
    s.auth.requireRecentUv(actor);
    s.nodes.revoke(req.params.id, actor.username);
    return { ok: true };
  });

  // Node-held keys (D28): generated by the node's package, now or once the package is installed.
  app.post<{ Params: { id: string }; Body: { secretId: string; keyKind: SecretKind; package: string; replace?: boolean } }>("/api/v1/nodes/:id/keys", async (req) => {
    const actor = requireRole(req, "operator");
    if (!s.nodes.get(req.params.id)) throw new AuthError(404, "not-found", "node not found");
    try {
      return s.nodeKeys.request(req.params.id, req.body, actor.username);
    } catch (e) {
      throw new AuthError(400, "invalid-key-request", e instanceof Error ? e.message : String(e));
    }
  });

  // Import of the previous planner's etcd export (D58): preview, then stage as this account's saved changes.
  app.post<{ Body: { export: unknown } & ImportOptions }>("/api/v1/import/etcd/preview", async (req) => {
    requireRole(req, "admin");
    try {
      return previewImport(s, req.body.export, req.body);
    } catch (e) {
      throw new AuthError(400, "invalid-export", e instanceof Error ? e.message : String(e));
    }
  });
  app.post<{ Body: { export: unknown } & ImportOptions }>("/api/v1/import/etcd/apply", async (req) => {
    const actor = requireRole(req, "admin");
    s.auth.requireRecentUv(actor); // creates nodes and stores secrets in bulk
    try {
      return applyImport(s, req.body.export, req.body, actor.username);
    } catch (e) {
      throw new AuthError(400, "invalid-export", e instanceof Error ? e.message : String(e));
    }
  });

  // Forces a subscription download on every node running Mihomo (all of them, or those using `subscription`).
  app.post<{ Body: { subscription?: string } }>("/api/v1/mihomo/refresh", async (req) => {
    const actor = requireRole(req, "operator");
    const doc = s.config.currentDocument().document;
    const jobs: Array<{ node: string; jobId: string }> = [];
    for (const node of Object.keys(doc.nodes).sort()) {
      for (const short of ["mihomo", "mihomo-openwrt"]) {
        const used = mihomoSubscriptionOf(doc, node, short);
        if (!used || (req.body?.subscription && used !== req.body.subscription) || !s.nodes.get(node)) continue;
        const jobId = s.nodes.createJob({ nodeId: node, kind: "run-action", package: `meduza-np-${short}`, params: { action: "refresh-subscription" } }, actor.username);
        jobs.push({ node, jobId });
      }
    }
    return { jobs };
  });

  app.get<{ Querystring: { node?: string } }>("/api/v1/node-keys/pending", async (req) => {
    requireRole(req, "viewer");
    return s.nodeKeys.pending(req.query.node);
  });

  app.get<{ Querystring: { node?: string } }>("/api/v1/jobs", async (req) => {
    requireRole(req, "viewer");
    return s.nodes.jobs(req.query.node);
  });

  app.post<{ Params: { id: string }; Body: { kind: string; package?: string; instance?: string; params?: unknown } }>("/api/v1/nodes/:id/jobs", async (req) => {
    const actor = requireRole(req, "operator");
    const allowed = ["restart-instance", "stop-instance", "start-instance", "collect-diagnostics", "run-action", "generate-keypair", "sync-now", "update-check", "update-apply"];
    if (!allowed.includes(req.body.kind)) throw new AuthError(400, "invalid-job", `job kind must be one of ${allowed.join(", ")}`);
    if (req.body.kind === "run-action") {
      // Only actions declared in a package UI descriptor can be triggered (§5.3).
      const params = (req.body.params ?? {}) as { action?: string };
      const entry = req.body.package ? s.catalog.get(req.body.package) : undefined;
      const ui = entry?.ui as { actions?: Array<{ id: string; role?: Role }>; collections?: Array<{ actions?: Array<{ id: string; role?: Role }> }> } | null | undefined;
      const actions = [...(ui?.actions ?? []), ...(ui?.collections ?? []).flatMap((c) => c.actions ?? [])];
      const action = actions.find((a) => a.id === params.action);
      if (!action) throw new AuthError(400, "unknown-action", "action is not declared by the package");
      if (!roleAtLeast(actor.role, action.role ?? "operator")) throw new AuthError(403, "forbidden", `action requires ${action.role}`);
    }
    return { jobId: s.nodes.createJob({ nodeId: req.params.id, kind: req.body.kind, package: req.body.package, instance: req.body.instance, params: req.body.params }, actor.username) };
  });

  // ---- configuration -----------------------------------------------------------------------------------

  app.get("/api/v1/config", async (req) => {
    requireRole(req, "viewer");
    return s.config.currentDocument();
  });
  app.get("/api/v1/revisions", async (req) => {
    requireRole(req, "viewer");
    return s.config.listRevisions();
  });
  app.get<{ Params: { rev: string } }>("/api/v1/revisions/:rev/results", async (req) => {
    requireRole(req, "viewer");
    return s.config.publishResults(req.params.rev);
  });
  app.post<{ Params: { rev: string }; Body: { message?: string } }>("/api/v1/revisions/:rev/rollback", async (req) => {
    const actor = requireRole(req, "operator");
    return s.config.rollback(req.params.rev, actor.username, req.body?.message);
  });

  app.get("/api/v1/drafts", async (req) => {
    requireRole(req, "viewer");
    return s.config.listDrafts();
  });
  // Staged changes of the signed-in account (D23, like LuCI's unsaved changes): its open draft.
  app.get("/api/v1/drafts/mine", async (req) => {
    const actor = requireRole(req, "viewer");
    const d = s.config.myDraft(actor.username);
    return d ? { id: d.id, baseRevision: d.baseRevision === null ? null : revisionId(d.baseRevision), updatedAt: d.updatedAt, document: d.document } : null;
  });
  app.post<{ Params: { id: string } }>("/api/v1/drafts/:id/rebase", async (req) => {
    const actor = requireRole(req, "operator");
    return s.config.rebaseDraft(req.params.id, actor.username);
  });
  app.post<{ Body: { document?: ClusterDocument } }>("/api/v1/drafts", async (req) => {
    const actor = requireRole(req, "operator");
    return s.config.createDraft(actor.username, req.body?.document);
  });
  app.get<{ Params: { id: string } }>("/api/v1/drafts/:id", async (req, reply) => {
    requireRole(req, "viewer");
    return s.config.getDraft(req.params.id) ?? reply.code(404).send({ error: "not-found" });
  });
  app.put<{ Params: { id: string }; Body: { document: ClusterDocument } }>("/api/v1/drafts/:id", async (req) => {
    const actor = requireRole(req, "operator");
    s.config.updateDraft(req.params.id, req.body.document, actor.username);
    return { ok: true };
  });
  app.delete<{ Params: { id: string } }>("/api/v1/drafts/:id", async (req) => {
    const actor = requireRole(req, "operator");
    s.config.discardDraft(req.params.id, actor.username);
    return { ok: true };
  });
  app.post<{ Params: { id: string } }>("/api/v1/drafts/:id/validate", async (req, reply) => {
    requireRole(req, "viewer");
    const d = s.config.getDraft(req.params.id);
    if (!d) return reply.code(404).send({ error: "not-found" });
    return s.config.preview(d.document);
  });
  app.post<{ Params: { id: string }; Body: { message?: string } }>("/api/v1/drafts/:id/publish", async (req, reply) => {
    const actor = requireRole(req, "operator");
    const d = s.config.getDraft(req.params.id);
    if (!d || d.status !== "open") return reply.code(404).send({ error: "not-found" });
    const current = s.config.currentDocument();
    if (d.baseRevision !== current.number) {
      return reply.code(409).send({ error: "stale-draft", message: `draft is based on ${d.baseRevision ?? "nothing"}, current is ${current.revision}; rebase it first` });
    }
    return s.config.publish(d.document, actor.username, { message: req.body?.message, draftId: d.id });
  });
  // Validate an arbitrary document (used by the form editor for instant cross-field checks).
  app.post<{ Body: { document: ClusterDocument } }>("/api/v1/validate", async (req) => {
    requireRole(req, "viewer");
    return s.config.preview(req.body.document);
  });
  // Direct editing (D21): the console sends the document it edited and the revision it started from; applying
  // publishes it as a new revision unless someone published in between (the console then reloads and retries).
  app.post<{ Body: { document: ClusterDocument; baseRevision: string | null; message?: string } }>("/api/v1/config/apply", async (req, reply) => {
    const actor = requireRole(req, "operator");
    const current = s.config.currentDocument();
    if ((req.body?.baseRevision ?? null) !== current.revision) {
      return reply.code(409).send({ error: "conflict", message: `the configuration changed meanwhile (now ${current.revision ?? "empty"})`, revision: current.revision });
    }
    return s.config.publish(req.body.document, actor.username, { message: req.body.message });
  });

  // ---- packages ----------------------------------------------------------------------------------------

  app.get("/api/v1/packages", async (req) => {
    requireRole(req, "viewer");
    return s.catalog.list().map((e) => ({ manifest: e.manifest, manifestDigest: e.manifestDigest, ui: e.ui, uiDigest: e.uiDigest, configSchema: e.configSchema }));
  });

  app.get<{ Params: { name: string } }>("/api/v1/schemas/:name", async (req, reply) => {
    requireRole(req, "viewer");
    const schema = s.catalog.rawSchemas.get(req.params.name);
    return schema ?? reply.code(404).send({ error: "not-found" });
  });

  // ---- secrets -----------------------------------------------------------------------------------------

  app.get("/api/v1/secrets", async (req) => {
    requireRole(req, "viewer");
    return s.secrets.list();
  });

  app.post<{ Body: { id: string; kind: SecretKind; value: string; nodeScope?: string[]; description?: string } }>("/api/v1/secrets", async (req) => {
    const actor = requireRole(req, "operator");
    const b = req.body;
    if (!SECRET_KINDS.includes(b.kind)) throw new AuthError(400, "invalid-kind", "unknown secret kind");
    let publicValue: string | null = null;
    if (b.kind === "wireguard-private-key") {
      try {
        publicValue = wireGuardPublicKey(b.value);
      } catch (e) {
        throw new AuthError(400, "invalid-key", (e as Error).message);
      }
    }
    if (b.kind === "wireguard-psk") {
      try {
        parseWireGuardKey(b.value);
      } catch (e) {
        throw new AuthError(400, "invalid-key", (e as Error).message);
      }
    }
    const rec = s.secrets.put({ id: b.id, kind: b.kind, value: b.value, publicValue, nodeScope: b.nodeScope, description: b.description });
    s.audit.record(actor.username, "secret.import", b.id, { kind: b.kind, nodeScope: b.nodeScope });
    return rec;
  });

  app.post<{ Body: { id: string; kind: "wireguard-private-key" | "wireguard-psk"; nodeScope?: string[]; description?: string; replace?: boolean } }>(
    "/api/v1/secrets/generate",
    async (req) => {
      const actor = requireRole(req, "operator");
      const b = req.body;
      // Regenerating invalidates existing connections; the UI must say so and the caller must confirm it.
      if (s.secrets.meta(b.id) && !b.replace) throw new AuthError(409, "exists", "secret exists; pass replace=true to regenerate (existing peers must be updated)");
      let rec;
      if (b.kind === "wireguard-private-key") {
        const kp = generateWireGuardKeyPair();
        rec = s.secrets.put({ id: b.id, kind: b.kind, value: kp.privateKey, publicValue: kp.publicKey, nodeScope: b.nodeScope, description: b.description });
      } else if (b.kind === "wireguard-psk") {
        rec = s.secrets.put({ id: b.id, kind: b.kind, value: generateWireGuardPsk(), nodeScope: b.nodeScope, description: b.description });
      } else {
        throw new AuthError(400, "unsupported", "manager-side generation supports wireguard-private-key and wireguard-psk; other kinds use node jobs or PKI");
      }
      s.audit.record(actor.username, b.replace ? "secret.regenerate" : "secret.generate", b.id, { kind: b.kind });
      return rec;
    },
  );

  app.patch<{ Params: { id: string }; Body: { nodeScope: string[] } }>("/api/v1/secrets/:id", async (req) => {
    const actor = requireRole(req, "operator");
    s.secrets.setScope(req.params.id, req.body.nodeScope);
    s.audit.record(actor.username, "secret.scope", req.params.id, { nodeScope: req.body.nodeScope });
    return s.secrets.meta(req.params.id);
  });

  /** Explicit, separately authorized export with recent Passkey verification (§6.3). */
  app.post<{ Params: { id: string } }>("/api/v1/secrets/:id/export", async (req) => {
    const actor = requireRole(req, "admin");
    s.auth.requireRecentUv(actor);
    const value = s.secrets.reveal(req.params.id);
    s.audit.record(actor.username, "secret.export", req.params.id);
    return { id: req.params.id, value };
  });

  app.delete<{ Params: { id: string } }>("/api/v1/secrets/:id", async (req) => {
    const actor = requireRole(req, "admin");
    s.auth.requireRecentUv(actor);
    s.secrets.delete(req.params.id);
    s.audit.record(actor.username, "secret.delete", req.params.id);
    return { ok: true };
  });

  // ---- PKI ---------------------------------------------------------------------------------------------

  app.get("/api/v1/pki/cas", async (req) => {
    requireRole(req, "viewer");
    return s.pki.listCas();
  });
  app.post<{ Body: { id: string; name: string; algorithm?: KeyAlgorithm; validityDays?: number } }>("/api/v1/pki/cas", async (req) => {
    const actor = requireRole(req, "admin");
    s.auth.requireRecentUv(actor);
    try {
      return await s.pki.createCa(req.body, actor.username);
    } catch (e) {
      throw new AuthError(400, "pki-error", (e as Error).message);
    }
  });
  app.get<{ Querystring: { ca?: string } }>("/api/v1/pki/certs", async (req) => {
    requireRole(req, "viewer");
    return s.pki.listCerts(req.query.ca);
  });
  app.post<{ Body: { caId: string; commonName: string; usage: "server" | "client"; node?: string; validityDays?: number; dnsNames?: string[]; algorithm?: KeyAlgorithm } }>(
    "/api/v1/pki/certs",
    async (req) => {
      const actor = requireRole(req, "operator");
      if (req.body.usage !== "server" && req.body.usage !== "client") throw new AuthError(400, "invalid-usage", "usage must be server or client");
      try {
        return await s.pki.issueWithKey(req.body, actor.username);
      } catch (e) {
        throw new AuthError(400, "pki-error", (e as Error).message);
      }
    },
  );
  app.post<{ Body: { caId: string; csrPem: string; usage: "server" | "client"; node?: string; validityDays?: number } }>("/api/v1/pki/csr", async (req) => {
    const actor = requireRole(req, "operator");
    try {
      return await s.pki.signCsr(req.body, actor.username);
    } catch (e) {
      throw new AuthError(400, "pki-error", (e as Error).message);
    }
  });
  app.post<{ Params: { serial: string }; Body: { reason?: string } }>("/api/v1/pki/certs/:serial/revoke", async (req) => {
    const actor = requireRole(req, "admin");
    s.auth.requireRecentUv(actor);
    await s.pki.revoke(req.params.serial, req.body?.reason ?? "unspecified", actor.username);
    return { ok: true, note: "the updated CRL reaches nodes with the next publish" };
  });
  app.post<{ Body: { id: string; nodes: string[]; replace?: boolean } }>("/api/v1/pki/static-keys", async (req) => {
    const actor = requireRole(req, "operator");
    try {
      return s.pki.createStaticKey(req.body.id, req.body.nodes ?? [], actor.username, !!req.body.replace);
    } catch (e) {
      throw new AuthError(409, "exists", (e as Error).message);
    }
  });
  /** Access client profile export: separately authorized, re-verified and audited (§6.3, §9.3). */
  app.post<{ Body: { node: string; instance: string; certRef?: string; keyRef?: string; remoteHost?: string } }>("/api/v1/pki/access-profile", async (req, reply) => {
    const actor = requireRole(req, "admin");
    s.auth.requireRecentUv(actor);
    const desired = s.config.desired(req.body.node);
    const snap = desired ? s.config.snapshot(req.body.node, desired.revision) : undefined;
    const inst = ((snap?.config.services as Record<string, any> | undefined)?.openvpn?.instances ?? []).find((i: Record<string, any>) => i.id === req.body.instance && i.kind === "access");
    if (!inst) return reply.code(404).send({ error: "not-found", message: "no published Access instance with that id on the node" });
    const endpoint = String(req.body.remoteHost ?? inst.publicEndpoint ?? s.nodes.get(req.body.node)?.hostname ?? "");
    const [host, port] = endpoint.includes(":") ? endpoint.split(":") : [endpoint, String(inst.port ?? 1194)];
    if (!host) return reply.code(400).send({ error: "no-endpoint", message: "set publicEndpoint on the instance or pass remoteHost" });
    const profile = s.pki.accessClientProfile({
      name: `${req.body.node}/${req.body.instance}`,
      remoteHost: host,
      port: Number(port),
      proto: String(inst.proto ?? "udp"),
      caRef: inst.tls?.caRef,
      tlsCryptRef: inst.tls?.tlsCryptRef,
      certRef: req.body.certRef,
      keyRef: req.body.keyRef,
      verifyName: inst.tls?.verifyX509Name,
    });
    s.audit.record(actor.username, "pki.access-profile.export", `${req.body.node}/${req.body.instance}`, { withClientCert: !!req.body.certRef });
    return { filename: `${req.body.node}-${req.body.instance}.ovpn`, content: profile };
  });

  // ---- Access / LDAP -----------------------------------------------------------------------------------

  app.get("/api/v1/access/ldap", async (req) => {
    requireRole(req, "admin");
    return s.access.settings();
  });
  app.put<{ Body: LdapSettings }>("/api/v1/access/ldap", async (req, reply) => {
    const actor = requireRole(req, "admin");
    const problems = s.access.saveSettings(req.body, actor.username);
    return problems.length ? reply.code(422).send({ error: "invalid-settings", problems }) : { ok: true };
  });
  app.post<{ Body: { username?: string } }>("/api/v1/access/ldap/test", async (req) => {
    requireRole(req, "admin");
    return s.access.test(req.body?.username);
  });
  app.get("/api/v1/access/policies", async (req) => {
    requireRole(req, "viewer");
    return s.access.policies();
  });
  app.put<{ Params: { id: string }; Body: AccessPolicy }>("/api/v1/access/policies/:id", async (req) => {
    const actor = requireRole(req, "admin");
    s.access.savePolicy({ ...req.body, id: req.params.id }, actor.username);
    return { ok: true };
  });
  app.delete<{ Params: { id: string } }>("/api/v1/access/policies/:id", async (req) => {
    const actor = requireRole(req, "admin");
    s.access.deletePolicy(req.params.id, actor.username);
    return { ok: true };
  });
  app.get("/api/v1/access/log", async (req) => {
    requireRole(req, "operator");
    return s.access.recentLog();
  });
  // Online Access clients from the latest node reports, and disconnecting a user from one node's instance (D39).
  app.get("/api/v1/access/sessions", async (req) => {
    requireRole(req, "operator");
    const reports = s.nodes.list().flatMap((n) => {
      const r = n.revoked_at ? undefined : s.nodes.latestReport(n.id);
      return r ? [{ nodeId: n.id, report: r.report, receivedAt: r.receivedAt }] : [];
    });
    return s.access.sessions(reports);
  });
  app.post<{ Body: { node?: string; instance?: string; username?: string } }>("/api/v1/access/sessions/disconnect", async (req) => {
    const actor = requireRole(req, "operator");
    const { node, instance, username } = req.body ?? {};
    if (typeof node !== "string" || typeof instance !== "string" || typeof username !== "string" || !username) {
      throw new AuthError(400, "invalid-request", "node, instance and username are required");
    }
    if (!s.nodes.get(node)) throw new AuthError(404, "not-found", "node not found");
    try {
      // Refuse the user's token first, so a client reconnecting right after the kill cannot resume the session.
      s.access.endSession(node, instance, username, actor.username);
    } catch (e) {
      throw new AuthError(404, "not-found", e instanceof Error ? e.message : String(e));
    }
    const jobId = s.nodes.createJob(
      { nodeId: node, kind: "run-action", package: "meduza-np-openvpn", instance: `openvpn/${instance}`, params: { action: "disconnect-client", username }, ttlSeconds: 600 },
      actor.username,
    );
    return { jobId };
  });

  app.get<{ Params: { id: string } }>("/api/v1/dashboard-logins/:id", async (req) => {
    requireRole(req, "viewer");
    return s.dashboard.view(req.params.id);
  });
  app.post<{ Params: { id: string } }>("/api/v1/dashboard-logins/:id/approve", async (req) => s.dashboard.approve(req.params.id, requireRole(req, "viewer")));
  app.post<{ Params: { id: string } }>("/api/v1/dashboard-logins/:id/deny", async (req) => s.dashboard.deny(req.params.id, requireRole(req, "viewer")));

  app.get<{ Querystring: { before?: string } }>("/api/v1/audit", async (req) => {
    requireRole(req, "operator");
    return s.audit.list(200, req.query.before ? Number(req.query.before) : undefined);
  });

  // ---- node machine API --------------------------------------------------------------------------------

  const nodeRoute = async (req: FastifyRequest) => {
    if (s.shuttingDown()) throw new Overloaded(503, 30, "manager restarting");
    const nodeId = s.nodes.authenticate(req.headers.authorization);
    if (!nodeId) throw new NodeApiError(401, "unauthorized", "node token missing or expired");
    req.nodeId = nodeId;
  };

  const limited = <T>(nodeId: string | null, fn: () => T | Promise<T>): Promise<T> => {
    const release = s.limiter.admit(nodeId);
    return Promise.resolve().then(fn).finally(release);
  };

  app.post<{ Body: Record<string, unknown> }>("/api/v1/nodes/enroll", async (req) => limited(null, () => s.nodes.enroll(req.body as never)));
  app.post<{ Body: { nodeId: string } }>("/api/v1/nodes/self/session/challenge", async (req) => limited(null, () => s.nodes.challenge(String(req.body?.nodeId ?? ""))));
  app.post<{ Body: { nodeId: string; nonce: string; signature: string } }>("/api/v1/nodes/self/session", async (req) => limited(null, () => s.nodes.openSession(req.body ?? {})));

  app.post<{ Body: { newPublicKey: string; signature: string } }>("/api/v1/nodes/self/keys/rotate", { preHandler: nodeRoute }, async (req) =>
    limited(req.nodeId, () => {
      s.nodes.rotateKey(req.nodeId!, req.body);
      return { ok: true };
    }));

  app.get<{ Querystring: { after?: string; revision?: string } }>("/api/v1/nodes/self/config", { preHandler: nodeRoute }, async (req, reply) =>
    limited(req.nodeId, () => {
      const nodeId = req.nodeId!;
      s.nodes.touch(nodeId);
      const desired = s.config.desired(nodeId);
      const revision = req.query.revision ?? desired?.revision;
      if (!revision) return reply.code(204).send();
      if (!req.query.revision) {
        const etag = `"${desired!.digest}"`;
        if (req.headers["if-none-match"] === etag || req.query.after === revision) return reply.code(304).header("ETag", etag).send();
        reply.header("ETag", etag);
      }
      const snap = s.config.snapshot(nodeId, revision);
      if (!snap) return reply.code(404).send({ error: "unknown-revision" });
      return {
        meta: { revision, schemaVersion: 1, configDigest: snap.digest, generatedAt: snap.createdAt },
        config: snap.config,
      };
    }));

  app.post<{ Body: Record<string, unknown> }>("/api/v1/nodes/self/reports", { preHandler: nodeRoute }, async (req, reply) =>
    limited(req.nodeId, () => {
      const problems = s.catalog.validate("node-report", req.body);
      if (problems.length) return reply.code(422).send({ error: "invalid-report", problems: problems.slice(0, 20) });
      s.nodes.touch(req.nodeId!);
      const r = s.nodes.acceptReport(req.nodeId!, req.body as never);
      // a package the node just installed may be waiting to generate a requested key (D28)
      if (r.accepted) s.nodeKeys.kick(req.nodeId!);
      const desired = s.config.desired(req.nodeId!);
      return { ...r, desiredRevision: desired?.revision ?? null, configDigest: desired?.digest ?? null };
    }));

  app.post<{ Body: Parameters<NodeService["ack"]>[1] }>("/api/v1/nodes/self/acks", { preHandler: nodeRoute }, async (req) =>
    limited(req.nodeId, () => s.nodes.ack(req.nodeId!, req.body)));

  app.get("/api/v1/nodes/self/jobs", { preHandler: nodeRoute }, async (req) => limited(req.nodeId, () => s.nodes.pendingJobs(req.nodeId!)));

  app.post<{ Params: { id: string }; Body: { ok: boolean; result?: unknown; errorCode?: string; message?: string } }>(
    "/api/v1/nodes/self/jobs/:id/result",
    { preHandler: nodeRoute },
    async (req) =>
      limited(req.nodeId, () => {
        const r = s.nodes.jobResult(req.nodeId!, req.params.id, req.body);
        // Node-side key generation: record the public half (the private key never leaves the node, §6.3) and
        // distribute it to the other nodes (D28); a failed attempt is retried once the package is ready.
        if (!r.duplicate && r.job.kind === "generate-keypair") {
          const result = req.body.result as { publicKey?: string } | undefined;
          s.nodeKeys.finished(req.nodeId!, r.job, req.body.ok, result?.publicKey);
        }
        return { ok: true, duplicate: r.duplicate };
      }),
  );

  app.post<{ Body: Parameters<AccessService["authenticate"]>[1] }>("/api/v1/nodes/self/access-auth", { preHandler: nodeRoute }, async (req) =>
    limited(req.nodeId, () => s.access.authenticate(req.nodeId!, req.body)));

  app.post<{ Body: { nonceHash?: string; returnUrl?: string } }>("/api/v1/nodes/self/dashboard/login", { preHandler: nodeRoute }, async (req) =>
    limited(req.nodeId, () => s.dashboard.begin(req.nodeId!, req.body)));
  app.post<{ Body: { id?: string; code?: string; nonce?: string } }>("/api/v1/nodes/self/dashboard/exchange", { preHandler: nodeRoute }, async (req) =>
    limited(req.nodeId, () => s.dashboard.exchange(req.nodeId!, req.body)));

  app.get(
    "/api/v1/nodes/self/stream",
    {
      websocket: true,
      preValidation: async (req) => {
        await nodeRoute(req);
        (req as FastifyRequest & { streamRelease?: () => void }).streamRelease = s.limiter.admitStream();
      },
    },
    (socket, req) => {
      const release = (req as FastifyRequest & { streamRelease?: () => void }).streamRelease ?? (() => undefined);
      s.nodes.touch(req.nodeId!);
      s.hub.attach(socket, req.nodeId!, release);
    },
  );

  // ---- web UI ------------------------------------------------------------------------------------------

  if (existsSync(join(cfg.webDir, "index.html"))) {
    await app.register(fastifyStatic, { root: cfg.webDir, prefix: "/", index: ["index.html"], cacheControl: false });
    app.addHook("onSend", async (req, reply, payload) => {
      // Hashed assets are immutable; index.html must always revalidate so upgrades load matching assets.
      if (req.url.startsWith("/assets/")) reply.header("Cache-Control", "public, max-age=31536000, immutable");
      else if (!req.url.startsWith("/api/")) reply.header("Cache-Control", "no-cache");
      return payload;
    });
    app.setNotFoundHandler((req, reply) => {
      if (req.method === "GET" && !req.url.startsWith("/api/")) return reply.sendFile("index.html");
      return reply.code(404).send({ error: "not-found" });
    });
  }

  return app;
}
