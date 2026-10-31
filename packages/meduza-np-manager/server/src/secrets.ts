// Encrypted secret store (AGENT.md §6.3, §6.5, D4). AES-256-GCM, random 96-bit nonce per write, the secret id
// and version bound as additional authenticated data so ciphertexts cannot be swapped between records.
// The key comes from SECRET_ENCRYPTION_KEY_FILE and is never stored with the database.

import { createCipheriv, createDecipheriv, randomBytes } from "node:crypto";
import type { Db } from "./db.js";
import { nowIso } from "./db.js";

export interface SecretRecord {
  id: string;
  kind: string;
  description: string;
  nodeScope: string[];
  holder: "manager" | "node";
  holderNode: string | null;
  publicValue: string | null;
  createdAt: string;
  rotatedAt: string | null;
  version: number;
}

/** Secret kinds; `node` holders keep the private part on the node (key generated there, §6.3). */
export const SECRET_KINDS = [
  "wireguard-private-key",
  "wireguard-psk",
  "tinc-ed25519-private",
  "tinc-rsa-private",
  "pem-private-key",
  "openvpn-static-key",
  "openvpn-tls-crypt",
  "pem-certificate",
  "pem-crl",
  "ldap-bind-password",
  "text",
] as const;
export type SecretKind = (typeof SECRET_KINDS)[number];

const ID_RE = /^[a-z0-9][a-z0-9_.-]{0,95}$/;

export class SecretStore {
  constructor(private readonly db: Db, private readonly key: Buffer) {
    if (key.length !== 32) throw new Error("secret encryption key must be 32 bytes");
  }

  private aad(id: string, version: number): Buffer {
    return Buffer.from(`meduza-np-secret:${id}:${version}`);
  }

  private seal(id: string, version: number, plaintext: string): { ciphertext: Buffer; nonce: Buffer } {
    const nonce = randomBytes(12);
    const cipher = createCipheriv("aes-256-gcm", this.key, nonce);
    cipher.setAAD(this.aad(id, version));
    const body = Buffer.concat([cipher.update(plaintext, "utf8"), cipher.final()]);
    return { ciphertext: Buffer.concat([body, cipher.getAuthTag()]), nonce };
  }

  private open(id: string, version: number, ciphertext: Uint8Array, nonce: Uint8Array): string {
    const buf = Buffer.from(ciphertext);
    const decipher = createDecipheriv("aes-256-gcm", this.key, Buffer.from(nonce));
    decipher.setAAD(this.aad(id, version));
    decipher.setAuthTag(buf.subarray(buf.length - 16));
    return Buffer.concat([decipher.update(buf.subarray(0, buf.length - 16)), decipher.final()]).toString("utf8");
  }

  /** Encrypts an arbitrary JSON value bound to `label` (e.g. a node snapshot's secret envelope). */
  sealEnvelope(label: string, value: unknown): { ciphertext: Buffer; nonce: Buffer } {
    const nonce = randomBytes(12);
    const cipher = createCipheriv("aes-256-gcm", this.key, nonce);
    cipher.setAAD(Buffer.from(`meduza-np-envelope:${label}`));
    const body = Buffer.concat([cipher.update(JSON.stringify(value), "utf8"), cipher.final()]);
    return { ciphertext: Buffer.concat([body, cipher.getAuthTag()]), nonce };
  }

  openEnvelope<T>(label: string, ciphertext: Uint8Array, nonce: Uint8Array): T {
    const buf = Buffer.from(ciphertext);
    const decipher = createDecipheriv("aes-256-gcm", this.key, Buffer.from(nonce));
    decipher.setAAD(Buffer.from(`meduza-np-envelope:${label}`));
    decipher.setAuthTag(buf.subarray(buf.length - 16));
    return JSON.parse(Buffer.concat([decipher.update(buf.subarray(0, buf.length - 16)), decipher.final()]).toString("utf8")) as T;
  }

  /** Creates or replaces a manager-held secret. Replacing bumps the version (rotation). */
  put(input: { id: string; kind: SecretKind; value: string; publicValue?: string | null; nodeScope?: string[]; description?: string }): SecretRecord {
    if (!ID_RE.test(input.id)) throw new Error(`invalid secret id ${input.id}`);
    const existing = this.db.get<{ version: number }>("SELECT version FROM secrets WHERE id = ?", input.id);
    const version = existing ? existing.version + 1 : 1;
    const { ciphertext, nonce } = this.seal(input.id, version, input.value);
    const now = nowIso();
    this.db.run(
      `INSERT INTO secrets (id, kind, description, node_scope, holder, holder_node, ciphertext, nonce, public_value, created_at, rotated_at, version)
       VALUES (?, ?, ?, ?, 'manager', NULL, ?, ?, ?, ?, NULL, ?)
       ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, description = excluded.description, node_scope = excluded.node_scope,
         holder = 'manager', holder_node = NULL, ciphertext = excluded.ciphertext, nonce = excluded.nonce,
         public_value = excluded.public_value, rotated_at = ?, version = excluded.version`,
      input.id, input.kind, input.description ?? "", JSON.stringify(input.nodeScope ?? []), ciphertext, nonce,
      input.publicValue ?? null, now, version, now,
    );
    return this.meta(input.id)!;
  }

  /** Records a secret whose private part lives only on `node` (node-side key generation). */
  putNodeHeld(input: { id: string; kind: SecretKind; node: string; publicValue: string; description?: string }): SecretRecord {
    if (!ID_RE.test(input.id)) throw new Error(`invalid secret id ${input.id}`);
    const existing = this.db.get<{ version: number }>("SELECT version FROM secrets WHERE id = ?", input.id);
    const now = nowIso();
    this.db.run(
      `INSERT INTO secrets (id, kind, description, node_scope, holder, holder_node, ciphertext, nonce, public_value, created_at, version)
       VALUES (?, ?, ?, ?, 'node', ?, NULL, NULL, ?, ?, ?)
       ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, holder = 'node', holder_node = excluded.holder_node,
         node_scope = excluded.node_scope, ciphertext = NULL, nonce = NULL, public_value = excluded.public_value,
         rotated_at = ?, version = excluded.version`,
      input.id, input.kind, input.description ?? "", JSON.stringify([input.node]), input.node, input.publicValue, now,
      existing ? existing.version + 1 : 1, now,
    );
    return this.meta(input.id)!;
  }

  /** Records a node-held key the node has not generated yet (D28): referenced like any key, no public value. */
  putPending(input: { id: string; kind: SecretKind; node: string; description?: string }): SecretRecord {
    if (!ID_RE.test(input.id)) throw new Error(`invalid secret id ${input.id}`);
    this.db.run(
      `INSERT INTO secrets (id, kind, description, node_scope, holder, holder_node, ciphertext, nonce, public_value, created_at, version)
       VALUES (?, ?, ?, ?, 'node', ?, NULL, NULL, NULL, ?, 1) ON CONFLICT(id) DO NOTHING`,
      input.id, input.kind, input.description ?? "", JSON.stringify([input.node]), input.node, nowIso(),
    );
    return this.meta(input.id)!;
  }

  meta(id: string): SecretRecord | undefined {
    const r = this.db.get<Record<string, unknown>>(
      "SELECT id, kind, description, node_scope, holder, holder_node, public_value, created_at, rotated_at, version FROM secrets WHERE id = ?",
      id,
    );
    return r ? toRecord(r) : undefined;
  }

  list(): SecretRecord[] {
    return this.db
      .all<Record<string, unknown>>("SELECT id, kind, description, node_scope, holder, holder_node, public_value, created_at, rotated_at, version FROM secrets ORDER BY id")
      .map(toRecord);
  }

  /** Decrypts a manager-held secret. Callers must have authorized the access (export / node delivery). */
  reveal(id: string): string {
    const r = this.db.get<{ ciphertext: Uint8Array | null; nonce: Uint8Array | null; version: number; holder: string }>(
      "SELECT ciphertext, nonce, version, holder FROM secrets WHERE id = ?",
      id,
    );
    if (!r) throw new Error(`secret ${id} not found`);
    if (r.holder !== "manager" || !r.ciphertext || !r.nonce) throw new Error(`secret ${id} is held by a node and cannot be revealed`);
    return this.open(id, r.version, r.ciphertext, r.nonce);
  }

  /** Whether `node` may receive the secret's value in its config envelope. */
  deliverableTo(id: string, node: string): boolean {
    const m = this.meta(id);
    return !!m && m.holder === "manager" && (m.nodeScope.includes(node) || m.nodeScope.includes("*"));
  }

  setScope(id: string, nodes: string[]): void {
    this.db.run("UPDATE secrets SET node_scope = ? WHERE id = ?", JSON.stringify([...new Set(nodes)].sort()), id);
  }

  delete(id: string): boolean {
    return this.db.run("DELETE FROM secrets WHERE id = ?", id).changes > 0;
  }
}

function toRecord(r: Record<string, unknown>): SecretRecord {
  return {
    id: String(r.id),
    kind: String(r.kind),
    description: String(r.description ?? ""),
    nodeScope: JSON.parse(String(r.node_scope ?? "[]")) as string[],
    holder: r.holder === "node" ? "node" : "manager",
    holderNode: (r.holder_node as string | null) ?? null,
    publicValue: (r.public_value as string | null) ?? null,
    createdAt: String(r.created_at),
    rotatedAt: (r.rotated_at as string | null) ?? null,
    version: Number(r.version),
  };
}
