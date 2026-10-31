// Cluster configuration in three layers, like LuCI (D23):
//   published — the current revision;
//   staged    — the account's saved but not applied changes (a server-side draft: survives reloads, other tabs);
//   doc       — the working copy pages edit (unsaved form edits on top of the staged state).
// "Save" stages the working copy, "Reset" drops unsaved edits, "Apply" publishes the staged changes as a new
// revision (rebasing once onto a newer revision someone else applied), "Revert" discards them.
// Revisions applied elsewhere (other accounts, the manager distributing a node key) show up without a page reload:
// the store checks the current revision periodically, and staged changes based on an older revision are rebased onto
// the newer one when loading, so the forms never show settings older than what is applied.

import { useEffect, useState } from "preact/hooks";
import { ApiError, del, get, post, put } from "./api.ts";

export interface ClusterDoc {
  schemaVersion: number;
  routing?: { internalAsn?: number; anycast?: unknown; bfd?: Record<string, unknown>; maximumPaths?: number; bgpFilter?: { in?: string[]; out?: string[] } };
  mihomo?: MihomoCatalog;
  /** cluster-wide DNS hosts, Tinc mesh defaults and MosDNS configuration (D58) */
  dns?: { hosts?: Array<{ hostname: string; ips: string[] }> };
  tinc?: { cipher?: string; digest?: string };
  mosdns?: Record<string, unknown>;
  defaults?: Record<string, any>;
  nodes: Record<string, Record<string, any>>;
}

export interface MihomoSubscription {
  id: string;
  name?: string;
  urlRef: string;
  enabled?: boolean;
}

/** Cluster-wide Mihomo subscriptions (D29). */
export interface MihomoCatalog {
  subscriptions?: MihomoSubscription[];
  default?: string;
  refreshSeconds?: number;
}

export interface Problem {
  path: string;
  message: string;
  node?: string;
  severity?: "warning";
}

export interface Preview {
  problems: Problem[];
  nodes: Array<{ nodeId: string; changed: boolean; currentRevision: string | null; diff: Array<{ path: string; op: string; before?: unknown; after?: unknown }> }>;
}

interface State {
  loaded: boolean;
  loading: boolean;
  error: string | null;
  revision: string | null;
  published: ClusterDoc;
  draftId: string | null;
  draftBase: string | null;
  staged: ClusterDoc;
  doc: ClusterDoc;
  /** validation of the working copy (field problems while editing) */
  preview: Preview | null;
  /** sections both this account and someone else changed, after a rebase */
  conflicts: string[];
  /** a newer revision applied elsewhere while this page has unsaved edits (loaded on the next save) */
  newer: string | null;
}

const empty = (): ClusterDoc => ({ schemaVersion: 1, nodes: {} });

/**
 * Brings services written in earlier shapes to the current one, so forms edit what the manager compiles: Tinc listed
 * networks (`instances`) before it became a single mesh (D22). Applied to published and staged alike, so the
 * conversion alone never shows up as a change.
 */
function normalizeDoc(doc: ClusterDoc): ClusterDoc {
  const out = structuredClone(doc);
  for (const spec of Object.values(out.nodes ?? {})) {
    const tinc = spec?.services?.tinc;
    if (tinc && Array.isArray(tinc.instances)) {
      const { id: _id, netname: _netname, ...mesh } = (tinc.instances[0] ?? {}) as Record<string, unknown>;
      spec.services.tinc = { ...mesh, enabled: tinc.enabled !== false && mesh.enabled !== false };
    }
  }
  return out;
}
let state: State = { loaded: false, loading: false, error: null, revision: null, published: empty(), draftId: null, draftBase: null, staged: empty(), doc: empty(), preview: null, conflicts: [], newer: null };
const listeners = new Set<(s: State) => void>();
const set = (patch: Partial<State>) => {
  state = { ...state, ...patch };
  for (const f of listeners) f(state);
};
const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);

export const isError = (p: Problem) => p.severity !== "warning";
/** Unsaved form edits on top of the staged state. */
export const unsaved = (s: State = state) => !same(s.doc, s.staged);
/** Saved but not applied changes. */
export const stagedChanges = (s: State = state) => !same(s.staged, s.published);

/** Sections (nodes, routing, defaults) that differ between two documents: what the changes counter counts. */
export function changedSections(a: ClusterDoc, b: ClusterDoc): string[] {
  const out: string[] = [];
  for (const k of [...new Set([...Object.keys(a.nodes ?? {}), ...Object.keys(b.nodes ?? {})])].sort()) if (!same(a.nodes?.[k], b.nodes?.[k])) out.push(`node:${k}`);
  // every other top-level section counts as one (routing, defaults, mihomo, dns, tinc, mosdns…)
  const keys = [...new Set([...Object.keys(a), ...Object.keys(b)])].filter((k) => k !== "nodes" && k !== "schemaVersion").sort();
  for (const k of keys) if (!same((a as unknown as Record<string, unknown>)[k], (b as unknown as Record<string, unknown>)[k])) out.push(k);
  return out;
}

type Draft = { id: string; baseRevision: string | null; document: ClusterDoc } | null;

/** (Re)loads published and staged state; drops unsaved form edits. Staged changes on an older base are rebased. */
export async function reload(): Promise<void> {
  set({ loading: true, error: null });
  try {
    const [c, first] = await Promise.all([get<{ revision: string | null; document: ClusterDoc }>("/api/v1/config"), get<Draft>("/api/v1/drafts/mine")]);
    let mine = first;
    let conflicts: string[] = [];
    if (mine && mine.baseRevision !== c.revision) {
      // Someone applied a newer revision: keep this account's changes, take theirs (conflicts listed for review).
      try {
        conflicts = (await post<{ conflicts: string[] }>(`/api/v1/drafts/${mine.id}/rebase`, {})).conflicts;
        mine = await get<Draft>("/api/v1/drafts/mine");
      } catch {
        // viewers cannot rebase; they see their staged state as it is
      }
    }
    const published = normalizeDoc(c.document ?? empty());
    let staged = mine?.document ? normalizeDoc(mine.document) : published;
    if (mine && same(staged, published)) {
      // nothing of this account's changes is left to apply
      await del(`/api/v1/drafts/${mine.id}`).catch(() => undefined);
      mine = null;
      staged = published;
    }
    set({ loaded: true, loading: false, revision: c.revision, published, draftId: mine?.id ?? null, draftBase: mine?.baseRevision ?? null, staged, doc: structuredClone(staged), preview: null, conflicts, newer: null });
  } catch (e) {
    set({ loading: false, error: e instanceof Error ? e.message : String(e) });
  }
}

/** Picks up revisions applied elsewhere: reloads at once, or after the next save when the page has unsaved edits. */
async function checkNewer(): Promise<void> {
  if (!state.loaded || state.loading || typeof document === "undefined" || document.hidden) return;
  try {
    const c = await get<{ revision: string | null }>("/api/v1/config");
    if (c.revision === state.revision) return;
    if (unsaved()) set({ newer: c.revision });
    else await reload();
  } catch {
    // next round
  }
}
let watching = false;
function watch(): void {
  if (watching || typeof window === "undefined") return;
  watching = true;
  setInterval(() => void checkNewer(), 15_000);
  document.addEventListener("visibilitychange", () => void checkNewer());
}

let timer: ReturnType<typeof setTimeout> | undefined;
/** Edits the working copy; validation (server-side, authoritative) follows shortly after. */
export function update(fn: (doc: ClusterDoc) => ClusterDoc): void {
  set({ doc: fn(state.doc) });
  clearTimeout(timer);
  timer = setTimeout(() => void validate(), 500);
}

export async function validate(doc: ClusterDoc = state.doc): Promise<Preview | null> {
  try {
    const p = await post<Preview>("/api/v1/validate", { document: doc });
    if (doc === state.doc) set({ preview: p });
    return p;
  } catch {
    return null;
  }
}

/** Drops unsaved form edits (back to the staged state). */
export function reset(): void {
  set({ doc: structuredClone(state.staged), preview: null });
}

async function stage(doc: ClusterDoc): Promise<void> {
  if (same(doc, state.published)) {
    // nothing left to apply: no draft
    if (state.draftId) await del(`/api/v1/drafts/${state.draftId}`);
    set({ draftId: null, draftBase: null, staged: structuredClone(doc), conflicts: [] });
    return;
  }
  if (state.draftId) {
    await put(`/api/v1/drafts/${state.draftId}`, { document: doc });
    set({ staged: structuredClone(doc) });
  } else {
    const d = await post<{ id: string }>("/api/v1/drafts", { document: doc });
    set({ draftId: d.id, draftBase: state.revision, staged: structuredClone(doc) });
  }
}

/** LuCI "Save": stages the working copy (and merges it onto a revision applied meanwhile). */
export async function save(): Promise<void> {
  await stage(state.doc);
  if (state.newer) await reload();
}

/** Applies the staged changes. A newer revision applied meanwhile is merged in first; conflicts stop for review. */
export async function apply(message: string): Promise<{ revision: string; nodes: Array<{ nodeId: string; changed: boolean }> } | null> {
  if (!state.draftId) return null;
  const publish = () => post<{ revision: string; nodes: Array<{ nodeId: string; changed: boolean }> }>(`/api/v1/drafts/${state.draftId}/publish`, { message });
  try {
    const r = await publish();
    await reload();
    return r;
  } catch (e) {
    if (!(e instanceof ApiError && e.status === 409)) throw e;
    const rb = await post<{ conflicts: string[] }>(`/api/v1/drafts/${state.draftId}/rebase`, {});
    const keepEdits = unsaved() ? state.doc : null;
    await reload();
    if (rb.conflicts.length) {
      set({ conflicts: rb.conflicts });
      return null;
    }
    const r = await publish();
    await reload();
    if (keepEdits) set({ doc: keepEdits });
    return r;
  }
}

/** LuCI "Revert": discards every staged change (and unsaved edits). */
export async function revert(): Promise<void> {
  if (state.draftId) await del(`/api/v1/drafts/${state.draftId}`);
  set({ draftId: null, draftBase: null, staged: structuredClone(state.published), doc: structuredClone(state.published), preview: null, conflicts: [] });
}

/** Reverts one section (`node:<id>`, `routing`, `defaults`) of the staged changes. */
export async function revertSection(section: string): Promise<void> {
  const back = (d: ClusterDoc): ClusterDoc => {
    const out: ClusterDoc = structuredClone(d);
    if (section.startsWith("node:")) {
      const id = section.slice(5);
      if (state.published.nodes[id] === undefined) delete out.nodes[id];
      else out.nodes[id] = structuredClone(state.published.nodes[id]!);
    } else {
      const k = section as keyof ClusterDoc;
      if (state.published[k] === undefined) delete out[k];
      else (out as any)[k] = structuredClone(state.published[k]);
    }
    return out;
  };
  await stage(back(state.staged));
  set({ doc: back(state.doc) });
}

/** Subscribes the component to the configuration; loads it on first use and retries after a failed load. */
export function useConfig(): State {
  const [s, setS] = useState(state);
  useEffect(() => {
    listeners.add(setS);
    if (!state.loaded && !state.loading) void reload();
    watch();
    return () => void listeners.delete(setS);
  }, []);
  return s;
}

/** A node's spec in the working copy (a fresh default for nodes not configured yet). */
export function nodeSpec(doc: ClusterDoc, id: string): Record<string, any> {
  return doc.nodes[id] ?? { behavior: "static", networks: { lan: [], privateLan: [] }, services: {} };
}

export function setNodeSpec(id: string, spec: Record<string, any>): void {
  update((d) => ({ ...d, nodes: { ...d.nodes, [id]: spec } }));
}

/** Problems that belong to one node (or, with `null`, to the cluster-wide settings). */
export function problemsFor(preview: Preview | null, node: string | null): Problem[] {
  return (preview?.problems ?? []).filter((p) => (node === null ? !p.node && !p.path.startsWith("/nodes/") : p.node === node || p.path.startsWith(`/nodes/${node}/`)));
}
