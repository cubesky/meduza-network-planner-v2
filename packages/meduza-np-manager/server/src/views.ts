// Read models for the UI (AGENT.md §6.2 overview / nodes, §12.2). "No report received" is never shown as healthy:
// freshness is derived from the node's declared behavior and its configured intervals.

import type { Services } from "./app.js";

export type Freshness = "online" | "stale" | "offline" | "never" | "revoked" | "not-enrolled";

export function freshness(input: { behavior: string; lastSeenAt: string | null; streamConnected: boolean; revoked: boolean; enrolled: boolean; heartbeatSeconds?: number; pollSeconds?: number }, now = Date.now()): Freshness {
  if (input.revoked) return "revoked";
  if (!input.enrolled) return "not-enrolled";
  if (!input.lastSeenAt) return "never";
  const age = (now - Date.parse(input.lastSeenAt)) / 1000;
  // Roaming nodes only talk when they poll; judge them by their poll interval, not by push keepalives (§7.3).
  const interval = input.behavior === "roaming" ? input.pollSeconds ?? 60 : input.heartbeatSeconds ?? 15;
  if (input.streamConnected || age <= interval * 3) return "online";
  if (age <= Math.max(interval * 10, 600)) return "stale";
  return "offline";
}

function syncSettings(s: Services, nodeId: string): { heartbeatSeconds?: number; roamingPollSeconds?: number } {
  const desired = s.config.desired(nodeId);
  if (!desired) return {};
  const snap = s.db.get<{ config: string }>("SELECT config FROM node_snapshots WHERE node_id = ? AND revision = ?", nodeId, desired.revision);
  const node = snap ? ((JSON.parse(snap.config) as { node?: { sync?: Record<string, number> } }).node ?? {}) : {};
  return node.sync ?? {};
}

export function overview(s: Services) {
  const nodes = s.nodes.list().map((n) => {
    const latest = s.nodes.latestReport(n.id);
    const report = latest?.report as Record<string, any> | undefined;
    const desired = s.config.desired(n.id);
    const sync = s.nodes.syncState(n.id);
    const intervals = syncSettings(s, n.id);
    const fresh = freshness({
      behavior: n.behavior,
      lastSeenAt: n.last_seen_at,
      streamConnected: s.hub.connected(n.id),
      revoked: !!n.revoked_at,
      enrolled: !!n.public_key,
      heartbeatSeconds: intervals.heartbeatSeconds,
      pollSeconds: intervals.roamingPollSeconds,
    });
    const features = (report?.features ?? []) as Array<Record<string, unknown>>;
    // A failed reapply can leave appliedRevision unchanged. Preserve an explicit current failure even if
    // its ACK was lost; conversely a transport lastError on an in-sync report is not an application error.
    const reportedFailure = !!desired && report?.sync?.state === "apply-failed" && report.sync.desiredRevision === desired.revision;
    const failedRevision = reportedFailure ? desired.revision : (sync?.failed_revision as string | null) ?? null;
    return {
      id: n.id,
      displayName: n.display_name,
      behavior: n.behavior,
      tags: JSON.parse(n.tags) as string[],
      arch: n.arch,
      hostname: n.hostname,
      // installed planner package version from the latest report (the enrollment value is only a fallback)
      plannerVersion:
        ((report?.packages ?? []) as Array<{ name?: string; version?: string }>).find((p) => p.name === "meduza-np-planner")?.version ?? n.planner_version,
      freshness: fresh,
      lastSeenAt: n.last_seen_at,
      reportReceivedAt: latest?.receivedAt ?? null,
      streamConnected: s.hub.connected(n.id),
      sync: report?.sync ?? null,
      desiredRevision: desired?.revision ?? null,
      receivedRevision: (sync?.received_revision as string | null) ?? null,
      appliedRevision: (sync?.applied_revision as string | null) ?? null,
      failedRevision,
      lastError: (reportedFailure ? report.sync.lastError : sync?.last_error) ?? null,
      inSync: !!desired && sync?.applied_revision === desired.revision
        && failedRevision !== desired.revision && !report?.sync?.recoveryError && !report?.sync?.applyStage,
      enabledFeatures: features.filter((f) => f.enabled).map((f) => f.id),
      problems: [
        ...(report?.sync?.recoveryError ? [{ id: "configuration.recovery", reason: String(report.sync.recoveryError) }] : []),
        ...(report?.update?.error ? [{ id: "updater.installation", reason: report.update.error }] : []),
        ...((report?.packages ?? []) as Array<Record<string, unknown>>)
          .filter((p) => p.registration !== "registered")
          .map((p) => ({ id: String(p.name), reason: (p.reason ?? p.reasonCode ?? p.registration) as unknown })),
        ...features.filter((f) => f.registered && f.enabled && !f.available).map((f) => ({ id: f.id, reason: f.reason ?? f.reasonCode })),
      ],
      resources: report?.resources ?? null,
      maintenance: report?.maintenance ?? null,
      update: report?.update ?? null,
    };
  });
  const current = s.config.currentDocument();
  return {
    revision: current.revision,
    nodes,
    counts: {
      total: nodes.length,
      online: nodes.filter((n) => n.freshness === "online").length,
      stale: nodes.filter((n) => n.freshness === "stale").length,
      offline: nodes.filter((n) => n.freshness === "offline" || n.freshness === "never").length,
      outOfSync: nodes.filter((n) => n.desiredRevision && !n.inSync).length,
    },
    server: { connectedStreams: s.hub.size, ...s.limiter.stats() },
  };
}

export function nodeDetail(s: Services, nodeId: string) {
  const n = s.nodes.get(nodeId);
  if (!n) return null;
  const summary = overview(s).nodes.find((x) => x.id === nodeId)!;
  const latest = s.nodes.latestReport(nodeId);
  const installed = new Map<string, Record<string, unknown>>();
  for (const p of ((latest?.report as Record<string, any> | undefined)?.packages ?? []) as Array<Record<string, unknown>>) installed.set(String(p.name), p);
  // Package view: what the node actually has, plus the catalog entries it does not have (shown as "not installed").
  const packages = s.catalog.list().filter((e) => e.manifest.role !== "manager").map((e) => {
    const p = installed.get(e.manifest.package);
    return {
      package: e.manifest.package,
      role: e.manifest.role,
      catalogVersion: e.manifest.version,
      installed: !!p,
      installedVersion: (p?.version as string | undefined) ?? null,
      registration: (p?.registration as string | undefined) ?? null,
      reason: (p?.reason as string | undefined) ?? (p?.reasonCode as string | undefined) ?? null,
      uiCompatible: p ? !p.uiDigest || p.uiDigest === e.uiDigest : null,
    };
  });
  return {
    ...summary,
    enrolledAt: n.enrolled_at,
    revokedAt: n.revoked_at,
    report: latest?.report ?? null,
    packages,
    jobs: s.nodes.jobs(nodeId, 50),
  };
}
