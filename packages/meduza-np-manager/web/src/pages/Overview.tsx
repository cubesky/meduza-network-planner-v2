import { Badge, Card, Loadable, PageHead, StateBadge, StatusDot, Table, msg, relativeTime, t, useAsync } from "@meduza-np/ui-kit";
import { get } from "../api.ts";

export interface NodeSummary {
  id: string;
  displayName: string;
  behavior: string;
  freshness: string;
  lastSeenAt: string | null;
  streamConnected: boolean;
  sync: null | { state: string; mode: string; transport: string; degradedReason?: string; backoff?: { failureLevel: number; nextAttemptInMs?: number; lastFailure?: string } };
  desiredRevision: string | null;
  appliedRevision: string | null;
  failedRevision: string | null;
  lastError: string | null;
  inSync: boolean;
  enabledFeatures: string[];
  problems: Array<{ id: string; reason: string }>;
  resources: null | { memAvailableKib?: number; memTotalKib?: number; plannerRssKib?: number };
  maintenance: null | { reason: string; until: string };
  arch: string | null;
  tags: string[];
}

interface Overview {
  revision: string | null;
  nodes: NodeSummary[];
  counts: { total: number; online: number; stale: number; offline: number; outOfSync: number };
  server: { connectedStreams: number; inFlight: number };
}

export function syncCell(n: NodeSummary) {
  if (!n.sync) return <span class="faint">{t("no report")}</span>;
  return (
    <span class="row" style={{ gap: "6px" }}>
      <StateBadge state={n.sync.state} />
      {n.sync.degradedReason && <Badge tone="warn" title={n.sync.degradedReason}>{t("degraded")}</Badge>}
      {/* backoff left over from an earlier outage is noise once the node is in sync again */}
      {n.sync.state !== "in-sync" && (n.sync.backoff?.failureLevel ?? 0) > 0 && <Badge tone="warn" title={n.sync.backoff?.lastFailure}>{t("backoff {level}", { level: n.sync.backoff?.failureLevel ?? 0 })}</Badge>}
    </span>
  );
}

/** Whether the node runs the configuration it should: up to date, waiting, or failed (with the error as tooltip). */
export function revisionBadge(n: NodeSummary) {
  if (!n.desiredRevision) return <span class="faint">{t("not configured")}</span>;
  if (n.failedRevision && n.failedRevision === n.desiredRevision) return <Badge tone="bad" title={n.lastError ?? ""}>{t("apply failed")}</Badge>;
  if (!n.inSync) return <Badge tone="warn" title={`${n.appliedRevision ?? "—"} → ${n.desiredRevision}`}>{t("pending")}</Badge>;
  return <Badge tone="ok" title={n.appliedRevision ?? ""}>{t("up to date")}</Badge>;
}

export const needsAttention = (n: NodeSummary) => n.problems.length > 0 || n.freshness === "offline" || (!!n.failedRevision && n.failedRevision === n.desiredRevision);

export function OverviewPage() {
  const state = useAsync(() => get<Overview>("/api/v1/overview"), [], 10_000);
  return (
    <div>
      <Loadable state={state}>
        {(o) => {
          const attention = o.nodes.filter(needsAttention);
          return (
            <>
              <PageHead title={t("Overview")} sub={t("Published configuration: {revision}", { revision: o.revision ?? t("none") })} />
              <div class="stats">
                {(
                  [
                    [msg("Nodes"), o.counts.total, ""],
                    [msg("Online"), o.counts.online, ""],
                    [msg("Need attention"), attention.length, attention.length ? "bad" : ""],
                    [msg("Not at the desired revision"), o.counts.outOfSync, o.counts.outOfSync ? "warn" : ""],
                  ] as const
                ).map(([label, value, tone]) => (
                  <div class={`stat ${tone}`}>
                    <div class="value">{value}</div>
                    <div class="label">{t(label)}</div>
                  </div>
                ))}
              </div>
              {attention.length > 0 && (
                <Card title={t("Need attention")}>
                  <div class="stack" style={{ gap: "8px" }}>
                    {attention.map((n) => (
                      <a class="row" href={`#/nodes/${encodeURIComponent(n.id)}`} style={{ color: "inherit" }}>
                        <StatusDot state={n.freshness} />
                        <strong>{n.displayName || n.id}</strong>
                        <span class="muted small">
                          {n.freshness === "offline"
                            ? t("offline since {time}", { time: relativeTime(n.lastSeenAt) })
                            : n.failedRevision === n.desiredRevision && n.lastError
                              ? n.lastError
                              : n.problems.map((p) => p.id).join(", ")}
                        </span>
                      </a>
                    ))}
                  </div>
                </Card>
              )}
              <Table
                rows={o.nodes as unknown as Record<string, unknown>[]}
                rowKey={(r) => String(r.id)}
                onSelect={(r) => (location.hash = `/nodes/${encodeURIComponent(String(r.id))}`)}
                empty={t("No nodes yet. Create one under Nodes and enroll it with an enrollment token.")}
                columns={[
                  {
                    key: "id",
                    label: t("Node"),
                    render: (r) => (
                      <span class="row" style={{ gap: "10px", flexWrap: "nowrap" }}>
                        <StatusDot state={String(r.freshness)} />
                        <strong>{String(r.displayName || r.id)}</strong>
                      </span>
                    ),
                  },
                  { key: "sync", label: t("Sync"), render: (r) => syncCell(r as unknown as NodeSummary) },
                  { key: "rev", label: t("Configuration"), render: (r) => revisionBadge(r as unknown as NodeSummary) },
                  {
                    key: "problems",
                    label: t("Problems"),
                    render: (r) => {
                      const p = r.problems as NodeSummary["problems"];
                      return p.length ? <Badge tone="bad" title={p.map((x) => `${x.id}: ${x.reason}`).join("\n")}>{p.length}</Badge> : <span class="faint">—</span>;
                    },
                  },
                  { key: "lastSeenAt", label: t("Last contact"), render: (r) => <span class="nowrap">{relativeTime(r.lastSeenAt as string | null)}</span> },
                ]}
              />
            </>
          );
        }}
      </Loadable>
    </div>
  );
}
