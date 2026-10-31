#!/usr/bin/env node
// Lab driver for the manager HTTP API (no dependencies). Signs in with a software passkey kept in STATE (P-256,
// user verification always set) — exactly the WebAuthn ceremony a browser performs — then calls the operator API.
// Test/lab use only: the key file is a real admin credential for that manager.
//
//   MANAGER=http://10.50.0.10:8787 ORIGIN=http://10.50.0.10:8787 STATE=./lab-admin.json node manager-api.mjs <cmd> [...]
//   bootstrap <token-file> <username>     register the first admin with the bootstrap token
//   invite <invite-url|token>             register a fresh soft key from an invite (e.g. a host recovery invite
//                                         after the manager origin changed; the old key no longer matches the RP ID)
//   invite-account <username> <role>      create an account and print its invitation link
//   node <id> [static|roaming]            create a node
//   token <id>                            print a one-time enrollment token for the node
//   wgkey <secret-id> <node-id>           generate a manager-held WireGuard private key scoped to the node
//   nodekey <node> <secret-id> <kind> <package>   generate a node-held key on the node (job), wait for its public key
//   pubkey <secret-id>                    print a secret's public value
//   publish <document.json> [message]     create a draft with the document, validate it and publish it
//   status <id>                           node detail (sync pointers, last report)
//   wait-applied <id> <revision> [secs]   wait until the node ACKed the revision as applied
//   call <METHOD> <path> [json|@file]       any operator API call (re-verifies when the server asks for it)

import { createHash, createPrivateKey, createPublicKey, generateKeyPairSync, randomBytes, sign } from "node:crypto";
import { existsSync, readFileSync, writeFileSync } from "node:fs";

const MANAGER = process.env.MANAGER ?? "http://127.0.0.1:8787";
const ORIGIN = process.env.ORIGIN ?? MANAGER;
const RP_ID = new URL(ORIGIN).hostname;
const STATE = process.env.STATE ?? "lab-admin.json";

// ---- tiny CBOR encoder (enough for a "none" attestation) ----------------------------------------------------
function head(major, n) {
  if (n < 24) return Buffer.from([(major << 5) | n]);
  if (n < 256) return Buffer.from([(major << 5) | 24, n]);
  const b = Buffer.alloc(3);
  b[0] = (major << 5) | 25;
  b.writeUInt16BE(n, 1);
  return b;
}
function cbor(v) {
  if (typeof v === "number") return v >= 0 ? head(0, v) : head(1, -1 - v);
  if (typeof v === "string") return Buffer.concat([head(3, Buffer.byteLength(v)), Buffer.from(v)]);
  if (Buffer.isBuffer(v)) return Buffer.concat([head(2, v.length), v]);
  if (v instanceof Map) return Buffer.concat([head(5, v.size), ...[...v].flatMap(([k, x]) => [cbor(k), cbor(x)])]);
  throw new Error("cbor: unsupported");
}

// ---- persistent state: passkey + cookie + csrf --------------------------------------------------------------
const state = existsSync(STATE) ? JSON.parse(readFileSync(STATE, "utf8")) : {};
const save = () => writeFileSync(STATE, JSON.stringify(state, null, 2), { mode: 0o600 });
function key() {
  if (!state.jwk) {
    const { privateKey } = generateKeyPairSync("ec", { namedCurve: "P-256" });
    state.jwk = privateKey.export({ format: "jwk" });
    state.credentialId = randomBytes(16).toString("base64url");
    state.counter = 0;
    save();
  }
  return createPrivateKey({ key: state.jwk, format: "jwk" });
}
const rpIdHash = () => createHash("sha256").update(RP_ID).digest();

function registration(challenge) {
  const pub = createPublicKey(key()).export({ format: "jwk" });
  const cose = cbor(new Map([[1, 2], [3, -7], [-1, 1], [-2, Buffer.from(pub.x, "base64url")], [-3, Buffer.from(pub.y, "base64url")]]));
  const cred = Buffer.from(state.credentialId, "base64url");
  const len = Buffer.alloc(2);
  len.writeUInt16BE(cred.length);
  const authData = Buffer.concat([rpIdHash(), Buffer.from([0x45]), Buffer.alloc(4), Buffer.alloc(16), len, cred, cose]);
  const clientData = Buffer.from(JSON.stringify({ type: "webauthn.create", challenge, origin: ORIGIN, crossOrigin: false }));
  const att = cbor(new Map([["fmt", "none"], ["attStmt", new Map()], ["authData", authData]]));
  return {
    id: state.credentialId, rawId: state.credentialId, type: "public-key", clientExtensionResults: {}, authenticatorAttachment: "platform",
    response: { clientDataJSON: clientData.toString("base64url"), attestationObject: att.toString("base64url"), transports: ["internal"] },
  };
}

function assertion(challenge) {
  state.counter = (state.counter ?? 0) + 1;
  save();
  const counter = Buffer.alloc(4);
  counter.writeUInt32BE(state.counter);
  const authData = Buffer.concat([rpIdHash(), Buffer.from([0x05]), counter]);
  const clientData = Buffer.from(JSON.stringify({ type: "webauthn.get", challenge, origin: ORIGIN, crossOrigin: false }));
  const sig = sign("sha256", Buffer.concat([authData, createHash("sha256").update(clientData).digest()]), key());
  return {
    id: state.credentialId, rawId: state.credentialId, type: "public-key", clientExtensionResults: {}, authenticatorAttachment: "platform",
    response: { clientDataJSON: clientData.toString("base64url"), authenticatorData: authData.toString("base64url"), signature: sig.toString("base64url") },
  };
}

// ---- HTTP ---------------------------------------------------------------------------------------------------
async function http(method, path, body) {
  const headers = { origin: ORIGIN };
  if (state.cookie) headers.cookie = state.cookie;
  if (method !== "GET") {
    headers["content-type"] = "application/json";
    if (state.csrf) headers["x-csrf-token"] = state.csrf;
  }
  const res = await fetch(`${MANAGER}${path}`, { method, headers, body: method === "GET" ? undefined : JSON.stringify(body ?? {}) });
  const set = res.headers.getSetCookie?.() ?? [];
  for (const c of set) {
    const kv = c.split(";")[0];
    if (/mnp_session=/.test(kv)) {
      state.cookie = kv;
      save();
    }
  }
  const text = await res.text();
  const data = text ? JSON.parse(text) : undefined;
  if (!res.ok) {
    const err = new Error(`${method} ${path}: ${res.status} ${data?.error ?? ""} ${data?.message ?? text}`);
    err.code = data?.error;
    err.body = data;
    throw err;
  }
  return data;
}

async function refreshSession() {
  const s = await http("GET", "/api/v1/auth/session");
  state.csrf = s.csrfToken ?? null;
  save();
  return s;
}

async function login() {
  const { challengeId, options } = await http("POST", "/api/v1/auth/passkeys/authenticate/options");
  await http("POST", "/api/v1/auth/passkeys/authenticate/verify", { challengeId, response: assertion(options.challenge) });
  await refreshSession();
}

/** Calls the API; signs in when the session is gone and re-verifies when the server asks for it. */
async function api(method, path, body) {
  for (let attempt = 0; attempt < 3; attempt++) {
    try {
      return await http(method, path, body);
    } catch (e) {
      if (e.code === "unauthorized" || e.code === "no-session" || /401/.test(e.message)) {
        await login();
      } else if (e.code === "reauth-required") {
        const { challengeId, options } = await http("POST", "/api/v1/auth/reauth/options");
        await http("POST", "/api/v1/auth/reauth/verify", { challengeId, response: assertion(options.challenge) });
      } else if (e.code === "bad-csrf") {
        await refreshSession();
      } else {
        throw e;
      }
    }
  }
  throw new Error(`${method} ${path}: gave up`);
}

const [cmd, ...args] = process.argv.slice(2);
const out = (v) => console.log(typeof v === "string" ? v : JSON.stringify(v, null, 2));
switch (cmd) {
  case "bootstrap": {
    const token = readFileSync(args[0], "utf8").trim();
    const { challengeId, options } = await http("POST", "/api/v1/auth/passkeys/register/options", { bootstrapToken: token, username: args[1], displayName: args[1] });
    out(await http("POST", "/api/v1/auth/passkeys/register/verify", { challengeId, response: registration(options.challenge), name: "lab soft key" }));
    out(await refreshSession());
    break;
  }
  case "invite": {
    const token = args[0].split("/invite/").pop();
    for (const k of ["jwk", "credentialId", "counter", "cookie", "csrf"]) delete state[k];
    const { challengeId, options } = await http("POST", "/api/v1/auth/passkeys/register/options", { inviteToken: token });
    out(await http("POST", "/api/v1/auth/passkeys/register/verify", { challengeId, response: registration(options.challenge), name: "lab soft key" }));
    out(await refreshSession());
    break;
  }
  case "invite-account": {
    const r = await api("POST", "/api/v1/accounts", { username: args[0], role: args[1] ?? "admin" });
    out(`${r.inviteUrl} (expires ${r.expiresAt})`);
    break;
  }
  case "node":
    out(await api("POST", "/api/v1/nodes", { id: args[0], behavior: args[1] ?? "static" }));
    break;
  case "token": {
    const r = await api("POST", `/api/v1/nodes/${args[0]}/enrollment-tokens`, { ttlSeconds: 3600 });
    out(r.token);
    break;
  }
  case "wgkey":
    out(await api("POST", "/api/v1/secrets/generate", { id: args[0], kind: "wireguard-private-key", nodeScope: [args[1]], replace: true }));
    break;
  case "nodekey": {
    const [node, secretId, keyKind, pkg] = args;
    await api("POST", `/api/v1/nodes/${node}/jobs`, { kind: "generate-keypair", package: pkg, params: { secretId, keyKind, package: pkg } });
    const deadline = Date.now() + 120_000;
    for (;;) {
      const list = await api("GET", "/api/v1/secrets");
      const rec = (Array.isArray(list) ? list : list.secrets ?? []).find((x) => x.id === secretId && x.publicValue);
      if (rec) {
        out(rec.publicValue);
        break;
      }
      if (Date.now() > deadline) throw new Error(`${secretId}: no public key from ${node}`);
      await new Promise((r) => setTimeout(r, 2000));
    }
    break;
  }
  case "pubkey": {
    const list = await api("GET", "/api/v1/secrets");
    const rec = (Array.isArray(list) ? list : list.secrets ?? []).find((x) => x.id === args[0]);
    if (!rec?.publicValue) throw new Error(`${args[0]}: no public value`);
    out(rec.publicValue);
    break;
  }
  case "publish": {
    const document = JSON.parse(readFileSync(args[0], "utf8"));
    const d = await api("POST", "/api/v1/drafts", { document });
    // A refused publish must not leave the draft open: the console would show it as this account's saved changes.
    const discard = () => api("DELETE", `/api/v1/drafts/${d.id}`).catch(() => undefined);
    const v = await api("POST", `/api/v1/drafts/${d.id}/validate`);
    if (v && v.ok === false) {
      await discard();
      out(v);
      process.exit(1);
    }
    try {
      out(await api("POST", `/api/v1/drafts/${d.id}/publish`, { message: args[1] ?? "lab publish" }));
    } catch (e) {
      await discard();
      throw e;
    }
    break;
  }
  case "status":
    out(await api("GET", `/api/v1/nodes/${args[0]}`));
    break;
  case "wait-applied": {
    const deadline = Date.now() + Number(args[2] ?? 120) * 1000;
    for (;;) {
      const n = await api("GET", `/api/v1/nodes/${args[0]}`);
      const applied = n?.sync?.appliedRevision ?? n?.sync?.applied_revision ?? n?.applied_revision;
      if (applied === args[1]) {
        out(`${args[0]} applied ${args[1]}`);
        break;
      }
      if (Date.now() > deadline) {
        out(n);
        throw new Error(`${args[0]} did not apply ${args[1]}`);
      }
      await new Promise((r) => setTimeout(r, 2000));
    }
    break;
  }
  case "call": {
    const [method, path, body] = args;
    const payload = body === undefined ? undefined : JSON.parse(body.startsWith("@") ? readFileSync(body.slice(1), "utf8") : body);
    out(await api(method.toUpperCase(), path, payload));
    break;
  }
  default:
    console.error("commands: bootstrap node token wgkey nodekey pubkey publish status wait-applied call");
    process.exit(2);
}
