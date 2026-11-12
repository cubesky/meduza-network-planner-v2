// Same-origin API client for the Rust dashboard server: cookie session, CSRF header on writes.
import { t } from "@meduza-np/ui-kit";

export class ApiError extends Error {
  constructor(readonly status: number, readonly code: string, message: string) {
    super(message);
  }
}

let csrf: string | null = null;
export const setCsrf = (t: string | null) => void (csrf = t);

async function call<T>(method: "GET" | "POST", path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {};
  if (method === "POST") {
    headers["content-type"] = "application/json";
    if (path === "/api/mihomo/panel") headers["x-meduza-panel"] = "open";
    if (csrf) headers["x-csrf-token"] = csrf;
  }
  const res = await fetch(path, { method, headers, credentials: "same-origin", body: method === "POST" ? JSON.stringify(body ?? {}) : undefined });
  const text = await res.text();
  const data = text ? (JSON.parse(text) as unknown) : undefined;
  if (!res.ok) {
    const d = (data ?? {}) as { error?: string; message?: string };
    throw new ApiError(res.status, d.error ?? "error", d.message ?? res.statusText);
  }
  return data as T;
}

export const get = <T = unknown>(path: string) => call<T>("GET", path);
export const post = <T = unknown>(path: string, body?: unknown) => call<T>("POST", path, body);

export function errorText(e: unknown): string {
  if (e instanceof ApiError) return e.status === 503 ? t("Service unavailable: {error}", { error: e.message }) : `${e.message} (${e.code})`;
  return e instanceof Error ? t(e.message) : String(e);
}

export interface Session {
  authenticated: boolean;
  user?: string;
  role?: "viewer" | "operator" | "admin";
  csrf?: string;
  via?: "manager" | "local";
  managerLogin: boolean;
}

export const roleAtLeast = (have: string | undefined, need: "viewer" | "operator" | "admin") =>
  ["viewer", "operator", "admin"].indexOf(have ?? "") >= ["viewer", "operator", "admin"].indexOf(need);

// --- shapes of planner/updater replies (subset used by the UI) ------------------------------------------------

export interface Backoff {
  failureLevel: number;
  windowSeconds: number;
  nextAttemptInMs?: number;
  lastFailure?: string;
  serverCooldownMs?: number;
  probing: boolean;
}

export interface Report {
  update?: import("@meduza-np/ui-kit/package-progress").InstallProgress | null;
  nodeId: string;
  bootId: string;
  observedAt: string;
  sync: {
    mode: string;
    transport: string;
    behavior: string;
    state: string;
    desiredRevision?: string;
    receivedRevision?: string;
    validatedRevision?: string;
    appliedRevision?: string;
    observedRevision?: string;
    lastSuccessfulRevision?: string;
    degradedReason?: string;
    applyStage?: string;
    lastError?: string;
    recoveryError?: string;
    backoff: Backoff;
  };
  features: Feature[];
  instances: Array<import("@meduza-np/ui-kit/instance-tree").RuntimeStatus & { mode?: string; providerState?: string; downloadProgress?: import("@meduza-np/ui-kit/package-progress").BinaryTransferProgress | null }>;
  maintenance?: { active: boolean; reason: string; by: string; until: string; applyPaused: boolean } | null;
  resources?: { packages?: import("@meduza-np/ui-kit/package-resources").PackageResources[]; plannerRssKib?: number; memAvailableKib?: number; memTotalKib?: number; stateDiskFreeKib?: number; load1Milli?: number; supervisedProcesses?: number } | null;
  system?: Record<string, unknown> | null;
}

export interface PackageStatus {
  name: string;
  version: string;
  role: string;
  registration: string;
  reasonCode?: string;
  reason?: string;
  provides?: string[];
}

export interface Feature {
  id: string;
  package: string;
  installed: boolean;
  registered: boolean;
  enabled: boolean;
  available: boolean;
  running?: boolean;
  healthy?: boolean;
  provider?: string;
  reasonCode?: string;
  reason?: string;
  limitationCode?: string;
  limitation?: string;
}
