// API endpoints report either textual problems (LDAP/settings) or structured field problems (config).
export class ApiError extends Error {
  constructor(readonly status: number, readonly code: string, message: string, readonly body?: unknown) {
    super(message || code || `HTTP ${status}`);
  }
}

export function errorText(e: unknown): string {
  if (e instanceof ApiError) {
    const raw = e.body && typeof e.body === "object" ? (e.body as { problems?: unknown }).problems : undefined;
    const problems = Array.isArray(raw) ? raw.flatMap((p: unknown) => {
      if (typeof p === "string") return p.trim() ? [p] : [];
      if (!p || typeof p !== "object") return [];
      const { path, message } = p as { path?: unknown; message?: unknown };
      if (typeof message !== "string" || !message.trim()) return [];
      return [typeof path === "string" && path ? `${path} ${message}` : message];
    }).slice(0, 3) : [];
    return problems.length ? `${e.message}: ${problems.join("; ")}` : e.message;
  }
  if (e instanceof Error) return e.message;
  return String(e);
}
