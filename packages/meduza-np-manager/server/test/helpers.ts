// Test helpers: isolated manager instance, software WebAuthn authenticator, node identity.

import { createHash, generateKeyPairSync, randomBytes, sign, type KeyObject } from "node:crypto";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { loadConfig } from "../src/config.js";
import { Db } from "../src/db.js";
import { buildApp, createServices, type Services } from "../src/app.js";
import type { FastifyInstance } from "fastify";

export const REPO_ROOT = fileURLToPath(new URL("../../../../../", import.meta.url));
export const ORIGIN = "https://manager.example.test";
export const BOOTSTRAP_TOKEN = "bootstrap-token-0123456789abcdef";

export async function startManager(extraEnv: Record<string, string> = {}): Promise<{ app: FastifyInstance; s: Services; dir: string }> {
  const dir = mkdtempSync(join(tmpdir(), "mnp-manager-"));
  writeFileSync(join(dir, "session.key"), randomBytes(32).toString("hex"));
  writeFileSync(join(dir, "secret.key"), randomBytes(32).toString("hex"));
  writeFileSync(join(dir, "bootstrap"), BOOTSTRAP_TOKEN);
  const cfg = loadConfig({
    PUBLIC_ORIGIN: ORIGIN,
    RP_ID: "manager.example.test",
    DATA_DIR: join(dir, "data"),
    SESSION_SECRET_FILE: join(dir, "session.key"),
    SECRET_ENCRYPTION_KEY_FILE: join(dir, "secret.key"),
    BOOTSTRAP_TOKEN_FILE: join(dir, "bootstrap"),
    PACKAGES_DIR: join(REPO_ROOT, "packages"),
    WEB_DIR: join(dir, "no-web"),
    PER_NODE_REQUESTS_PER_MINUTE: "600",
    ...extraEnv,
  });
  const db = Db.memory();
  db.migrate();
  let ready = true;
  const s: Services = { ...createServices(cfg, db, join(REPO_ROOT, "schemas")), ready: () => ready, shuttingDown: () => false };
  const app = await buildApp(s);
  return { app, s, dir };
}

// ---- minimal CBOR encoder (enough for COSE keys and attestation objects) ---------------------------------

function cborHead(major: number, n: number): Buffer {
  if (n < 24) return Buffer.from([(major << 5) | n]);
  if (n < 256) return Buffer.from([(major << 5) | 24, n]);
  if (n < 65536) return Buffer.from([(major << 5) | 25, n >> 8, n & 255]);
  const b = Buffer.alloc(5);
  b[0] = (major << 5) | 26;
  b.writeUInt32BE(n, 1);
  return b;
}

export function cbor(value: unknown): Buffer {
  if (typeof value === "number") return value >= 0 ? cborHead(0, value) : cborHead(1, -1 - value);
  if (typeof value === "string") {
    const b = Buffer.from(value, "utf8");
    return Buffer.concat([cborHead(3, b.length), b]);
  }
  if (Buffer.isBuffer(value) || value instanceof Uint8Array) return Buffer.concat([cborHead(2, value.length), Buffer.from(value)]);
  if (value instanceof Map) {
    const parts = [cborHead(5, value.size)];
    for (const [k, v] of value) parts.push(cbor(k), cbor(v));
    return Buffer.concat(parts);
  }
  throw new Error("unsupported cbor value");
}

/** A platform-like authenticator holding one P-256 credential, always doing user verification. */
export class SoftAuthenticator {
  readonly credentialId = randomBytes(16);
  private readonly key: { privateKey: KeyObject; publicKey: KeyObject };
  private counter = 0;

  constructor(private readonly rpId = "manager.example.test", private readonly origin = ORIGIN) {
    this.key = generateKeyPairSync("ec", { namedCurve: "P-256" });
  }

  private rpIdHash(): Buffer {
    return createHash("sha256").update(this.rpId).digest();
  }

  private coseKey(): Buffer {
    const jwk = this.key.publicKey.export({ format: "jwk" });
    return cbor(new Map<number, unknown>([
      [1, 2], // kty EC2
      [3, -7], // ES256
      [-1, 1], // P-256
      [-2, Buffer.from(jwk.x!, "base64url")],
      [-3, Buffer.from(jwk.y!, "base64url")],
    ]));
  }

  register(options: { challenge: string }, overrides: { origin?: string; uv?: boolean } = {}) {
    const clientData = Buffer.from(JSON.stringify({ type: "webauthn.create", challenge: options.challenge, origin: overrides.origin ?? this.origin, crossOrigin: false }));
    const flags = 0x01 | (overrides.uv === false ? 0 : 0x04) | 0x40; // UP | UV | AT
    const counter = Buffer.alloc(4);
    const credLen = Buffer.alloc(2);
    credLen.writeUInt16BE(this.credentialId.length);
    const authData = Buffer.concat([this.rpIdHash(), Buffer.from([flags]), counter, Buffer.alloc(16), credLen, this.credentialId, this.coseKey()]);
    const attestationObject = cbor(new Map<string, unknown>([["fmt", "none"], ["attStmt", new Map()], ["authData", authData]]));
    const id = this.credentialId.toString("base64url");
    return {
      id,
      rawId: id,
      type: "public-key",
      response: { clientDataJSON: clientData.toString("base64url"), attestationObject: attestationObject.toString("base64url"), transports: ["internal"] },
      clientExtensionResults: {},
      authenticatorAttachment: "platform",
    };
  }

  authenticate(options: { challenge: string }, overrides: { origin?: string; uv?: boolean } = {}) {
    this.counter++;
    const clientData = Buffer.from(JSON.stringify({ type: "webauthn.get", challenge: options.challenge, origin: overrides.origin ?? this.origin, crossOrigin: false }));
    const counter = Buffer.alloc(4);
    counter.writeUInt32BE(this.counter);
    const authData = Buffer.concat([this.rpIdHash(), Buffer.from([0x01 | (overrides.uv === false ? 0 : 0x04)]), counter]);
    const signature = sign("sha256", Buffer.concat([authData, createHash("sha256").update(clientData).digest()]), this.key.privateKey);
    const id = this.credentialId.toString("base64url");
    return {
      id,
      rawId: id,
      type: "public-key",
      response: { clientDataJSON: clientData.toString("base64url"), authenticatorData: authData.toString("base64url"), signature: signature.toString("base64url") },
      clientExtensionResults: {},
      authenticatorAttachment: "platform",
    };
  }
}

/** Node identity as the planner implements it (Ed25519, D3). */
export class NodeIdentity {
  private readonly key = generateKeyPairSync("ed25519");
  publicKeyB64(): string {
    const der = this.key.publicKey.export({ format: "der", type: "spki" });
    return der.subarray(der.length - 32).toString("base64");
  }
  sign(data: Buffer): string {
    return sign(null, data, this.key.privateKey).toString("base64");
  }
}

export function cookieFrom(res: { headers: Record<string, unknown> }): string {
  const raw = res.headers["set-cookie"];
  const first = Array.isArray(raw) ? raw[0] : String(raw);
  return String(first).split(";")[0]!;
}

/** Bootstraps the first admin with a software passkey; returns headers for authenticated API writes. */
export async function adminHeaders(app: FastifyInstance): Promise<Record<string, string>> {
  const base = { origin: ORIGIN, "content-type": "application/json" };
  const authn = new SoftAuthenticator();
  const opt = await app.inject({ method: "POST", url: "/api/v1/auth/passkeys/register/options", headers: base, payload: { bootstrapToken: BOOTSTRAP_TOKEN, username: "alice" } });
  const { challengeId, options } = opt.json();
  const ver = await app.inject({ method: "POST", url: "/api/v1/auth/passkeys/register/verify", headers: base, payload: { challengeId, response: authn.register(options), name: "laptop" } });
  const cookie = cookieFrom(ver);
  const csrf = (await app.inject({ method: "GET", url: "/api/v1/auth/session", headers: { cookie } })).json().csrfToken as string;
  return { ...base, cookie, "x-csrf-token": csrf };
}
