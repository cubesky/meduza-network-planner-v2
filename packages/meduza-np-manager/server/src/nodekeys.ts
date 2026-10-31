// Node-held keys requested from the console (D28). The private half of a Tinc/WireGuard key is generated on the node
// by its package, so a node that does not have the package yet cannot generate it — and before D28 its configuration
// could not be applied without the key, so the package was never installed. Now:
//   1. a request records the key as pending (a node-held secret without a public value): the configuration applies,
//      the node's updater installs the package, the feature waits on the node for its key;
//   2. as soon as the node reports the package registered, the generation job is queued (again after a failed
//      attempt, e.g. when the job ran before the package was ready, up to MAX_ATTEMPTS);
//   3. when the public key arrives, the current revision is published again so the other nodes receive it
//      (compiled snapshots change, the document does not).

import type { Audit } from "./audit.js";
import type { Db } from "./db.js";
import { nowIso } from "./db.js";
import type { NodeService } from "./nodes.js";
import type { ConfigService } from "./publish.js";
import { isError } from "./cluster.js";
import type { SecretKind, SecretStore } from "./secrets.js";

/** Keys a node generates itself (its package's `generate-keypair`). */
export const NODE_KEY_KINDS: SecretKind[] = ["tinc-rsa-private", "tinc-ed25519-private", "wireguard-private-key", "pem-private-key"];
const MAX_ATTEMPTS = 5;
const SYSTEM = "system";

interface Row {
  secret_id: string;
  node_id: string;
  kind: string;
  package: string;
  replace: number;
  attempts: number;
  job_id: string | null;
}

export class NodeKeys {
  constructor(
    private readonly db: Db,
    private readonly secrets: SecretStore,
    private readonly nodes: NodeService,
    private readonly config: ConfigService,
    private readonly audit: Audit,
  ) {}

  /** Requests a key on `node`; generated at once when the package is there, otherwise once it is installed. */
  request(node: string, input: { secretId: string; keyKind: SecretKind; package: string; replace?: boolean }, actor: string): { pending: boolean; jobId: string | null } {
    if (!NODE_KEY_KINDS.includes(input.keyKind)) throw new Error(`${input.keyKind} is not generated on nodes`);
    const meta = this.secrets.meta(input.secretId);
    if (meta && (meta.holder !== "node" || meta.holderNode !== node)) throw new Error(`secret ${input.secretId} already exists and is not held by ${node}`);
    if (meta?.publicValue && !input.replace) return { pending: false, jobId: null };
    if (!meta) this.secrets.putPending({ id: input.secretId, kind: input.keyKind, node });
    this.db.run(
      `INSERT INTO key_requests (secret_id, node_id, kind, package, replace, attempts, job_id, requested_by, created_at) VALUES (?, ?, ?, ?, ?, 0, NULL, ?, ?)
       ON CONFLICT(secret_id) DO UPDATE SET node_id = excluded.node_id, kind = excluded.kind, package = excluded.package,
         replace = excluded.replace, attempts = 0, job_id = NULL, requested_by = excluded.requested_by, created_at = excluded.created_at`,
      input.secretId, node, input.keyKind, input.package, input.replace ? 1 : 0, actor, nowIso(),
    );
    this.audit.record(actor, "node-key.request", input.secretId, { node, kind: input.keyKind, package: input.package });
    return { pending: true, jobId: this.kick(node) ?? null };
  }

  /** Pending key ids of `node` (the console shows them as "generated once the package is installed"). */
  pending(node?: string): Array<{ secretId: string; node: string; package: string; attempts: number; queued: boolean }> {
    const rows = node ? this.db.all<Row>("SELECT * FROM key_requests WHERE node_id = ?", node) : this.db.all<Row>("SELECT * FROM key_requests");
    return rows.map((r) => ({ secretId: r.secret_id, node: r.node_id, package: r.package, attempts: r.attempts, queued: !!r.job_id }));
  }

  /** Queues the generation jobs of `node` whose package it reports as registered; returns the last job id. */
  kick(node: string): string | undefined {
    const report = this.nodes.latestReport(node)?.report as { packages?: Array<{ name?: string; registration?: string }> } | undefined;
    const registered = new Set((report?.packages ?? []).filter((p) => p.registration === "registered").map((p) => String(p.name)));
    let last: string | undefined;
    for (const r of this.db.all<Row>("SELECT * FROM key_requests WHERE node_id = ?", node)) {
      if (r.job_id) {
        const job = this.db.get<{ status: string }>("SELECT status FROM jobs WHERE id = ?", r.job_id);
        if (job && !["failed", "expired", "cancelled"].includes(job.status)) continue; // still on its way
        this.db.run("UPDATE key_requests SET job_id = NULL WHERE secret_id = ?", r.secret_id);
      }
      if (!registered.has(r.package) || r.attempts >= MAX_ATTEMPTS) continue;
      const id = this.nodes.createJob(
        { nodeId: node, kind: "generate-keypair", package: r.package, params: { secretId: r.secret_id, keyKind: r.kind, package: r.package, replace: !!r.replace } },
        SYSTEM,
      );
      this.db.run("UPDATE key_requests SET job_id = ?, attempts = attempts + 1 WHERE secret_id = ?", id, r.secret_id);
      last = id;
    }
    return last;
  }

  /** A generation job finished: records the public key, ends the request and distributes the key. */
  finished(node: string, job: Record<string, unknown>, ok: boolean, publicKey: string | undefined): void {
    const params = JSON.parse(String(job.params ?? "{}")) as { secretId?: string; keyKind?: SecretKind };
    if (!params.secretId) return;
    if (!ok) {
      this.kick(node); // retried once the cause (package not ready yet) is gone, up to MAX_ATTEMPTS
      return;
    }
    if (params.keyKind && publicKey) this.secrets.putNodeHeld({ id: params.secretId, kind: params.keyKind, node, publicValue: publicKey });
    this.db.run("DELETE FROM key_requests WHERE secret_id = ?", params.secretId);
    this.distribute(params.secretId, node);
  }

  /** Publishes the current document again when the new key changes what nodes receive. */
  private distribute(secretId: string, node: string): void {
    const current = this.config.currentDocument();
    if (!current.revision) return;
    const preview = this.config.preview(current.document);
    if (preview.problems.some(isError) || !preview.nodes.some((n) => n.changed)) return;
    try {
      this.config.publish(current.document, SYSTEM, { message: `distribute key ${secretId} generated on ${node}` });
    } catch (e) {
      this.audit.record(SYSTEM, "node-key.distribute-failed", secretId, { node, error: e instanceof Error ? e.message : String(e) });
    }
  }
}
