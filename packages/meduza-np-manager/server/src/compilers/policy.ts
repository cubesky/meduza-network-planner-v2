// Connection roles (docs/DECISIONS.md D17): managed nodes reach each other only over the internal Tinc mesh, which
// runs without NAT. WireGuard and OpenVPN describe external connections (third-party sites, remote clients, Access
// users) with explicit keys/endpoints; they never reference another managed node. Checked before schema validation
// so the operator sees the rule instead of a generic "unknown property" error.

import type { Problem } from "../cluster.js";

const MESH_HINT = "managed nodes reach each other over the internal Tinc mesh";

export function connectionPolicy(short: string, nodeId: string, raw: Record<string, unknown>): Problem[] {
  if (raw.enabled === false) return [];
  const problems: Problem[] = [];
  const base = `/nodes/${nodeId}/services/${short}`;
  const instances = (Array.isArray(raw.instances) ? raw.instances : []) as Array<Record<string, unknown>>;
  if (short === "wireguard") {
    for (const [i, inst] of instances.entries()) {
      if (!inst || inst.enabled === false) continue;
      for (const [j, peer] of ((Array.isArray(inst.peers) ? inst.peers : []) as Array<Record<string, unknown>>).entries()) {
        if (!peer || peer.enabled === false) continue;
        if (peer.node !== undefined || peer.instance !== undefined) {
          problems.push({
            path: `${base}/instances/${i}/peers/${j}`,
            node: nodeId,
            message: `WireGuard is for external connections only; ${MESH_HINT}. Remove node/instance and enter the external peer's publicKey and endpoint`,
          });
        }
      }
    }
  }
  if (short === "openvpn") {
    for (const [i, inst] of instances.entries()) {
      if (!inst || inst.enabled === false) continue;
      if (inst.peer !== undefined) {
        problems.push({
          path: `${base}/instances/${i}/peer`,
          node: nodeId,
          message: `OpenVPN is for external connections only (external Site2Site, Access clients); ${MESH_HINT}. Remove peer and list the external remotes`,
        });
      }
    }
  }
  return problems;
}
