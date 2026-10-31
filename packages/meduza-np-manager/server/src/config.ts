// Deployment configuration (AGENT.md §14.4.2). Environment variables plus an optional read-only JSON file.
// Secret material is only ever read from files (Podman secrets / read-only mounts), never from env values.

import { readFileSync, existsSync } from "node:fs";
import { isIP } from "node:net";
import { resolve } from "node:path";

export interface ManagerConfig {
  publicOrigin: string;
  rpId: string;
  rpName: string;
  listenHost: string;
  port: number;
  dataDir: string;
  sessionSecret: Buffer;
  secretEncryptionKey: Buffer;
  bootstrapToken: string | null;
  trustedProxies: string[];
  packagesDir: string;
  webDir: string;
  /** Session lifetime and idle timeout. */
  sessionTtlSeconds: number;
  sessionIdleSeconds: number;
  /** Re-verification window for sensitive operations (secret export). */
  reauthWindowSeconds: number;
  limits: Limits;
  /** Development only: allow http:// origins (never in production images). */
  allowInsecureOrigin: boolean;
}

/** Server-side capacity protection (§7.5.3): global concurrency and per-node request rate. */
export interface Limits {
  maxConcurrentStreams: number;
  maxStreamHandshakesPerSecond: number;
  maxConcurrentNodeRequests: number;
  perNodeRequestsPerMinute: number;
  maxRequestBodyBytes: number;
}

export class ConfigError extends Error {}

function readSecretFile(path: string | undefined, name: string, minBytes: number): Buffer {
  if (!path) throw new ConfigError(`${name} is required (path to a secret file)`);
  if (!existsSync(path)) throw new ConfigError(`${name}: file ${path} does not exist`);
  const raw = readFileSync(path);
  // Accept raw bytes, hex or base64 text; key material is normalized to bytes.
  const text = raw.toString("utf8").trim();
  let bytes: Buffer = raw;
  if (/^[0-9a-fA-F]+$/.test(text) && text.length % 2 === 0) bytes = Buffer.from(text, "hex");
  else if (/^[A-Za-z0-9+/=_-]+$/.test(text)) bytes = Buffer.from(text, "base64");
  if (bytes.length < minBytes) throw new ConfigError(`${name}: need at least ${minBytes} bytes of key material`);
  return bytes;
}

function int(value: string | undefined, fallback: number, name: string): number {
  if (value === undefined || value === "") return fallback;
  const n = Number(value);
  if (!Number.isInteger(n) || n <= 0) throw new ConfigError(`${name} must be a positive integer`);
  return n;
}

export function validateTrustedProxy(entry: string): boolean {
  const [addr, prefix] = entry.split("/");
  if (!addr || isIP(addr) === 0) return false;
  if (prefix === undefined) return true;
  const p = Number(prefix);
  return Number.isInteger(p) && p >= 0 && p <= (isIP(addr) === 4 ? 32 : 128);
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env): ManagerConfig {
  let file: Record<string, unknown> = {};
  if (env.CONFIG_FILE) {
    if (!existsSync(env.CONFIG_FILE)) throw new ConfigError(`CONFIG_FILE ${env.CONFIG_FILE} does not exist`);
    file = JSON.parse(readFileSync(env.CONFIG_FILE, "utf8")) as Record<string, unknown>;
  }
  const pick = (key: string): string | undefined => {
    const v = env[key] ?? file[key];
    return v === undefined || v === null ? undefined : String(v);
  };

  const allowInsecureOrigin = pick("ALLOW_INSECURE_ORIGIN") === "1";
  const publicOrigin = pick("PUBLIC_ORIGIN");
  if (!publicOrigin) throw new ConfigError("PUBLIC_ORIGIN is required, e.g. https://manager.example.test");
  let origin: URL;
  try {
    origin = new URL(publicOrigin);
  } catch {
    throw new ConfigError("PUBLIC_ORIGIN must be an absolute URL");
  }
  if (origin.protocol !== "https:" && !allowInsecureOrigin) throw new ConfigError("PUBLIC_ORIGIN must use https");
  if (origin.pathname !== "/" || origin.search || origin.hash) throw new ConfigError("PUBLIC_ORIGIN must be an origin without path");

  const rpId = pick("RP_ID") ?? origin.hostname;
  // The RP ID must be the origin's host or a registrable suffix of it (WebAuthn §5.1.4.1 / §13.4.9).
  if (origin.hostname !== rpId && !origin.hostname.endsWith(`.${rpId}`)) {
    throw new ConfigError(`RP_ID ${rpId} is not compatible with ${origin.hostname}`);
  }

  const trustedProxies = (pick("TRUSTED_PROXIES") ?? "").split(",").map((s) => s.trim()).filter(Boolean);
  for (const p of trustedProxies) if (!validateTrustedProxy(p)) throw new ConfigError(`TRUSTED_PROXIES: invalid entry ${p}`);

  const bootstrapPath = pick("BOOTSTRAP_TOKEN_FILE");
  const bootstrapToken = bootstrapPath && existsSync(bootstrapPath) ? readFileSync(bootstrapPath, "utf8").trim() : null;
  if (bootstrapToken !== null && bootstrapToken.length < 24) throw new ConfigError("bootstrap token must be at least 24 characters");

  return {
    publicOrigin: origin.origin,
    rpId,
    rpName: pick("RP_NAME") ?? "Meduza NP Manager",
    listenHost: pick("LISTEN_HOST") ?? "0.0.0.0",
    port: int(pick("PORT"), 8787, "PORT"),
    dataDir: resolve(pick("DATA_DIR") ?? "/var/lib/meduza-np-manager"),
    sessionSecret: readSecretFile(pick("SESSION_SECRET_FILE"), "SESSION_SECRET_FILE", 32),
    secretEncryptionKey: readSecretFile(pick("SECRET_ENCRYPTION_KEY_FILE"), "SECRET_ENCRYPTION_KEY_FILE", 32).subarray(0, 32),
    bootstrapToken,
    trustedProxies,
    packagesDir: resolve(pick("PACKAGES_DIR") ?? "/usr/share/meduza-np-manager/packages"),
    webDir: resolve(pick("WEB_DIR") ?? "/usr/share/meduza-np-manager/web"),
    sessionTtlSeconds: int(pick("SESSION_TTL_SECONDS"), 12 * 3600, "SESSION_TTL_SECONDS"),
    sessionIdleSeconds: int(pick("SESSION_IDLE_SECONDS"), 3600, "SESSION_IDLE_SECONDS"),
    reauthWindowSeconds: int(pick("REAUTH_WINDOW_SECONDS"), 300, "REAUTH_WINDOW_SECONDS"),
    limits: {
      maxConcurrentStreams: int(pick("MAX_CONCURRENT_STREAMS"), 5000, "MAX_CONCURRENT_STREAMS"),
      maxStreamHandshakesPerSecond: int(pick("MAX_STREAM_HANDSHAKES_PER_SECOND"), 50, "MAX_STREAM_HANDSHAKES_PER_SECOND"),
      maxConcurrentNodeRequests: int(pick("MAX_CONCURRENT_NODE_REQUESTS"), 64, "MAX_CONCURRENT_NODE_REQUESTS"),
      perNodeRequestsPerMinute: int(pick("PER_NODE_REQUESTS_PER_MINUTE"), 60, "PER_NODE_REQUESTS_PER_MINUTE"),
      maxRequestBodyBytes: int(pick("MAX_REQUEST_BODY_BYTES"), 2 * 1024 * 1024, "MAX_REQUEST_BODY_BYTES"),
    },
    allowInsecureOrigin,
  };
}
