// Mihomo subscriptions (D29): one cluster-wide list (`mihomo.subscriptions`, URL kept as a secret shared with every
// node, scope `*`), one of them the default, and a refresh interval. A node running Mihomo (meduza-np-mihomo or the
// OpenClash adapter meduza-np-mihomo-openwrt) uses the default unless its `subscription` names another entry of the
// list; the compiler turns that into the plugin's own `subscriptions` (only the chosen one, so a node receives only
// the URL it uses) and `activeSubscription`. Documents without a global list keep their per-node subscriptions.

import type { ClusterDocument, Problem } from "../cluster.js";
import type { CompileContext } from "./index.js";

export interface MihomoSubscription {
  id: string;
  name?: string;
  /** Secret holding the URL (URLs usually embed an access token). */
  urlRef: string;
  enabled?: boolean;
}

export interface MihomoCatalog {
  subscriptions?: MihomoSubscription[];
  /** Subscription used by every node that does not choose another one. */
  default?: string;
  /** Automatic refresh interval (seconds). */
  refreshSeconds?: number;
}

export const DEFAULT_REFRESH_SECONDS = 43_200;
const ID_RE = /^[a-z0-9][a-z0-9_-]{0,62}$/;

/** Cluster-level checks of the subscription list. */
export function mihomoProblems(doc: ClusterDocument): Problem[] {
  const cat = doc.mihomo;
  if (!cat) return [];
  const problems: Problem[] = [];
  const ids = new Set<string>();
  for (const [i, s] of (cat.subscriptions ?? []).entries()) {
    if (!ID_RE.test(s.id ?? "") || ids.has(s.id)) problems.push({ path: `/mihomo/subscriptions/${i}/id`, message: "subscription ids are unique stable ids" });
    ids.add(s.id);
    if (!s.urlRef) problems.push({ path: `/mihomo/subscriptions/${i}/urlRef`, message: "enter the subscription URL" });
  }
  const enabled = (cat.subscriptions ?? []).filter((s) => s.enabled !== false);
  if (cat.default !== undefined && !enabled.some((s) => s.id === cat.default)) problems.push({ path: "/mihomo/default", message: `no enabled subscription ${cat.default}` });
  if (cat.default === undefined && enabled.length) problems.push({ path: "/mihomo/default", message: "choose the default subscription" });
  const r = cat.refreshSeconds;
  if (r !== undefined && (!Number.isInteger(r) || r < 60)) problems.push({ path: "/mihomo/refreshSeconds", message: "at least 60 seconds" });
  return problems;
}

export function compileMihomo(nodeId: string, service: Record<string, unknown>, ctx: CompileContext, short: string): Record<string, unknown> {
  const out = structuredClone(service) as Record<string, unknown> & { subscription?: string; refreshSeconds?: number };
  const chosen = out.subscription;
  const ownRefresh = out.refreshSeconds;
  delete out.subscription;
  delete out.refreshSeconds;
  if (ownRefresh !== undefined && (!Number.isInteger(ownRefresh) || ownRefresh < 60)) {
    ctx.problems.push({ path: `/nodes/${nodeId}/services/${short}/refreshSeconds`, node: nodeId, message: "at least 60 seconds" });
  }
  const cat = ctx.doc.mihomo;
  const list = cat?.subscriptions ?? [];
  if (!list.length) {
    // no global list: the node's own subscriptions (documents written before D29)
    if (chosen) ctx.problems.push({ path: `/nodes/${nodeId}/services/${short}/subscription`, node: nodeId, message: "there are no Mihomo subscriptions in Global settings" });
    return out;
  }
  if (out.enabled === false) return out;
  const id = chosen ?? cat?.default;
  const sub = list.find((s) => s.id === id && s.enabled !== false);
  if (!sub) {
    ctx.problems.push(
      chosen
        ? { path: `/nodes/${nodeId}/services/${short}/subscription`, node: nodeId, message: `no enabled subscription ${chosen} in Global settings` }
        : { path: `/nodes/${nodeId}/services/${short}`, node: nodeId, message: "no default Mihomo subscription in Global settings" },
    );
    return out;
  }
  out.subscriptions = [
    // the node's own interval (D58) over the cluster's
    { id: sub.id, ...(sub.name ? { name: sub.name } : {}), urlRef: sub.urlRef, refreshSeconds: ownRefresh ?? cat?.refreshSeconds ?? DEFAULT_REFRESH_SECONDS },
  ];
  out.activeSubscription = sub.id;
  return out;
}

/** The subscription a node's Mihomo uses (for the global refresh), or undefined. */
export function mihomoSubscriptionOf(doc: ClusterDocument, nodeId: string, short: string): string | undefined {
  const svc = doc.nodes[nodeId]?.services?.[short] as { enabled?: boolean; subscription?: string; activeSubscription?: string } | undefined;
  if (!svc || svc.enabled === false) return undefined;
  return doc.mihomo?.subscriptions?.length ? (svc.subscription ?? doc.mihomo.default) : svc.activeSubscription;
}
