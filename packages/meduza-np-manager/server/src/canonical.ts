// Canonical JSON + SHA-256, byte-identical to crates/protocol/src/digest.rs: object keys sorted by UTF-16
// code units, no whitespace, JSON.stringify for scalars. Configuration documents only contain integers.

import { createHash } from "node:crypto";

export function canonicalJson(value: unknown): string {
  if (value === null || typeof value !== "object") {
    if (typeof value === "number" && !Number.isFinite(value)) throw new TypeError("non-finite number");
    if (value === undefined) throw new TypeError("undefined is not JSON");
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) return `[${value.map((v) => canonicalJson(v === undefined ? null : v)).join(",")}]`;
  const obj = value as Record<string, unknown>;
  const keys = Object.keys(obj).filter((k) => obj[k] !== undefined).sort();
  return `{${keys.map((k) => `${JSON.stringify(k)}:${canonicalJson(obj[k])}`).join(",")}}`;
}

export function sha256Hex(data: string | Buffer): string {
  return createHash("sha256").update(data).digest("hex");
}

export function canonicalDigest(value: unknown): string {
  return sha256Hex(canonicalJson(value));
}
