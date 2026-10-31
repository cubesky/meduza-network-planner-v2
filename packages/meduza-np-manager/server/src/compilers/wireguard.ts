// WireGuard compilation (AGENT.md §9.4, D17): WireGuard describes external connections only, so there is nothing
// to resolve across nodes. The compiler only fills the interface's own public key from its key secret, for display
// and for handing to the external side. Managed-node references are rejected earlier (compilers/policy.ts). A peer's
// `bgp` session belongs to FRR (compilers/frr.ts) and is not passed to the plugin.

import type { CompileContext } from "./index.js";

interface WgInterface {
  privateKeyRef?: string;
  publicKey?: string;
  peers?: Array<{ bgp?: unknown }>;
}

export function compileWireGuard(_nodeId: string, service: Record<string, unknown>, ctx: CompileContext): Record<string, unknown> {
  const out = structuredClone(service) as { instances?: WgInterface[] };
  for (const inst of out.instances ?? []) {
    if (inst.privateKeyRef) {
      const own = ctx.secrets.publicValue(inst.privateKeyRef);
      if (own) inst.publicKey = own;
    }
    for (const peer of inst.peers ?? []) delete peer.bgp;
  }
  return out as Record<string, unknown>;
}
