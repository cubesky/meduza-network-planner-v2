// Node machine identity, enrollment, reports, ACKs and jobs (AGENT.md §7.1–§7.4, §12.1, D3).

import { createHmac, createPublicKey, verify as cryptoVerify, randomBytes } from "node:crypto";
import type { Db } from "./db.js";
import { isoIn, nowIso } from "./db.js";
import type { Audit } from "./audit.js";
import { randomId, randomToken } from "./audit.js";
import type { ConfigService } from "./publish.js";
import { revisionNumber } from "./cluster.js";

export const MANAGER_API = { min: 1, max: 1 };
const NODE_ID_RE = /^[a-z0-9][a-z0-9_-]{0,62}$/;
const ED25519_SPKI_PREFIX = Buffer.from("302a300506032b6570032100", "hex");
const CHALLENGE_TTL = 60;
const NODE_TOKEN_TTL = 15 * 60;
const HISTORY_PER_NODE = 1000;

/** Job results are kept as JSON up to this size; a larger one is replaced by a marker instead of being cut mid-document. */
export const MAX_JOB_RESULT_CHARS = 256_000;

function boundedResult(result: unknown): string {
  const text = JSON.stringify(result ?? null);
  return text.length <= MAX_JOB_RESULT_CHARS ? text : JSON.stringify({ truncated: true, bytes: text.length });
}

export class NodeApiError extends Error {
  constructor(readonly status: number, readonly code: string, message: string) {
    super(message);
  }
}

export function sessionSigningInput(nodeId: string, nonce: string): Buffer {
  return Buffer.from(`meduza-np-node-session-v1\n${nodeId}\n${nonce}`);
}

function publicKeyFromRaw(b64: string) {
  const raw = Buffer.from(b64, "base64");
  if (raw.length !== 32) throw new NodeApiError(400, "invalid-public-key", "public key must be 32 raw Ed25519 bytes (base64)");
  return createPublicKey({ key: Buffer.concat([ED25519_SPKI_PREFIX, raw]), format: "der", type: "spki" });
}

export interface NodeRow {
  id: string;
  display_name: string;
  behavior: "static" | "roaming";
  tags: string;
  public_key: string | null;
  enrolled_at: string | null;
  hostname: string | null;
  arch: string | null;
  planner_version: string | null;
  last_seen_at: string | null;
  revoked_at: string | null;
  created_at: string;
}

export type JobListener = (nodeId: string, jobId: string, seq: number) => void;
export type RevokeListener = (nodeId: string) => void;

export class NodeService {
  private jobListeners: JobListener[] = [];
  private revokeListeners: RevokeListener[] = [];
  private lastHistoryAt = new Map<string, number>();

  constructor(
    private readonly db: Db,
    private readonly secret: Buffer,
    private readonly config: ConfigService,
    private readonly audit: Audit,
  ) {}

  onJob(fn: JobListener): void {
    this.jobListeners.push(fn);
  }

  onRevoke(fn: RevokeListener): void {
    this.revokeListeners.push(fn);
  }

  private hash(kind: string, token: string): string {
    return createHmac("sha256", this.secret).update(`${kind}:${token}`).digest("base64url");
  }

  // ---- administration ----------------------------------------------------------------------------------

  createNode(input: { id: string; displayName?: string; behavior?: "static" | "roaming"; tags?: string[] }, actor: string): void {
    if (!NODE_ID_RE.test(input.id)) throw new NodeApiError(400, "invalid-node-id", "node id: lowercase letters, digits, '-', '_' (max 63)");
    if (this.get(input.id)) throw new NodeApiError(409, "exists", "node exists");
    this.db.run("INSERT INTO nodes (id, display_name, behavior, tags, created_at) VALUES (?, ?, ?, ?, ?)",
      input.id, input.displayName ?? input.id, input.behavior ?? "static", JSON.stringify(input.tags ?? []), nowIso());
    this.audit.record(actor, "node.create", input.id, input);
  }

  /** One-time, expiring enrollment token (§7.1). Re-issuing for an enrolled node rotates its identity on use. */
  createEnrollmentToken(nodeId: string, actor: string, ttlSeconds = 24 * 3600): { token: string; expiresAt: string } {
    const node = this.get(nodeId);
    if (!node || node.revoked_at) throw new NodeApiError(404, "not-found", "node not found or revoked");
    const token = `mnp-enroll-${randomToken(24)}`;
    const expiresAt = isoIn(ttlSeconds);
    this.db.run("INSERT INTO enrollment_tokens (id, node_id, created_by, expires_at) VALUES (?, ?, ?, ?)", this.hash("enroll", token), nodeId, actor, expiresAt);
    this.audit.record(actor, "node.enrollment-token", nodeId, { expiresAt });
    return { token, expiresAt };
  }

  revoke(nodeId: string, actor: string): void {
    const now = nowIso();
    this.db.tx(() => {
      this.db.run("UPDATE nodes SET revoked_at = ?, public_key = NULL WHERE id = ?", now, nodeId);
      this.db.run("UPDATE node_tokens SET revoked_at = ? WHERE node_id = ? AND revoked_at IS NULL", now, nodeId);
      this.db.run("UPDATE jobs SET status = 'cancelled', finished_at = ? WHERE node_id = ? AND status IN ('pending','delivered')", now, nodeId);
    });
    this.audit.record(actor, "node.revoke", nodeId);
    for (const l of this.revokeListeners) l(nodeId);
  }

  get(nodeId: string): NodeRow | undefined {
    return this.db.get<NodeRow>("SELECT * FROM nodes WHERE id = ?", nodeId);
  }

  list(): NodeRow[] {
    return this.db.all<NodeRow>("SELECT * FROM nodes ORDER BY id");
  }

  // ---- enrollment & sessions ---------------------------------------------------------------------------

  enroll(body: { enrollmentToken?: string; publicKey?: string; hostname?: string; arch?: string; plannerVersion?: string; plannerApi?: number }) {
    if (!body.enrollmentToken || !body.publicKey) throw new NodeApiError(400, "invalid-request", "enrollmentToken and publicKey are required");
    publicKeyFromRaw(body.publicKey); // validates shape
    if (body.plannerApi !== undefined && (body.plannerApi < MANAGER_API.min || body.plannerApi > MANAGER_API.max)) {
      throw new NodeApiError(409, "incompatible", `planner API ${body.plannerApi} not supported (manager ${MANAGER_API.min}-${MANAGER_API.max})`);
    }
    const tokenId = this.hash("enroll", body.enrollmentToken);
    return this.db.tx(() => {
      const t = this.db.get<{ node_id: string; expires_at: string; used_at: string | null }>("SELECT node_id, expires_at, used_at FROM enrollment_tokens WHERE id = ?", tokenId);
      if (!t || t.used_at || Date.parse(t.expires_at) < Date.now()) throw new NodeApiError(401, "invalid-enrollment-token", "enrollment token is invalid, used or expired");
      const node = this.get(t.node_id);
      if (!node || node.revoked_at) throw new NodeApiError(403, "node-revoked", "node is revoked");
      const now = nowIso();
      this.db.run("UPDATE enrollment_tokens SET used_at = ? WHERE id = ?", now, tokenId);
      // New identity replaces any previous key; outstanding tokens of the old identity die.
      this.db.run("UPDATE node_tokens SET revoked_at = ? WHERE node_id = ? AND revoked_at IS NULL", now, node.id);
      this.db.run("UPDATE nodes SET public_key = ?, enrolled_at = ?, hostname = ?, arch = ?, planner_version = ? WHERE id = ?",
        body.publicKey!, now, (body.hostname ?? "").slice(0, 253), (body.arch ?? "").slice(0, 32), (body.plannerVersion ?? "").slice(0, 64), node.id);
      this.audit.record(`node:${node.id}`, "node.enroll", node.id, { hostname: body.hostname, arch: body.arch });
      return { nodeId: node.id, behavior: node.behavior, managerApi: MANAGER_API };
    });
  }

  challenge(nodeId: string): { nonce: string; expiresInSeconds: number } {
    const node = this.get(nodeId);
    // Same answer shape for unknown nodes so the endpoint does not enumerate nodes.
    const nonce = randomBytes(32).toString("base64");
    if (node && node.public_key && !node.revoked_at) {
      this.db.run("DELETE FROM node_challenges WHERE expires_at < ?", nowIso());
      this.db.run("INSERT INTO node_challenges (nonce, node_id, expires_at) VALUES (?, ?, ?)", nonce, nodeId, isoIn(CHALLENGE_TTL));
    }
    return { nonce, expiresInSeconds: CHALLENGE_TTL };
  }

  openSession(body: { nodeId?: string; nonce?: string; signature?: string }): { token: string; expiresInSeconds: number } {
    if (!body.nodeId || !body.nonce || !body.signature) throw new NodeApiError(400, "invalid-request", "nodeId, nonce and signature are required");
    const ch = this.db.get<{ node_id: string; expires_at: string }>("SELECT node_id, expires_at FROM node_challenges WHERE nonce = ?", body.nonce);
    this.db.run("DELETE FROM node_challenges WHERE nonce = ?", body.nonce); // single use
    if (!ch || ch.node_id !== body.nodeId || Date.parse(ch.expires_at) < Date.now()) throw new NodeApiError(401, "invalid-challenge", "challenge invalid or expired");
    const node = this.get(body.nodeId);
    if (!node?.public_key || node.revoked_at) throw new NodeApiError(401, "unknown-node", "node not enrolled");
    const ok = cryptoVerify(null, sessionSigningInput(body.nodeId, body.nonce), publicKeyFromRaw(node.public_key), Buffer.from(body.signature, "base64"));
    if (!ok) throw new NodeApiError(401, "bad-signature", "signature verification failed");
    const token = `mnp-node-${randomToken(32)}`;
    this.db.run("DELETE FROM node_tokens WHERE node_id = ? AND (expires_at < ? OR revoked_at IS NOT NULL)", node.id, nowIso());
    this.db.run("INSERT INTO node_tokens (id, node_id, expires_at) VALUES (?, ?, ?)", this.hash("node", token), node.id, isoIn(NODE_TOKEN_TTL));
    return { token, expiresInSeconds: NODE_TOKEN_TTL };
  }

  /** Node rotates its key: request signed with the current key, carrying the new public key. */
  rotateKey(nodeId: string, body: { newPublicKey?: string; signature?: string }): void {
    if (!body.newPublicKey || !body.signature) throw new NodeApiError(400, "invalid-request", "newPublicKey and signature required");
    const node = this.get(nodeId)!;
    publicKeyFromRaw(body.newPublicKey);
    const input = Buffer.from(`meduza-np-node-rotate-v1\n${nodeId}\n${body.newPublicKey}`);
    if (!node.public_key || !cryptoVerify(null, input, publicKeyFromRaw(node.public_key), Buffer.from(body.signature, "base64"))) {
      throw new NodeApiError(401, "bad-signature", "rotation must be signed by the current key");
    }
    this.db.run("UPDATE nodes SET public_key = ? WHERE id = ?", body.newPublicKey, nodeId);
    this.audit.record(`node:${nodeId}`, "node.key-rotate", nodeId);
  }

  /** Resolves `Authorization: Bearer` to a node id, or null. */
  authenticate(authorization: string | undefined): string | null {
    const m = /^Bearer (mnp-node-[A-Za-z0-9_-]+)$/.exec(authorization ?? "");
    if (!m) return null;
    const row = this.db.get<{ node_id: string; expires_at: string; revoked_at: string | null }>("SELECT node_id, expires_at, revoked_at FROM node_tokens WHERE id = ?", this.hash("node", m[1]!));
    if (!row || row.revoked_at || Date.parse(row.expires_at) < Date.now()) return null;
    return row.node_id;
  }

  touch(nodeId: string): void {
    this.db.run("UPDATE nodes SET last_seen_at = ? WHERE id = ?", nowIso(), nodeId);
  }

  // ---- reports -----------------------------------------------------------------------------------------

  /** Stores a report unless it is older than what we have (same boot: lower/equal sequence; known old boot). */
  acceptReport(nodeId: string, report: { nodeId: string; bootId: string; sequence: number; observedAt: string } & Record<string, unknown>): { accepted: boolean; reason?: string } {
    if (report.nodeId !== nodeId) throw new NodeApiError(403, "node-mismatch", "report nodeId does not match the authenticated node");
    const now = nowIso();
    const prev = this.db.get<{ boot_id: string; sequence: number; previous_boots: string }>("SELECT boot_id, sequence, previous_boots FROM node_reports WHERE node_id = ?", nodeId);
    let previousBoots: string[] = prev ? (JSON.parse(prev.previous_boots) as string[]) : [];
    if (prev) {
      if (prev.boot_id === report.bootId && report.sequence <= prev.sequence) return { accepted: false, reason: "stale-sequence" };
      if (previousBoots.includes(report.bootId)) return { accepted: false, reason: "stale-boot" };
      if (prev.boot_id !== report.bootId) previousBoots = [prev.boot_id, ...previousBoots].slice(0, 16);
    }
    this.db.run(
      `INSERT INTO node_reports (node_id, boot_id, sequence, observed_at, received_at, previous_boots, report) VALUES (?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT(node_id) DO UPDATE SET boot_id = excluded.boot_id, sequence = excluded.sequence, observed_at = excluded.observed_at,
         received_at = excluded.received_at, previous_boots = excluded.previous_boots, report = excluded.report`,
      nodeId, report.bootId, report.sequence, report.observedAt, now, JSON.stringify(previousBoots), JSON.stringify(report),
    );
    const last = this.lastHistoryAt.get(nodeId) ?? 0;
    if (Date.now() - last > 60_000) {
      this.lastHistoryAt.set(nodeId, Date.now());
      const sync = (report.sync ?? {}) as Record<string, unknown>;
      const instances = (report.instances ?? []) as Array<{ state?: string }>;
      const summary = {
        syncState: sync.state,
        applied: sync.appliedRevision,
        instances: instances.length,
        up: instances.filter((i) => i.state === "up").length,
        down: instances.filter((i) => i.state === "down" || i.state === "degraded").length,
        resources: report.resources ?? null,
      };
      this.db.run("INSERT INTO report_history (node_id, received_at, summary) VALUES (?, ?, ?)", nodeId, now, JSON.stringify(summary));
      this.db.run("DELETE FROM report_history WHERE node_id = ? AND id <= (SELECT MAX(id) FROM report_history WHERE node_id = ?) - ?", nodeId, nodeId, HISTORY_PER_NODE);
    }
    return { accepted: true };
  }

  latestReport(nodeId: string): { report: Record<string, unknown>; receivedAt: string } | undefined {
    const r = this.db.get<{ report: string; received_at: string }>("SELECT report, received_at FROM node_reports WHERE node_id = ?", nodeId);
    return r ? { report: JSON.parse(r.report) as Record<string, unknown>, receivedAt: r.received_at } : undefined;
  }

  history(nodeId: string, limit = 200) {
    return this.db.all<{ received_at: string; summary: string }>("SELECT received_at, summary FROM report_history WHERE node_id = ? ORDER BY id DESC LIMIT ?", nodeId, limit)
      .map((r) => ({ receivedAt: r.received_at, ...(JSON.parse(r.summary) as Record<string, unknown>) }));
  }

  // ---- ACKs --------------------------------------------------------------------------------------------

  ack(nodeId: string, body: { revision: string; configDigest: string; stage: string; bootId: string; errorCode?: string; message?: string; instances?: unknown[] }): { duplicate: boolean } {
    const stages = ["received", "validated", "applied", "failed", "rejected"];
    if (!stages.includes(body.stage)) throw new NodeApiError(400, "invalid-stage", "unknown stage");
    const revNum = revisionNumber(body.revision);
    if (revNum === null) throw new NodeApiError(400, "invalid-revision", "invalid revision");
    const snap = this.db.get<{ digest: string }>("SELECT digest FROM node_snapshots WHERE node_id = ? AND revision = ?", nodeId, body.revision);
    if (!snap) throw new NodeApiError(404, "unknown-revision", "revision was never published for this node");
    if (snap.digest !== body.configDigest) throw new NodeApiError(409, "digest-mismatch", "config digest does not match the published snapshot");
    const now = nowIso();
    return this.db.tx(() => {
      const r = this.db.run(
        "INSERT OR IGNORE INTO acks (node_id, revision, stage, boot_id, config_digest, error_code, message, instances, received_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        nodeId, body.revision, body.stage, body.bootId, body.configDigest, body.errorCode ?? null, (body.message ?? "").slice(0, 2000) || null,
        JSON.stringify(body.instances ?? []).slice(0, 64_000), now,
      );
      if (r.changes === 0) return { duplicate: true };
      const column = { received: "received_revision", validated: "validated_revision", applied: "applied_revision", failed: "failed_revision", rejected: "failed_revision" }[body.stage]!;
      const current = this.db.get<Record<string, string | null>>(`SELECT ${column} AS rev FROM node_sync WHERE node_id = ?`, nodeId);
      const currentNum = current?.rev ? revisionNumber(current.rev) ?? -1 : -1;
      // Out-of-order ACKs never move a pointer backwards.
      if (revNum >= currentNum) {
        this.db.run(
          `INSERT INTO node_sync (node_id, ${column}, last_error, updated_at) VALUES (?, ?, ?, ?)
           ON CONFLICT(node_id) DO UPDATE SET ${column} = excluded.${column}, last_error = excluded.last_error, updated_at = excluded.updated_at`,
          nodeId, body.revision, body.stage === "failed" || body.stage === "rejected" ? `${body.errorCode ?? "error"}: ${body.message ?? ""}` : null, now,
        );
      }
      this.db.run("UPDATE publish_results SET stage = ?, error_code = ?, message = ?, updated_at = ? WHERE node_id = ? AND node_revision = ? AND changed = 1",
        body.stage, body.errorCode ?? null, body.message ?? null, now, nodeId, body.revision);
      return { duplicate: false };
    });
  }

  syncState(nodeId: string) {
    return this.db.get<Record<string, unknown>>("SELECT * FROM node_sync WHERE node_id = ?", nodeId);
  }

  // ---- jobs --------------------------------------------------------------------------------------------

  createJob(input: { nodeId: string; kind: string; package?: string; instance?: string; params?: unknown; ttlSeconds?: number }, actor: string): string {
    const node = this.get(input.nodeId);
    if (!node || node.revoked_at) throw new NodeApiError(404, "not-found", "node not found");
    const update = input.kind === "update-check" || input.kind === "update-apply";
    if (update) {
      if (input.package !== undefined || input.instance !== undefined || (input.params != null &&
        (typeof input.params !== "object" || Array.isArray(input.params) || Object.keys(input.params).length !== 0))) {
        throw new NodeApiError(400, "invalid-params", "update requests use the node's configured signed source and accept no package, instance or parameters");
      }
      const progress = this.latestReport(input.nodeId)?.report.update as { remoteControl?: boolean } | undefined;
      if (progress?.remoteControl !== true) throw new NodeApiError(409, "update-unsupported", "update planner and run updater on this node to enable remote upgrade control");
      const queued = this.db.get<{ id: string; kind: string }>(
        "SELECT id, kind FROM jobs WHERE node_id = ? AND kind IN ('update-check','update-apply') AND status IN ('pending','delivered') AND expires_at > ? ORDER BY created_at LIMIT 1", input.nodeId, nowIso());
      if (queued) {
        if (queued.kind === input.kind) return queued.id;
        throw new NodeApiError(409, "update-pending", "another update request is waiting for the node");
      }
    }
    const id = randomId("job");
    this.db.run(
      "INSERT INTO jobs (id, node_id, kind, package, instance, params, created_by, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
      id, input.nodeId, input.kind, input.package ?? null, input.instance ?? null, JSON.stringify(input.params ?? {}), actor, nowIso(), isoIn(input.ttlSeconds ?? (update ? 600 : 3600)),
    );
    const seq = this.config.appendEvent(input.nodeId, "job.available", null, null, id);
    this.audit.record(actor, "job.create", id, { node: input.nodeId, kind: input.kind, package: input.package, instance: input.instance });
    for (const l of this.jobListeners) l(input.nodeId, id, seq);
    return id;
  }

  pendingJobs(nodeId: string) {
    const now = nowIso();
    this.db.run("UPDATE jobs SET status = 'expired', finished_at = ? WHERE node_id = ? AND status IN ('pending','delivered') AND expires_at < ?", now, nodeId, now);
    const rows = this.db.all<{ id: string; kind: string; package: string | null; instance: string | null; params: string; expires_at: string }>(
      "SELECT id, kind, package, instance, params, expires_at FROM jobs WHERE node_id = ? AND status IN ('pending','delivered') ORDER BY created_at LIMIT 20", nodeId);
    this.db.run("UPDATE jobs SET status = 'delivered', delivered_at = COALESCE(delivered_at, ?) WHERE node_id = ? AND status = 'pending'", now, nodeId);
    return rows.map((r) => ({
      jobId: r.id, kind: r.kind, ...(r.package ? { package: r.package } : {}), ...(r.instance ? { instance: r.instance } : {}),
      params: JSON.parse(r.params) as unknown, expiresAt: r.expires_at,
    }));
  }

  jobResult(nodeId: string, jobId: string, body: { ok: boolean; result?: unknown; errorCode?: string; message?: string }): { duplicate: boolean; job: Record<string, unknown> } {
    const job = this.db.get<Record<string, unknown>>("SELECT * FROM jobs WHERE id = ? AND node_id = ?", jobId, nodeId);
    if (!job) throw new NodeApiError(404, "not-found", "job not found");
    if (["succeeded", "failed", "expired", "cancelled"].includes(String(job.status))) return { duplicate: true, job };
    this.db.run("UPDATE jobs SET status = ?, finished_at = ?, result = ?, error_code = ?, message = ? WHERE id = ?",
      body.ok ? "succeeded" : "failed", nowIso(), boundedResult(body.result), body.errorCode ?? null, (body.message ?? "").slice(0, 2000) || null, jobId);
    return { duplicate: false, job: this.db.get<Record<string, unknown>>("SELECT * FROM jobs WHERE id = ?", jobId)! };
  }

  jobs(nodeId?: string, limit = 100) {
    return nodeId
      ? this.db.all<Record<string, unknown>>("SELECT * FROM jobs WHERE node_id = ? ORDER BY created_at DESC LIMIT ?", nodeId, limit)
      : this.db.all<Record<string, unknown>>("SELECT * FROM jobs ORDER BY created_at DESC LIMIT ?", limit);
  }
}
