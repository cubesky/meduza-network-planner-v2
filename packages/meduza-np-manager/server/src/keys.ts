// Real key material generated on the manager (AGENT.md §6.3). Node-side generation is the default; these are
// used when an operator explicitly chooses manager generation/import.

import { createPrivateKey, createPublicKey, generateKeyPairSync, randomBytes } from "node:crypto";

const X25519_PKCS8_PREFIX = Buffer.from("302e020100300506032b656e04220420", "hex");

export interface WireGuardKeyPair {
  privateKey: string;
  publicKey: string;
}

function b64urlToB64(s: string): string {
  return Buffer.from(s, "base64url").toString("base64");
}

/** X25519 key pair in WireGuard's base64 encoding (32 raw bytes each). */
export function generateWireGuardKeyPair(): WireGuardKeyPair {
  const { privateKey, publicKey } = generateKeyPairSync("x25519");
  const priv = privateKey.export({ format: "jwk" });
  const pub = publicKey.export({ format: "jwk" });
  return { privateKey: b64urlToB64(priv.d!), publicKey: b64urlToB64(pub.x!) };
}

/** Derives the public key from a base64 WireGuard private key (clamping is applied by X25519 itself). */
export function wireGuardPublicKey(privateKeyB64: string): string {
  const raw = parseWireGuardKey(privateKeyB64);
  const key = createPrivateKey({ key: Buffer.concat([X25519_PKCS8_PREFIX, raw]), format: "der", type: "pkcs8" });
  return b64urlToB64(createPublicKey(key).export({ format: "jwk" }).x!);
}

export function generateWireGuardPsk(): string {
  return randomBytes(32).toString("base64");
}

/** Validates WireGuard key text: base64 of exactly 32 bytes. Throws with a user-facing message. */
export function parseWireGuardKey(text: string): Buffer {
  const trimmed = text.trim();
  if (!/^[A-Za-z0-9+/]{43}=$/.test(trimmed)) throw new Error("WireGuard keys are 44 characters of base64 ending in '='");
  return Buffer.from(trimmed, "base64");
}
