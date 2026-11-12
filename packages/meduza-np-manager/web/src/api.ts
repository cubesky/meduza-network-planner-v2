// API client: same-origin cookie session, CSRF header on writes, Passkey re-verification on demand.

import { startAuthentication, startRegistration } from "@simplewebauthn/browser";
import type { Locale } from "@meduza-np/ui-kit";
import { ApiError } from "./api-error.ts";
export { ApiError, errorText } from "./api-error.ts";

let csrfToken: string | null = null;

export function setCsrf(token: string | null): void {
  csrfToken = token;
}

async function raw<T>(method: string, path: string, body?: unknown): Promise<T> {
  const write = method !== "GET";
  const headers: Record<string, string> = {};
  if (write) {
    headers["content-type"] = "application/json";
    if (csrfToken) headers["x-csrf-token"] = csrfToken;
  }
  const res = await fetch(path, { method, headers, credentials: "same-origin", body: write ? JSON.stringify(body ?? {}) : undefined });
  const text = await res.text();
  const data = text ? (JSON.parse(text) as unknown) : undefined;
  if (!res.ok) {
    const d = (data ?? {}) as { error?: string; message?: string };
    throw new ApiError(res.status, d.error ?? "error", d.message ?? res.statusText, data);
  }
  return data as T;
}

/** Calls the API; if the server asks for a recent Passkey verification, runs it once and retries. */
export async function api<T = unknown>(method: string, path: string, body?: unknown): Promise<T> {
  try {
    return await raw<T>(method, path, body);
  } catch (e) {
    if (e instanceof ApiError && e.code === "reauth-required") {
      await reauthenticate();
      return raw<T>(method, path, body);
    }
    throw e;
  }
}

export const get = <T = unknown>(path: string) => api<T>("GET", path);
export const post = <T = unknown>(path: string, body?: unknown) => api<T>("POST", path, body ?? {});
export const put = <T = unknown>(path: string, body: unknown) => api<T>("PUT", path, body);
export const patch = <T = unknown>(path: string, body: unknown) => api<T>("PATCH", path, body);
export const del = <T = unknown>(path: string) => api<T>("DELETE", path, {});

export async function reauthenticate(): Promise<void> {
  const { challengeId, options } = await raw<{ challengeId: string; options: Parameters<typeof startAuthentication>[0]["optionsJSON"] }>("POST", "/api/v1/auth/reauth/options", {});
  const response = await startAuthentication({ optionsJSON: options });
  await raw("POST", "/api/v1/auth/reauth/verify", { challengeId, response });
}

export async function login(): Promise<void> {
  const { challengeId, options } = await raw<{ challengeId: string; options: Parameters<typeof startAuthentication>[0]["optionsJSON"] }>("POST", "/api/v1/auth/passkeys/authenticate/options", {});
  const response = await startAuthentication({ optionsJSON: options });
  await raw("POST", "/api/v1/auth/passkeys/authenticate/verify", { challengeId, response });
}

/** Registers a passkey: bootstrap (first admin), invite, or an additional key for the signed-in account. */
export async function registerPasskey(input: { bootstrapToken?: string; username?: string; displayName?: string; inviteToken?: string; name?: string }): Promise<{ signedIn: boolean }> {
  const { name, ...rest } = input;
  const { challengeId, options } = await api<{ challengeId: string; options: Parameters<typeof startRegistration>[0]["optionsJSON"] }>("POST", "/api/v1/auth/passkeys/register/options", rest);
  const response = await startRegistration({ optionsJSON: options });
  return api("POST", "/api/v1/auth/passkeys/register/verify", { challengeId, response, name });
}

export interface Session {
  authenticated: boolean;
  bootstrap: "completed" | "open" | "unavailable" | "expired";
  user?: { id: string; username: string; displayName: string; role: "admin" | "operator" | "viewer"; settings?: { locale?: Locale } };
  csrfToken?: string;
}

/** Saves the interface language in the account's settings; `null` follows the browser again. */
export function saveLocale(locale: Locale | null): Promise<unknown> {
  return put("/api/v1/account/settings", { locale });
}

export async function session(): Promise<Session> {
  const s = await raw<Session>("GET", "/api/v1/auth/session");
  setCsrf(s.csrfToken ?? null);
  return s;
}
