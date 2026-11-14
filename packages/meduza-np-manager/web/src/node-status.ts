/** Presence is independent of service/configuration health: roaming nodes may intentionally sleep. */
interface NodeStatus {
  behavior: string;
  freshness: string;
  problems: unknown[];
  failedRevision: string | null;
  desiredRevision: string | null;
}

export const isOffline = (n: { freshness: string }) => n.freshness === "offline" || n.freshness === "never";

export const needsAttention = (n: NodeStatus) => n.problems.length > 0
  || (n.freshness === "offline" && n.behavior !== "roaming")
  || (!!n.failedRevision && n.failedRevision === n.desiredRevision);

/** Preserve the actual presence label; only its colour becomes neutral. */
export const presenceTone = (n: { behavior?: unknown; freshness?: unknown }): "idle" | undefined =>
  n.behavior === "roaming" && ["offline", "stale", "never"].includes(String(n.freshness)) ? "idle" : undefined;
