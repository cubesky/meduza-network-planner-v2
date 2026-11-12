// Per-package cross-node compilation. A package without a compiler is passed through unchanged (its config
// only refers to the node itself). Compilers append problems instead of throwing.

import type { ClusterDocument, Problem, SecretSource } from "../cluster.js";
import type { PackageCatalog } from "../packages.js";

export interface CompileContext {
  doc: ClusterDocument;
  nodeIds: string[];
  secrets: SecretSource;
  catalog: PackageCatalog;
  problems: Problem[];
}

/** Returns the node-local service config, or undefined to omit the service. */
export type ServiceCompiler = (nodeId: string, service: Record<string, unknown>, ctx: CompileContext) => Record<string, unknown> | undefined;

import { compileWireGuard } from "./wireguard.js";
import { compileTinc, normalizeTinc } from "./tinc.js";
import { compileFrr } from "./frr.js";
import { compileMihomo } from "./mihomo.js";

/** MosDNS: the cluster's shared configuration (D58) under the node's own fields. */
const compileMosdns: ServiceCompiler = (_nodeId, service, ctx) => ({ ...structuredClone(ctx.doc.mosdns ?? {}), ...structuredClone(service) });

/**
 * OpenVPN needs no cross-node data; an instance's `bgp` session belongs to FRR and is not passed to the plugin.
 * The plugin's renegotiation rule is checked here too, so such a publish fails instead of only the node's apply.
 */
const compileOpenVpn: ServiceCompiler = (nodeId, service, ctx) => {
  const out = structuredClone(service) as { enabled?: boolean; instances?: Array<{ enabled?: boolean; bgp?: unknown; kind?: string; access?: { renegSeconds?: number } }> };
  if (out.enabled === false) return out;
  (out.instances ?? []).forEach((inst, i) => {
    delete inst.bgp;
    if (inst.enabled === false) return;
    const reneg = inst.access?.renegSeconds;
    if (inst.kind === "access" && typeof reneg === "number" && reneg > 0 && reneg < 300) {
      ctx.problems.push({
        path: `/nodes/${nodeId}/services/openvpn/instances/${i}/access/renegSeconds`,
        node: nodeId,
        message: "0 (disabled) or at least 300: every renegotiation re-checks the user with the manager (D38)",
      });
    }
  });
  return out as Record<string, unknown>;
};

const compilers = new Map<string, ServiceCompiler>([
  ["wireguard", compileWireGuard],
  ["tinc", compileTinc],
  ["frr", compileFrr],
  ["openvpn", compileOpenVpn],
  ["mosdns", compileMosdns],
  ["mihomo", (n, svc, ctx) => compileMihomo(n, svc, ctx, "mihomo")],
  ["mihomo-openwrt", (n, svc, ctx) => compileMihomo(n, svc, ctx, "mihomo-openwrt")],
]);

/** Brings a service written in an earlier shape to the current one before it is validated against its schema. */
export function normalizeService(short: string, service: Record<string, unknown>): Record<string, unknown> {
  if (short === "dnsmasq") {
    // Backend endpoints follow the enabled DNS services; stale manual lists must not survive a migration.
    const { backends: _backends, ...out } = service;
    if (Array.isArray(out.fallbackServers) && out.fallbackServers.length === 0) delete out.fallbackServers;
    return out;
  }
  return short === "tinc" ? (normalizeTinc(service) as Record<string, unknown>) : service;
}

export function registerCompiler(short: string, compiler: ServiceCompiler): void {
  compilers.set(short, compiler);
}

export function compileService(short: string, nodeId: string, service: Record<string, unknown>, ctx: CompileContext): Record<string, unknown> | undefined {
  const compiler = compilers.get(short);
  return compiler ? compiler(nodeId, service, ctx) : structuredClone(service);
}
