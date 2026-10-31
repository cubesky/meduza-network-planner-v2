// Tinc mesh compilation (AGENT.md §9.1, D22): there is one internal mesh; every node with Tinc enabled is a member.
// Each member's snapshot gets the *public* material of all other members (name, public key, addresses, port,
// subnets); private keys never leave their node or the manager's scoped secret store.
//
// The internal mesh runs without NAT (D17): every member must be directly reachable on its `publicAddresses`, and
// roaming nodes (which may sit behind NAT) do not join it; they connect to a mesh gateway with WireGuard or
// OpenVPN instead.

import type { CompileContext } from "./index.js";

export interface TincMesh {
  enabled?: boolean;
  name?: string;
  port?: number;
  address?: string;
  device?: string;
  subnets?: string[];
  publicAddresses?: string[];
  privateKeyRef?: string;
  publicKey?: string;
  hosts?: Array<Record<string, unknown>>;
  [k: string]: unknown;
}

export function normalizeTincName(nodeId: string): string {
  return nodeId.replace(/[^A-Za-z0-9]/g, "_");
}

/**
 * `services.tinc` as the single mesh. Documents of earlier versions listed networks (`instances`); their first
 * entry is the mesh (its `enabled` combined with the service's), and `id`/`netname` are dropped.
 */
export function normalizeTinc(service: Record<string, unknown>): TincMesh {
  if (!Array.isArray(service.instances)) return service as TincMesh;
  const first = (service.instances[0] ?? {}) as Record<string, unknown>;
  const { id: _id, netname: _netname, ...mesh } = first;
  return { ...mesh, enabled: service.enabled !== false && first.enabled !== false };
}

/** The node's mesh membership, or undefined when Tinc is absent or disabled. */
export function tincMesh(service: unknown): TincMesh | undefined {
  if (!service || typeof service !== "object") return undefined;
  const m = normalizeTinc(service as Record<string, unknown>);
  return m.enabled === false ? undefined : m;
}

/** Interface name of the mesh (`tnc-mesh` unless the node sets `device`). */
export const tincDevice = (m: TincMesh): string => m.device ?? "tnc-mesh";

function hostPrefix(cidr: string | undefined): string | null {
  if (!cidr) return null;
  const addr = cidr.split("/")[0]!;
  return addr.includes(":") ? `${addr}/128` : `${addr}/32`;
}

export function compileTinc(nodeId: string, service: Record<string, unknown>, ctx: CompileContext): Record<string, unknown> {
  const out = structuredClone(normalizeTinc(service));
  const base = `/nodes/${nodeId}/services/tinc`;
  // mesh-wide defaults (D58): every member speaks the same cipher/digest unless it sets its own
  if (ctx.doc.tinc?.cipher && out.cipher === undefined) out.cipher = ctx.doc.tinc.cipher;
  if (ctx.doc.tinc?.digest && out.digest === undefined) out.digest = ctx.doc.tinc.digest;
  const behavior = ctx.doc.nodes[nodeId]?.behavior ?? "static";
  // Static members are reached directly (no NAT between them); a roaming member (D27) may sit behind NAT and only
  // dials out to the static members, so it needs no publicAddresses.
  if (out.enabled !== false && behavior !== "roaming" && !(out.publicAddresses ?? []).length) {
    ctx.problems.push({
      path: `${base}/publicAddresses`,
      node: nodeId,
      message: "every static internal mesh member needs publicAddresses that the other members reach directly (no NAT)",
    });
  }
  if (out.privateKeyRef) {
    const pub = ctx.secrets.publicValue(out.privateKeyRef);
    if (pub) out.publicKey = pub;
  }
  const ownName = out.name ?? normalizeTincName(nodeId);
  const names = new Map<string, string>([[ownName, nodeId]]);
  const managed: Array<Record<string, unknown>> = [];
  let pendingStatic = false;
  for (const other of ctx.nodeIds) {
    if (other === nodeId) continue;
    const peer = tincMesh(ctx.doc.nodes[other]?.services?.tinc);
    if (!peer) continue;
    const name = peer.name ?? normalizeTincName(other);
    if (names.has(name)) {
      ctx.problems.push({ path: base, node: nodeId, message: `tinc name ${name} of ${other} collides with ${names.get(name)}; set an explicit name` });
      continue;
    }
    names.set(name, other);
    const key = peer.privateKeyRef ? ctx.secrets.publicValue(peer.privateKeyRef) : null;
    if (!key) {
      // D28: a key requested on a node without the Tinc package yet; the member joins once the node generated it
      // (the manager then distributes it with a new revision).
      if (peer.privateKeyRef && ctx.secrets.pending?.(peer.privateKeyRef)) {
        if ((peer.publicAddresses ?? []).length) pendingStatic = true;
        ctx.problems.push({ path: base, node: nodeId, severity: "warning", message: `member ${other} joins once its tinc key is generated on ${other}` });
      } else {
        ctx.problems.push({ path: base, node: nodeId, message: `member ${other} has no generated tinc key yet` });
      }
      continue;
    }
    managed.push({
      name,
      publicKey: key,
      addresses: peer.publicAddresses ?? [],
      port: peer.port ?? 655,
      subnets: [hostPrefix(peer.address), ...(peer.subnets ?? [])].filter((x): x is string => !!x),
      node: other,
    });
  }
  // Manually listed (unmanaged) hosts are kept; managed members are always regenerated.
  const manual = (out.hosts ?? []).filter((h) => !h.node && !names.has(String(h.name)));
  out.hosts = [...manual, ...managed];
  if (out.enabled !== false && behavior === "roaming" && !out.hosts.some((h) => ((h.addresses as unknown[] | undefined) ?? []).length)) {
    ctx.problems.push(
      pendingStatic
        ? { path: base, node: nodeId, severity: "warning", message: "a roaming mesh member dials out to the static members: it connects once their tinc keys are generated" }
        : { path: base, node: nodeId, message: "a roaming mesh member dials out to the static members: no member with publicAddresses to connect to" },
    );
  }
  return out as Record<string, unknown>;
}
