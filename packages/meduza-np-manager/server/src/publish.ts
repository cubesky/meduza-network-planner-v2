// Drafts, validation, publishing, rollback and per-node desired revisions (AGENT.md §6.2, §7.2, D5).

import type { Db } from "./db.js";
import { nowIso } from "./db.js";
import type { SecretStore } from "./secrets.js";
import type { PackageCatalog } from "./packages.js";
import type { Audit } from "./audit.js";
import { randomId } from "./audit.js";
import { canonicalDigest } from "./canonical.js";
import {
  compileCluster,
  emptyDocument,
  isError,
  materialize,
  revisionId,
  type ClusterDocument,
  type CompileResult,
  type NodeCapabilities,
  type Problem,
  type SecretSource,
} from "./cluster.js";
import { redactedDiff, type DiffEntry } from "./diff.js";

/** Secrets that do not exist yet but are assumed by a preview: an import's preview (D58) shows the result of staging
 * it, so the values it would store count as present and the node keys it would request as pending. */
export interface AssumedSecrets {
  present: Map<string, { kind: string; value: string; publicValue: string | null; nodeScope: string[] }>;
  pending: Set<string>;
}

/** Three-way merge of staged changes (`mine`, made on `base`) onto `theirs`, per node and per cluster section. */
export function rebaseDocument(base: ClusterDocument, mine: ClusterDocument, theirs: ClusterDocument): { document: ClusterDocument; conflicts: string[] } {
  const same = (a: unknown, b: unknown) => canonicalDigest(a ?? null) === canonicalDigest(b ?? null);
  const conflicts: string[] = [];
  const pick = <T>(path: string, b: T | undefined, m: T | undefined, th: T | undefined): T | undefined => {
    if (same(m, b)) return th;
    if (!same(th, b) && !same(th, m)) conflicts.push(path);
    return m;
  };
  const nodes: ClusterDocument["nodes"] = {};
  for (const k of [...new Set([...Object.keys(base.nodes ?? {}), ...Object.keys(mine.nodes ?? {}), ...Object.keys(theirs.nodes ?? {})])].sort()) {
    const v = pick(`/nodes/${k}`, base.nodes?.[k], mine.nodes?.[k], theirs.nodes?.[k]);
    if (v !== undefined) nodes[k] = v;
  }
  // every other top-level section (routing, defaults, mihomo, dns, tinc, mosdns…) merges as a unit
  const document = { ...theirs, nodes } as ClusterDocument & Record<string, unknown>;
  const sections = new Set([...Object.keys(base), ...Object.keys(mine), ...Object.keys(theirs)].filter((k) => k !== "nodes" && k !== "schemaVersion"));
  for (const k of [...sections].sort()) {
    const get = (d: ClusterDocument) => (d as unknown as Record<string, unknown>)[k];
    const v = pick(`/${k}`, get(base), get(mine), get(theirs));
    if (v !== undefined) document[k] = v;
    else delete document[k];
  }
  return { document, conflicts };
}

export class ValidationFailed extends Error {
  constructor(readonly problems: Problem[]) {
    super(`${problems.length} validation problem(s)`);
  }
}

export interface NodePreview {
  nodeId: string;
  changed: boolean;
  currentRevision: string | null;
  diff: DiffEntry[];
}

export interface PublishOutcome {
  revision: string;
  nodes: Array<{ nodeId: string; nodeRevision: string; changed: boolean }>;
}

export type ConfigListener = (nodeId: string, revision: string, digest: string, seq: number) => void;

const MAX_EVENTS_PER_NODE = 500;

export class ConfigService {
  private listeners: ConfigListener[] = [];

  constructor(
    private readonly db: Db,
    private readonly secrets: SecretStore,
    private readonly catalog: PackageCatalog,
    private readonly audit: Audit,
  ) {}

  onConfigAvailable(fn: ConfigListener): void {
    this.listeners.push(fn);
  }

  // ---- documents --------------------------------------------------------------------------------------

  currentDocument(): { revision: string | null; number: number | null; document: ClusterDocument } {
    const row = this.db.get<{ number: number; revision: string; document: string }>(
      "SELECT number, revision, document FROM cluster_revisions ORDER BY number DESC LIMIT 1",
    );
    return row
      ? { revision: row.revision, number: row.number, document: JSON.parse(row.document) as ClusterDocument }
      : { revision: null, number: null, document: emptyDocument() };
  }

  listRevisions(limit = 100) {
    return this.db.all<Record<string, unknown>>(
      "SELECT number, revision, document_digest, kind, rollback_of, message, created_by, created_at FROM cluster_revisions ORDER BY number DESC LIMIT ?",
      limit,
    );
  }

  createDraft(actor: string, document?: ClusterDocument): { id: string; baseRevision: number | null } {
    const current = this.currentDocument();
    const id = randomId("draft");
    const now = nowIso();
    this.db.run(
      "INSERT INTO drafts (id, base_revision, document, created_by, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
      id, current.number, JSON.stringify(document ?? current.document), actor, now, now,
    );
    this.audit.record(actor, "draft.create", id);
    return { id, baseRevision: current.number };
  }

  getDraft(id: string): { id: string; baseRevision: number | null; document: ClusterDocument; status: string; updatedAt: string } | undefined {
    const r = this.db.get<{ id: string; base_revision: number | null; document: string; status: string; updated_at: string }>("SELECT * FROM drafts WHERE id = ?", id);
    return r ? { id: r.id, baseRevision: r.base_revision, document: JSON.parse(r.document) as ClusterDocument, status: r.status, updatedAt: r.updated_at } : undefined;
  }

  listDrafts() {
    return this.db.all<Record<string, unknown>>("SELECT id, base_revision, created_by, created_at, updated_at, status FROM drafts WHERE status = 'open' ORDER BY updated_at DESC");
  }

  updateDraft(id: string, document: ClusterDocument, actor: string): void {
    const r = this.db.run("UPDATE drafts SET document = ?, updated_at = ? WHERE id = ? AND status = 'open'", JSON.stringify(document), nowIso(), id);
    if (r.changes === 0) throw new Error("draft not found or not open");
    this.audit.record(actor, "draft.update", id);
  }

  discardDraft(id: string, actor: string): void {
    this.db.run("UPDATE drafts SET status = 'discarded', updated_at = ? WHERE id = ? AND status = 'open'", nowIso(), id);
    this.audit.record(actor, "draft.discard", id);
  }

  /** The account's staged changes (D23): its newest open draft, if any. */
  myDraft(actor: string) {
    const r = this.db.get<{ id: string }>("SELECT id FROM drafts WHERE created_by = ? AND status = 'open' ORDER BY updated_at DESC LIMIT 1", actor);
    return r ? this.getDraft(r.id) : undefined;
  }

  /**
   * Moves staged changes onto the current revision after someone else published (three-way, per node and per
   * cluster section). Sections only the account changed keep its version; sections only the others changed take
   * theirs; sections both changed keep the account's and are reported as conflicts to review before applying.
   */
  rebaseDraft(id: string, actor: string): { conflicts: string[]; baseRevision: string | null } {
    const d = this.getDraft(id);
    if (!d || d.status !== "open") throw new Error("draft not found or not open");
    const baseRow = d.baseRevision === null ? undefined : this.db.get<{ document: string }>("SELECT document FROM cluster_revisions WHERE number = ?", d.baseRevision);
    const base = baseRow ? (JSON.parse(baseRow.document) as ClusterDocument) : emptyDocument();
    const current = this.currentDocument();
    const merged = rebaseDocument(base, d.document, current.document);
    this.db.run("UPDATE drafts SET document = ?, base_revision = ?, updated_at = ? WHERE id = ?", JSON.stringify(merged.document), current.number, nowIso(), id);
    this.audit.record(actor, "draft.rebase", id, { onto: current.revision, conflicts: merged.conflicts });
    return { conflicts: merged.conflicts, baseRevision: current.revision };
  }

  // ---- compilation ------------------------------------------------------------------------------------

  private knownNodes(): Set<string> {
    return new Set(this.db.all<{ id: string }>("SELECT id FROM nodes WHERE revoked_at IS NULL").map((r) => r.id));
  }

  capabilitiesOf = (nodeId: string): NodeCapabilities => {
    const row = this.db.get<{ report: string }>("SELECT report FROM node_reports WHERE node_id = ?", nodeId);
    if (!row) return { reported: false, packages: new Set(), capabilities: new Set() };
    const report = JSON.parse(row.report) as {
      packages?: Array<{ name: string; registration: string; provides?: string[] }>;
      system?: { networkMode?: string; capabilities?: string[] };
    };
    const packages = new Set<string>();
    const capabilities = new Set<string>();
    for (const p of report.packages ?? []) {
      if (p.registration !== "registered") continue;
      packages.add(p.name);
      for (const c of p.provides ?? []) capabilities.add(c);
    }
    const mode = report.system?.networkMode;
    const linux = report.system?.capabilities;
    return {
      reported: true,
      packages,
      capabilities,
      ...(mode === "bridge" || mode === "host" ? { networkMode: mode } : {}),
      ...(Array.isArray(linux) ? { linuxCapabilities: new Set(linux) } : {}),
    };
  };

  private secretSource(assume?: AssumedSecrets): SecretSource {
    return {
      resolve: (ref, node) => {
        const meta = this.secrets.meta(ref);
        if (!meta) {
          if (assume?.pending.has(ref)) return null; // the node generates it (D28)
          const a = assume?.present.get(ref);
          if (a && (a.nodeScope.includes(node) || a.nodeScope.includes("*"))) return { kind: a.kind, value: a.value };
          return { error: "does not exist" };
        }
        if (meta.holder === "node") return meta.holderNode === node ? null : { error: `held by node ${meta.holderNode}` };
        // `*`: shared with every node that references it (global Mihomo subscription URLs, D29)
        if (!meta.nodeScope.includes(node) && !meta.nodeScope.includes("*")) return { error: `not shared with ${node}` };
        return { kind: meta.kind, value: this.secrets.reveal(ref) };
      },
      publicValue: (ref) => this.secrets.meta(ref)?.publicValue ?? assume?.present.get(ref)?.publicValue ?? null,
      pending: (ref) => {
        const meta = this.secrets.meta(ref);
        return (!!meta && meta.holder === "node" && !meta.publicValue) || (!meta && !!assume?.pending.has(ref));
      },
    };
  }

  /** Nodes that currently have a desired config must keep receiving one even if removed from the document. */
  private withRetainedNodes(doc: ClusterDocument): ClusterDocument {
    const out: ClusterDocument = { ...doc, nodes: { ...doc.nodes } };
    const known = this.knownNodes();
    for (const { node_id } of this.db.all<{ node_id: string }>("SELECT node_id FROM node_desired")) {
      if (!(node_id in out.nodes) && known.has(node_id)) out.nodes[node_id] = {};
    }
    return out;
  }

  compile(doc: ClusterDocument, assume?: AssumedSecrets): CompileResult {
    return compileCluster(this.withRetainedNodes(doc), this.knownNodes(), this.capabilitiesOf, this.secretSource(assume), this.catalog);
  }

  /** Validation + redacted per-node diff against the current desired snapshots, without persisting. */
  preview(doc: ClusterDocument, assume?: AssumedSecrets): { problems: Problem[]; nodes: NodePreview[] } {
    const result = this.compile(doc, assume);
    const nodes = result.nodes.map((n) => {
      const current = this.desired(n.nodeId);
      const before = current ? (JSON.parse(this.db.get<{ config: string }>("SELECT config FROM node_snapshots WHERE node_id = ? AND revision = ?", n.nodeId, current.revision)!.config) as Record<string, unknown>) : {};
      const changed = !current || current.contentDigest !== n.contentDigest;
      const { revision: _a, ...b } = before;
      const { revision: _b, ...a } = n.config;
      const secretNote = (s: Record<string, unknown>) => Object.keys(s).sort();
      return {
        nodeId: n.nodeId,
        changed,
        currentRevision: current?.revision ?? null,
        diff: changed ? redactedDiff({ ...b, secretRefs: current ? this.snapshotSecretRefs(n.nodeId, current.revision) : [] }, { ...a, secretRefs: secretNote(n.secrets) }) : [],
      };
    });
    return { problems: result.problems, nodes };
  }

  private snapshotSecretRefs(nodeId: string, revision: string): string[] {
    const row = this.db.get<{ secrets_ciphertext: Uint8Array | null; secrets_nonce: Uint8Array | null }>(
      "SELECT secrets_ciphertext, secrets_nonce FROM node_snapshots WHERE node_id = ? AND revision = ?", nodeId, revision);
    if (!row?.secrets_ciphertext || !row.secrets_nonce) return [];
    return Object.keys(this.secrets.openEnvelope<Record<string, unknown>>(`${nodeId}/${revision}`, row.secrets_ciphertext, row.secrets_nonce)).sort();
  }

  // ---- publishing -------------------------------------------------------------------------------------

  publish(doc: ClusterDocument, actor: string, opts: { message?: string; draftId?: string; rollbackOf?: string } = {}): PublishOutcome {
    const notify: Array<[string, string, string, number]> = [];
    const outcome = this.db.tx(() => {
      const result = this.compile(doc);
      const errors = result.problems.filter(isError);
      if (errors.length) throw new ValidationFailed(errors);
      const last = this.db.get<{ n: number | null }>("SELECT MAX(number) AS n FROM cluster_revisions")?.n ?? 0;
      const number = last + 1;
      const revision = revisionId(number);
      const now = nowIso();
      this.db.run(
        "INSERT INTO cluster_revisions (number, revision, document, document_digest, kind, rollback_of, message, created_by, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        number, revision, JSON.stringify(doc), canonicalDigest(doc), opts.rollbackOf ? "rollback" : "publish", opts.rollbackOf ?? null, opts.message ?? null, actor, now,
      );
      const nodes: PublishOutcome["nodes"] = [];
      for (const n of result.nodes) {
        const current = this.desired(n.nodeId);
        if (current && current.contentDigest === n.contentDigest) {
          nodes.push({ nodeId: n.nodeId, nodeRevision: current.revision, changed: false });
          this.db.run(
            "INSERT INTO publish_results (publish_revision, node_id, node_revision, changed, stage, updated_at) VALUES (?, ?, ?, 0, 'unchanged', ?)",
            revision, n.nodeId, current.revision, now,
          );
          continue;
        }
        const full = materialize(n, revision);
        const digest = canonicalDigest(full);
        const sealed = Object.keys(n.secrets).length ? this.secrets.sealEnvelope(`${n.nodeId}/${revision}`, n.secrets) : null;
        this.db.run(
          "INSERT INTO node_snapshots (node_id, revision, config, digest, content_digest, secrets_ciphertext, secrets_nonce, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
          n.nodeId, revision, JSON.stringify({ ...n.config, revision }), digest, n.contentDigest, sealed?.ciphertext ?? null, sealed?.nonce ?? null, now,
        );
        this.db.run(
          `INSERT INTO node_desired (node_id, revision, digest, content_digest, published_in, updated_at) VALUES (?, ?, ?, ?, ?, ?)
           ON CONFLICT(node_id) DO UPDATE SET revision = excluded.revision, digest = excluded.digest, content_digest = excluded.content_digest,
             published_in = excluded.published_in, updated_at = excluded.updated_at`,
          n.nodeId, revision, digest, n.contentDigest, revision, now,
        );
        this.db.run(
          "INSERT INTO publish_results (publish_revision, node_id, node_revision, changed, stage, updated_at) VALUES (?, ?, ?, 1, 'pending', ?)",
          revision, n.nodeId, revision, now,
        );
        const spec = doc.nodes[n.nodeId];
        if (spec) this.db.run("UPDATE nodes SET behavior = ?, display_name = COALESCE(?, display_name), tags = ? WHERE id = ?",
          spec.behavior ?? "static", spec.displayName ?? null, JSON.stringify(spec.tags ?? []), n.nodeId);
        const seq = this.appendEvent(n.nodeId, "config.available", revision, digest, null);
        notify.push([n.nodeId, revision, digest, seq]);
        nodes.push({ nodeId: n.nodeId, nodeRevision: revision, changed: true });
      }
      if (opts.draftId) this.db.run("UPDATE drafts SET status = 'published', updated_at = ? WHERE id = ?", now, opts.draftId);
      return { revision, nodes };
    });
    this.audit.record(actor, opts.rollbackOf ? "config.rollback" : "config.publish", outcome.revision, {
      message: opts.message,
      rollbackOf: opts.rollbackOf,
      changedNodes: outcome.nodes.filter((n) => n.changed).map((n) => n.nodeId),
    });
    for (const [node, rev, digest, seq] of notify) for (const l of this.listeners) l(node, rev, digest, seq);
    return outcome;
  }

  /** Rollback = publishing an earlier document again as a new revision (history is never rewritten). */
  rollback(toRevision: string, actor: string, message?: string): PublishOutcome {
    const row = this.db.get<{ document: string }>("SELECT document FROM cluster_revisions WHERE revision = ?", toRevision);
    if (!row) throw new Error(`revision ${toRevision} not found`);
    return this.publish(JSON.parse(row.document) as ClusterDocument, actor, { message: message ?? `rollback to ${toRevision}`, rollbackOf: toRevision });
  }

  publishResults(revision: string) {
    return this.db.all<Record<string, unknown>>(
      `SELECT r.node_id, r.node_revision, r.changed, r.stage, r.error_code, r.message, r.updated_at,
              s.received_revision, s.validated_revision, s.applied_revision, s.failed_revision, s.last_error
       FROM publish_results r LEFT JOIN node_sync s ON s.node_id = r.node_id WHERE r.publish_revision = ? ORDER BY r.node_id`,
      revision,
    );
  }

  // ---- node side --------------------------------------------------------------------------------------

  desired(nodeId: string): { revision: string; digest: string; contentDigest: string } | undefined {
    const r = this.db.get<{ revision: string; digest: string; content_digest: string }>("SELECT revision, digest, content_digest FROM node_desired WHERE node_id = ?", nodeId);
    return r ? { revision: r.revision, digest: r.digest, contentDigest: r.content_digest } : undefined;
  }

  /** The immutable snapshot of `revision` for `node`, with secrets, as delivered to the node. */
  snapshot(nodeId: string, revision: string): { config: Record<string, unknown>; digest: string; createdAt: string } | undefined {
    const r = this.db.get<{ config: string; digest: string; created_at: string; secrets_ciphertext: Uint8Array | null; secrets_nonce: Uint8Array | null }>(
      "SELECT config, digest, created_at, secrets_ciphertext, secrets_nonce FROM node_snapshots WHERE node_id = ? AND revision = ?",
      nodeId, revision,
    );
    if (!r) return undefined;
    const config = JSON.parse(r.config) as Record<string, unknown>;
    if (r.secrets_ciphertext && r.secrets_nonce) {
      config.secrets = this.secrets.openEnvelope(`${nodeId}/${revision}`, r.secrets_ciphertext, r.secrets_nonce);
    }
    return { config, digest: r.digest, createdAt: r.created_at };
  }

  appendEvent(nodeId: string, type: string, revision: string | null, digest: string | null, jobId: string | null): number {
    const seq = (this.db.get<{ s: number | null }>("SELECT MAX(seq) AS s FROM node_events WHERE node_id = ?", nodeId)?.s ?? 0) + 1;
    this.db.run("INSERT INTO node_events (node_id, seq, type, revision, digest, job_id, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
      nodeId, seq, type, revision, digest, jobId, nowIso());
    this.db.run("DELETE FROM node_events WHERE node_id = ? AND seq <= ?", nodeId, seq - MAX_EVENTS_PER_NODE);
    return seq;
  }

  eventsAfter(nodeId: string, cursor: number): { events: Array<Record<string, unknown>>; cursorValid: boolean } {
    const oldest = this.db.get<{ s: number | null }>("SELECT MIN(seq) AS s FROM node_events WHERE node_id = ?", nodeId)?.s;
    const newest = this.db.get<{ s: number | null }>("SELECT MAX(seq) AS s FROM node_events WHERE node_id = ?", nodeId)?.s ?? 0;
    // A cursor ahead of the newest event (manager restored from backup) or older than retained history is a gap.
    const cursorValid = cursor <= newest && (oldest === null || oldest === undefined || cursor >= oldest - 1);
    if (!cursorValid) return { events: [], cursorValid: false };
    return { events: this.db.all("SELECT * FROM node_events WHERE node_id = ? AND seq > ? ORDER BY seq LIMIT 100", nodeId, cursor), cursorValid: true };
  }
}
