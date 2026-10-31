// Applying an etcd export import (D58): nodes, secrets, node-held keys, and the document as the account's saved
// changes (never published directly: the operator reviews and applies it like any other change).

import { createPublicKey, randomBytes } from "node:crypto";
import type { ClusterDocument, Problem } from "../cluster.js";
import { isError } from "../cluster.js";
import { wireGuardPublicKey } from "../keys.js";
import type { Services } from "../app.js";
import type { SecretKind } from "../secrets.js";
import { convertEtcdExport, setPath, type ImportResult } from "./etcd.js";
import type { AssumedSecrets } from "../publish.js";

export interface ImportOptions {
  /** values for the needs, by need id */
  values?: Record<string, string>;
  /** create the export's nodes that the manager does not know yet (pre-created, enrolled later) */
  createNodes?: boolean;
  /** `replace`: the import becomes the whole document; `merge`: its nodes and global sections replace those parts */
  mode?: "replace" | "merge";
}

export interface ImportPreview extends ImportResult {
  existingNodes: string[];
  existingSecrets: string[];
  problems: Problem[];
}

/** The document as it would be staged, with every value that is not supplied switched off. */
function finalDocument(s: Services, r: ImportResult, opts: ImportOptions): { document: ClusterDocument; disabled: string[] } {
  const doc = structuredClone(r.document) as ClusterDocument & Record<string, unknown>;
  const disabled: string[] = [];
  for (const n of r.needs) {
    const value = opts.values?.[n.id]?.trim() || n.value;
    if (n.kind === "public-key") {
      if (value) setPath(doc, n.id.slice("public:".length), value);
    }
    const available = !!value || (n.kind !== "public-key" && (n.generated || !!s.secrets.meta(n.id)));
    if (!available && n.disables && !disabled.includes(n.disables)) {
      setPath(doc, n.disables, false);
      disabled.push(n.disables);
    }
  }
  if (opts.mode !== "merge") return { document: doc, disabled };
  const current = s.config.currentDocument().document as ClusterDocument & Record<string, unknown>;
  const merged: ClusterDocument & Record<string, unknown> = { ...current, nodes: { ...current.nodes, ...doc.nodes } };
  for (const [k, v] of Object.entries(doc)) if (k !== "nodes" && k !== "schemaVersion") merged[k] = v;
  return { document: merged, disabled };
}

/** The secrets the import stores, each value checked first: a bad key fails the preview, and leaves no half-done
 * import behind. */
function importSecrets(s: Services, r: ImportResult, opts: ImportOptions) {
  const out: Array<{ need: ImportResult["needs"][number]; value: string; publicValue: string | null }> = [];
  for (const n of r.needs) {
    if (n.kind === "public-key") continue;
    let value = opts.values?.[n.id]?.trim() || n.value;
    if (!value && n.generated && !s.secrets.meta(n.id)) value = randomBytes(24).toString("base64url");
    if (!value) continue;
    let publicValue: string | null = null;
    try {
      if (n.kind === "wireguard-private-key") publicValue = wireGuardPublicKey(value);
      if (n.kind === "tinc-rsa-private") publicValue = createPublicKey(value).export({ type: "pkcs1", format: "pem" }).toString();
    } catch {
      throw new Error(`${n.label}: not a valid ${n.kind}`);
    }
    if (n.kind === "pem-certificate") publicValue = value;
    out.push({ need: n, value, publicValue });
  }
  return out;
}

/** Node keys the import asks for: redacted Tinc keys of nodes that exist or that it creates. */
const requestedKeys = (s: Services, r: ImportResult, opts: ImportOptions) =>
  r.nodeKeys.filter((k) => (s.nodes.get(k.node) || opts.createNodes !== false) && !s.secrets.meta(k.secretId));

export function previewImport(s: Services, exportJson: unknown, opts: ImportOptions): ImportPreview & { disabled: string[] } {
  const r = convertEtcdExport(exportJson);
  const { document, disabled } = finalDocument(s, r, opts);
  // the result of staging: what it stores counts as present, the keys it requests as pending on their nodes
  const assume: AssumedSecrets = {
    present: new Map(importSecrets(s, r, opts).map(({ need, value, publicValue }) => [need.id, { kind: need.kind, value, publicValue, nodeScope: need.node ? [need.node] : ["*"] }])),
    pending: new Set(requestedKeys(s, r, opts).map((k) => k.secretId)),
  };
  return {
    ...r,
    document,
    disabled,
    existingNodes: r.nodes.filter((n) => s.nodes.get(n.id)).map((n) => n.id),
    existingSecrets: r.needs.filter((n) => n.kind !== "public-key" && s.secrets.meta(n.id)).map((n) => n.id),
    // nodes the import would create are not registered yet: that is expected, not a problem of the import
    problems: s.config
      .preview(document, assume)
      .problems.filter((p) => !(opts.createNodes !== false && p.message === "node is not registered in the manager" && r.nodes.some((n) => p.path === `/nodes/${n.id}`))),
  };
}

export function applyImport(s: Services, exportJson: unknown, opts: ImportOptions, actor: string) {
  const r = convertEtcdExport(exportJson);
  const secrets = importSecrets(s, r, opts);
  const created: string[] = [];
  if (opts.createNodes !== false) {
    for (const n of r.nodes) {
      if (s.nodes.get(n.id)) continue;
      s.nodes.createNode({ id: n.id, displayName: n.displayName, behavior: n.behavior }, actor);
      created.push(n.id);
    }
  }
  const stored: string[] = [];
  for (const { need: n, value, publicValue } of secrets) {
    s.secrets.put({ id: n.id, kind: n.kind as SecretKind, value, publicValue, nodeScope: n.node ? [n.node] : ["*"], description: `imported: ${n.label}` });
    stored.push(n.id);
  }
  // Tinc keys the export carries redacted: generated on each node (now, or once its Tinc package is installed)
  const keys: string[] = [];
  for (const k of requestedKeys(s, r, opts)) {
    if (!s.nodes.get(k.node)) continue;
    s.nodeKeys.request(k.node, { secretId: k.secretId, keyKind: k.keyKind, package: k.package }, actor);
    keys.push(k.secretId);
  }
  const { document, disabled } = finalDocument(s, r, opts);
  const mine = s.config.myDraft(actor);
  if (mine) s.config.discardDraft(mine.id, actor);
  const draft = s.config.createDraft(actor, document);
  s.audit.record(actor, "import.etcd", draft.id, { nodes: r.nodes.length, created, stored: stored.length, keys: keys.length, disabled });
  const problems = s.config.preview(document).problems;
  return { draftId: draft.id, created, stored, keys, disabled, notes: r.notes, errors: problems.filter(isError).length, problems };
}
