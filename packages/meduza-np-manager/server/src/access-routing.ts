import type { ClusterDocument, NodeSpec } from "./cluster.js";

/** Migrate connection-level switches into the node's Network setting without changing the stored revision. */
export function normalizeAccessRouting(document: ClusterDocument): ClusterDocument {
  const out = structuredClone(document);
  for (const spec of Object.values(out.nodes)) {
    let legacy = false;
    for (const short of ["openvpn", "wireguard"]) {
      const service = spec.services?.[short] as { instances?: Array<{ bgp?: Record<string, unknown>; peers?: Array<{ bgp?: Record<string, unknown> }> }> } | undefined;
      for (const instance of Array.isArray(service?.instances) ? service.instances : []) {
        if (!instance || typeof instance !== "object") continue;
        for (const entry of [instance, ...(Array.isArray(instance.peers) ? instance.peers : [])]) {
          if (!entry || typeof entry !== "object") continue;
          if (!entry.bgp || typeof entry.bgp !== "object") continue;
          legacy ||= entry.bgp.exportAccessPools === true;
          delete entry.bgp.exportAccessPools;
        }
      }
    }
    const origin = spec.routing?.originate;
    if (origin?.access === undefined && (legacy || (Array.isArray(origin?.accessPools) && origin.accessPools.length > 0))) {
      (spec.routing ??= {}).originate = { ...origin, access: true };
    }
    if (spec.routing?.originate) delete spec.routing.originate.accessPools;
  }
  return out;
}

/** Expand the node-wide intent into the existing node protocol understood by FRR. */
export function accessPoolIds(spec: NodeSpec): string[] {
  if (spec.routing?.originate?.access !== true) return [];
  const service = spec.services?.openvpn as { enabled?: boolean; instances?: Array<{ id: string; kind?: string; enabled?: boolean; access?: { pool?: string } }> } | undefined;
  if (service?.enabled === false) return [];
  return [...new Set((service?.instances ?? [])
    .filter((i) => i.enabled !== false && i.kind === "access" && !!i.access?.pool)
    .map((i) => i.id))].sort();
}
