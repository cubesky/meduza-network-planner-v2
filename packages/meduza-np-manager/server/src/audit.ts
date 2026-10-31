// Audit log with redaction (AGENT.md §6.3, §6.5): secret values never reach audit details or diffs.

import { randomBytes } from "node:crypto";
import type { Db } from "./db.js";
import { nowIso } from "./db.js";

const SENSITIVE_KEY = /(password|passphrase|secret|privatekey|presharedkey|^psk$|token|bindcredential|^value$)/i;

/** Deep-copies `value` replacing sensitive fields. Fields named `*Ref`/`*Refs` are references and stay visible. */
export function redact(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(redact);
  if (value && typeof value === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(value as Record<string, unknown>)) {
      if (/Refs?$/.test(k)) out[k] = v;
      else if (SENSITIVE_KEY.test(k) && v !== null && v !== undefined && v !== "") out[k] = "<redacted>";
      else out[k] = redact(v);
    }
    return out;
  }
  if (typeof value === "string" && /-----BEGIN [A-Z ]*PRIVATE KEY-----/.test(value)) return "<redacted>";
  return value;
}

export class Audit {
  constructor(private readonly db: Db, private readonly maxRows = 200_000) {}

  record(actor: string, action: string, target: string | null, detail: unknown = {}): void {
    this.db.run("INSERT INTO audit (at, actor, action, target, detail) VALUES (?, ?, ?, ?, ?)", nowIso(), actor, action, target, JSON.stringify(redact(detail)));
    // Bounded history: trim occasionally rather than on every insert.
    if (Math.random() < 0.01) {
      this.db.run("DELETE FROM audit WHERE id <= (SELECT MAX(id) FROM audit) - ?", this.maxRows);
    }
  }

  list(limit = 200, before?: number): Array<{ id: number; at: string; actor: string; action: string; target: string | null; detail: unknown }> {
    const rows = before
      ? this.db.all<Record<string, unknown>>("SELECT * FROM audit WHERE id < ? ORDER BY id DESC LIMIT ?", before, limit)
      : this.db.all<Record<string, unknown>>("SELECT * FROM audit ORDER BY id DESC LIMIT ?", limit);
    return rows.map((r) => ({
      id: Number(r.id),
      at: String(r.at),
      actor: String(r.actor),
      action: String(r.action),
      target: (r.target as string | null) ?? null,
      detail: JSON.parse(String(r.detail)),
    }));
  }
}

/** URL-safe random identifier with the given prefix. */
export function randomId(prefix: string, bytes = 12): string {
  return `${prefix}-${randomBytes(bytes).toString("base64url")}`;
}

/** Random secret token (enrollment, invites, sessions). */
export function randomToken(bytes = 32): string {
  return randomBytes(bytes).toString("base64url");
}
