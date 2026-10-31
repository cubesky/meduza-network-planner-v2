// Structural JSON diff for the publish preview (AGENT.md §6.2 "脱敏差异"). Values are redacted before display.

import { redact } from "./audit.js";

export interface DiffEntry {
  path: string;
  op: "add" | "remove" | "change";
  before?: unknown;
  after?: unknown;
}

function isObj(v: unknown): v is Record<string, unknown> {
  return !!v && typeof v === "object" && !Array.isArray(v);
}

function idOf(v: unknown): string | null {
  return isObj(v) && typeof v.id === "string" ? v.id : null;
}

export function diffJson(before: unknown, after: unknown, path = "", out: DiffEntry[] = []): DiffEntry[] {
  if (isObj(before) && isObj(after)) {
    const keys = new Set([...Object.keys(before), ...Object.keys(after)]);
    for (const k of [...keys].sort()) {
      const p = `${path}/${k}`;
      if (!(k in after)) out.push({ path: p, op: "remove", before: before[k] });
      else if (!(k in before)) out.push({ path: p, op: "add", after: after[k] });
      else diffJson(before[k], after[k], p, out);
    }
    return out;
  }
  if (Array.isArray(before) && Array.isArray(after)) {
    // Arrays of objects with stable ids are matched by id so reordering is not a change of every element.
    if (before.every((v) => idOf(v) !== null) && after.every((v) => idOf(v) !== null)) {
      const b = new Map(before.map((v) => [idOf(v)!, v]));
      const a = new Map(after.map((v) => [idOf(v)!, v]));
      for (const [id, v] of b) if (!a.has(id)) out.push({ path: `${path}[id=${id}]`, op: "remove", before: v });
      for (const [id, v] of a) {
        if (!b.has(id)) out.push({ path: `${path}[id=${id}]`, op: "add", after: v });
        else diffJson(b.get(id), v, `${path}[id=${id}]`, out);
      }
      return out;
    }
    if (JSON.stringify(before) !== JSON.stringify(after)) out.push({ path, op: "change", before, after });
    return out;
  }
  if (JSON.stringify(before) !== JSON.stringify(after)) out.push({ path: path || "/", op: "change", before, after });
  return out;
}

export function redactedDiff(before: unknown, after: unknown): DiffEntry[] {
  return diffJson(redact(before), redact(after));
}
