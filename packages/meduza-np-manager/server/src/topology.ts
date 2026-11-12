// Routing topology for the console's graph (D26). Built from what was published (each node's compiled FRR neighbors:
// which sessions should exist) and what the nodes report (`bgp-neighbor` instances: state, prefix counts and the
// routes learned from each neighbor). Every managed node is a vertex carrying its AS; the console groups the nodes
// of one AS into a cluster labelled with the ASN (D26). iBGP sessions link two mesh members. eBGP sessions link the
// two nodes when both ends are managed (a branch in its own AS and the edge router it peers with: the sessions of
// both sides share the link), otherwise the node running them (an edge router) and a vertex for the unmanaged
// router at the other end (`peer`, named by its BGP hostname or router ID when the node reports them), clustered in
// its AS like the nodes. ASes further away are known only from the AS paths of the routes those routers send: each
// is one `as` vertex without routers, chained by `path` links in path order (which of their routers connect, and
// how, is unknown).

import type { Services } from "./app.js";
import { overview } from "./views.js";
import { isIPv4 } from "node:net";
import { asPathSegments, type BgpPath, type BgpPathReport } from "./bgp-paths.js";

export type LinkState = "up" | "partial" | "down" | "unknown";

export interface TopologyVertex {
  id: string;
  kind: "node" | "peer" | "as";
  label: string;
  asn?: number;
  /** peer vertices: the session address and, when reported, the router ID */
  address?: string;
  /** all session addresses of a merged peer; address remains the first for older clients */
  addresses?: string[];
  routerId?: string;
  /** node vertices: freshness and behavior from the overview */
  freshness?: string;
  behavior?: string;
  /** node vertices: runs eBGP sessions (an edge router of its AS) */
  edge?: boolean;
  /** AS vertices: the internal AS (reached from nodes outside it) */
  internal?: boolean;
}

export interface TopologySession {
  /** node whose FRR runs the session */
  node: string;
  neighbor: string;
  address: string;
  remoteAs?: number;
  /** reported FRR state, or "not reported" */
  bgpState: string;
  uptime?: string;
  prefixesReceived?: number;
  prefixesSent?: number;
  routes?: string[];
  routesTruncated?: boolean;
  transport?: string;
  /** external sessions: the peer's announced router ID and hostname, the AS paths of its routes */
  routerId?: string;
  hostname?: string;
  asPaths?: number[][];
}

export interface TopologyLink {
  id: string;
  /** `path`: consecutive ASes in the AS path of learned routes (no session of ours) */
  kind: "ibgp" | "ebgp" | "path";
  source: string;
  target: string;
  state: LinkState;
  sessions: TopologySession[];
  /** path links: the nodes whose routes carry this hop, and how many distinct AS paths do */
  via?: string[];
  paths?: number;
  asSource?: number;
  asTarget?: number;
}

export interface Topology {
  asNames: Record<string, string>;
  internalAsn: number | null;
  vertices: TopologyVertex[];
  links: TopologyLink[];
  bgpPaths: BgpPath[];
  pathReports: BgpPathReport[];
}

interface CompiledTunnels {
  wireguard?: { instances?: Array<{ addresses?: string[]; enabled?: boolean }> };
  openvpn?: { instances?: Array<{ ifconfig?: { local?: string }; enabled?: boolean }> };
}

interface CompiledFrr {
  enabled?: boolean;
  localAsn?: number;
  routerId?: string;
  internalNeighbors?: Array<{ id: string; node?: string; address: string; enabled?: boolean }>;
  externalNeighbors?: Array<{ id: string; address: string; remoteAsn: number; transport?: string; enabled?: boolean }>;
}

const nodeVertex = (id: string) => `node:${id}`;
const asVertex = (asn: number) => `as:${asn}`;
const peerVertex = (asn: number, key: string) => `peer:${asn}:${key}`;
/** FRR names the BGP daemon of a host "<host>-bgpd" when no hostname is configured for it */
const hostLabel = (h: string) => h.replace(/-bgpd$/, "");
const validRouterId = (value: unknown): string | undefined => typeof value === "string" && isIPv4(value) && value !== "0.0.0.0" ? value : undefined;
const identityKey = (asn: number, id: string) => `${asn}:${id}`;
const addressKey = (address: string): string => {
  if (!address.includes(":")) return address;
  try { return new URL(`http://[${address}]/`).hostname; } catch { return address; }
};
/** A duplicate identity is ambiguous: never let enumeration order decide which managed node it names. */
function recordIdentity(map: Map<string, string | null>, key: string, value: string): void {
  if (!map.has(key)) map.set(key, value);
  else if (map.get(key) !== value) map.set(key, null);
}

function stateOf(sessions: TopologySession[]): LinkState {
  const reported = sessions.filter((x) => x.bgpState !== "not reported");
  if (!reported.length) return "unknown";
  const up = reported.filter((x) => x.bgpState === "Established").length;
  if (up === sessions.length) return "up";
  return up ? "partial" : "down";
}

export function topology(s: Services): Topology {
  const asNames = Object.fromEntries((s.config.currentDocument().document?.asAddresses ?? []).map((a) => [a.asn, a.name.trim()]));
  const nodes = overview(s).nodes.filter((n) => n.freshness !== "revoked").sort((a, b) => a.id.localeCompare(b.id));
  let internalAsn: number | null = null;
  const asnOf = new Map<string, number>();
  const frrOf = new Map<string, CompiledFrr>();
  const routerIdOf = new Map<string, string>();
  const managedRouter = new Map<string, string | null>();
  // tunnel address → node: the managed node at the far end of an eBGP session
  const tunnelNode = new Map<string, string>();
  for (const n of nodes) {
    const rev = s.config.desired(n.id)?.revision;
    const config = rev ? (s.config.snapshot(n.id, rev)?.config as { node?: { routerId?: string }; routing?: { internalAsn?: number }; services?: { frr?: CompiledFrr } & CompiledTunnels } | undefined) : undefined;
    internalAsn ??= config?.routing?.internalAsn ?? null;
    for (const i of config?.services?.wireguard?.instances ?? []) if (i.enabled !== false) for (const a of i.addresses ?? []) tunnelNode.set(a.split("/")[0]!, n.id);
    for (const i of config?.services?.openvpn?.instances ?? []) if (i.enabled !== false && i.ifconfig?.local) tunnelNode.set(i.ifconfig.local.split("/")[0]!, n.id);
    const frr = config?.services?.frr;
    if (!frr || frr.enabled === false) continue;
    frrOf.set(n.id, frr);
    const asn = frr.localAsn ?? config?.routing?.internalAsn;
    if (asn !== undefined) asnOf.set(n.id, asn);
    const routerId = validRouterId(frr.routerId ?? config?.node?.routerId);
    if (routerId) {
      routerIdOf.set(n.id, routerId);
      if (asn !== undefined) recordIdentity(managedRouter, identityKey(asn, routerId), n.id);
    }
  }

  // Reported neighbor state by node and neighbor id (`frr/main/bgp-<id>`).
  const reported = (node: string): Map<string, Record<string, unknown>> => {
    const out = new Map<string, Record<string, unknown>>();
    // instance details are flattened into the instance status (meduza_np_protocol::report::InstanceStatus)
    const instances = (s.nodes.latestReport(node)?.report.instances ?? []) as Array<Record<string, unknown> & { id?: string; kind?: string }>;
    for (const i of instances) if (i.kind === "bgp-neighbor" && i.id?.startsWith("frr/main/bgp-")) out.set(i.id.slice("frr/main/bgp-".length), i);
    return out;
  };
  const session = (node: string, neighbor: string, address: string, extra: Partial<TopologySession>, r: Map<string, Record<string, unknown>>): TopologySession => {
    const d = r.get(neighbor);
    const num = (v: unknown) => (typeof v === "number" ? v : undefined);
    return {
      node,
      neighbor,
      address,
      ...extra,
      bgpState: typeof d?.bgpState === "string" ? d.bgpState : "not reported",
      ...(d?.remoteAs !== undefined ? { remoteAs: num(d.remoteAs) } : {}),
      ...(typeof d?.uptime === "string" ? { uptime: d.uptime } : {}),
      ...(num(d?.prefixesReceived) !== undefined ? { prefixesReceived: num(d?.prefixesReceived) } : {}),
      ...(num(d?.prefixesSent) !== undefined ? { prefixesSent: num(d?.prefixesSent) } : {}),
      ...(Array.isArray(d?.routes) ? { routes: (d.routes as unknown[]).map(String) } : {}),
      ...(d?.routesTruncated === true ? { routesTruncated: true } : {}),
      ...(typeof d?.routerId === "string" ? { routerId: d.routerId } : {}),
      ...(typeof d?.hostname === "string" && d.hostname ? { hostname: d.hostname } : {}),
      ...(Array.isArray(d?.asPaths) ? { asPaths: (d.asPaths as unknown[]).filter(Array.isArray).map((p) => (p as unknown[]).map(Number).filter(Number.isInteger)) } : {}),
    };
  };

  const vertices = new Map<string, TopologyVertex>();
  const bgpPaths: BgpPath[] = [];
  const pathReports: BgpPathReport[] = [];
  for (const n of nodes) {
    const instances = (s.nodes.latestReport(n.id)?.report.instances ?? []) as Array<Record<string, unknown>>;
    const snapshot = instances.find((i) => i.kind === "bgp-paths") as Record<string, unknown> | undefined;
    if (!snapshot) {
      const failure = instances.find((i) => i.kind === "probe" && i.id === "frr/probe");
      if (reported(n.id).size || failure || frrOf.get(n.id)?.externalNeighbors?.length || frrOf.get(n.id)?.internalNeighbors?.length) {
        pathReports.push({ node: n.id, complete: false, available: false, addressFamilies: [], errors: typeof failure?.reason === "string" ? [failure.reason] : [] });
      }
      continue;
    }
    pathReports.push({ node: n.id, complete: snapshot.complete === true, available: true,
      addressFamilies: Array.isArray(snapshot.addressFamilies) ? snapshot.addressFamilies.map(String) : [],
      errors: Array.isArray(snapshot.errors) ? snapshot.errors.map(String) : [] });
    const peers = snapshot.pathsByNeighbor;
    if (!peers || typeof peers !== "object" || Array.isArray(peers)) continue;
    const cfg = frrOf.get(n.id);
    for (const [address, paths] of Object.entries(peers)) {
      if (!Array.isArray(paths)) continue;
      const neighbor = [...(cfg?.internalNeighbors ?? []), ...(cfg?.externalNeighbors ?? [])].find((p) => addressKey(p.address) === addressKey(address))?.id ?? address;
      for (const p of paths) {
        if (!p || typeof p !== "object" || typeof p.prefix !== "string" || !["ipv4", "ipv6"].includes(p.family)) continue;
        bgpPaths.push({ node: n.id, address, neighbor, prefix: p.prefix, family: p.family,
          asPath: typeof p.asPath === "string" ? p.asPath : null, valid: typeof p.valid === "boolean" ? p.valid : null,
          best: p.best === true, nextHops: Array.isArray(p.nextHops) ? p.nextHops.filter((h: unknown) => typeof h === "string") : [] });
      }
    }
  }
  for (const n of nodes) {
    vertices.set(nodeVertex(n.id), {
      id: nodeVertex(n.id),
      kind: "node",
      label: n.displayName || n.id,
      freshness: n.freshness,
      behavior: n.behavior,
      ...(asnOf.has(n.id) ? { asn: asnOf.get(n.id) } : {}),
      ...(routerIdOf.has(n.id) ? { routerId: routerIdOf.get(n.id) } : {}),
    });
  }
  const ensureAs = (asn: number): string => {
    const v = asVertex(asn);
    if (!vertices.has(v)) vertices.set(v, { id: v, kind: "as", label: `AS${asn}`, asn, ...(asn === internalAsn ? { internal: true } : {}) });
    return v;
  };
  // A learned router ID identifies a router, scoped to its AS. Keep every address/session when merging.
  const ensurePeer = (sess: TopologySession, asn: number, routerId: string | undefined): string => {
    const v = peerVertex(asn, routerId ?? sess.address);
    const known = vertices.get(v);
    const label = sess.hostname ? hostLabel(sess.hostname) : routerId ?? sess.address;
    if (!known) vertices.set(v, { id: v, kind: "peer", label, asn, address: sess.address, addresses: [sess.address], ...(routerId ? { routerId } : {}) });
    else {
      if (sess.hostname && known.label === (known.routerId ?? known.address)) known.label = label;
      known.addresses = [...new Set([...(known.addresses ?? []), sess.address])].sort();
      known.address = known.addresses[0];
    }
    return v;
  };
  const links = new Map<string, TopologyLink>();
  const link = (kind: TopologyLink["kind"], a: string, b: string): TopologyLink => {
    // node pairs in a fixed order (both sides' sessions on one link); node → AS as given
    const [source, target] = kind !== "path" && a.startsWith("node:") && b.startsWith("node:") ? [a, b].sort() : [a, b];
    const id = `${kind}:${source}|${target}`;
    let l = links.get(id);
    if (!l) links.set(id, (l = { id, kind, source: source!, target: target!, state: "unknown", sessions: [], ...(kind === "path" ? { via: [], paths: 0 } : {}) }));
    return l;
  };

  const external: Array<{ node: string; asn: number; sess: TopologySession }> = [];
  for (const [node, frr] of frrOf) {
    const r = reported(node);
    for (const n of frr.internalNeighbors ?? []) {
      if (n.enabled === false || !n.node || !vertices.has(nodeVertex(n.node))) continue;
      link("ibgp", nodeVertex(node), nodeVertex(n.node)).sessions.push(session(node, n.id, n.address, {}, r));
    }
    for (const n of frr.externalNeighbors ?? []) {
      if (n.enabled === false) continue;
      const sess = session(node, n.id, n.address, { remoteAs: n.remoteAsn, ...(n.transport ? { transport: n.transport } : {}) }, r);
      external.push({ node, asn: n.remoteAsn, sess });
    }
  }
  // FRR learns this from the peer's OPEN and can retain it when the session goes down. Do not split a known
  // router just because one of its sessions is reconnecting; a missing/zero/invalid ID remains unidentified.
  const learnedId = (sess: TopologySession) => validRouterId(sess.routerId);
  // One observer may not have established its own session yet. Reuse a confirmed, unambiguous identity for the
  // same AS/address instead of drawing an address-only copy beside the router another observer already knows.
  const addressRouter = new Map<string, string | null>();
  for (const { asn, sess } of external) {
    const id = learnedId(sess);
    if (id) recordIdentity(addressRouter, identityKey(asn, sess.address), id);
  }
  for (const { node, asn, sess } of external) {
    const routerId = learnedId(sess) ?? addressRouter.get(identityKey(asn, sess.address)) ?? undefined;
    const byId = routerId ? managedRouter.get(identityKey(asn, routerId)) : undefined;
    const byAddress = tunnelNode.get(sess.address);
    const peer = byId ?? (byAddress && asnOf.get(byAddress) === asn ? byAddress : undefined);
    if (peer && peer !== node && vertices.has(nodeVertex(peer))) link("ebgp", nodeVertex(node), nodeVertex(peer)).sessions.push(sess);
    else link("ebgp", nodeVertex(node), ensurePeer(sess, asn, routerId)).sessions.push(sess);
  }

  // Dotted links are AS-level observations, not assertions about which physical router transits a
  // prefix. Reuse a deterministic vertex of known ASes and keep walking beyond them.
  const representatives = new Map<number, string>();
  for (const v of [...vertices.values()].sort((a, b) => a.id.localeCompare(b.id))) {
    if (v.asn !== undefined && !representatives.has(v.asn)) representatives.set(v.asn, v.id);
  }
  const endpoint = (asn: number) => representatives.get(asn) ?? ensureAs(asn);
  const hop = (from: string, target: string, node: string, valid: boolean) => {
    if (from === target) return;
    const l = link("path", from, target);
    l.asSource = vertices.get(from)?.asn;
    l.asTarget = vertices.get(target)?.asn;
    if (!l.via!.includes(node)) l.via!.push(node);
    l.paths! += 1;
    if (valid) l.state = "up";
  };
  const walk = (node: string, anchor: string, text: string, valid: boolean) => {
    const segments = asPathSegments(text);
    const first = segments[0]?.asns[0];
    let from: string | undefined = vertices.get(anchor)?.asn === first ? anchor : nodeVertex(node);
    for (const segment of segments) {
      if (!segment.ordered) {
        segment.asns.forEach(endpoint);
        from = undefined;
        continue;
      }
      for (const asn of segment.asns) {
        if (from && vertices.get(from)?.asn === asn) continue; // AS prepending
        const target = endpoint(asn);
        if (from) hop(from, target, node, valid);
        from = target;
      }
    }
  };
  const anchors = new Map<string, string>();
  for (const l of [...links.values()]) {
    for (const sess of l.sessions) {
      const anchor = l.source === nodeVertex(sess.node) ? l.target : l.source;
      anchors.set(`${sess.node}|${addressKey(sess.address)}`, anchor);
      // Older nodes have only compatibility AS sequences. New reports are authoritative, including
      // partial collection metadata; never double-count their paths via both representations.
      if (pathReports.some((r) => r.node === sess.node && r.available)) continue;
      for (const path of sess.asPaths ?? []) walk(sess.node, anchor, path.join(" "), true);
    }
  }
  const seenPaths = new Map<string, { path: BgpPath; valid: boolean }>();
  for (const path of bgpPaths) {
    if (path.asPath === null) continue;
    const key = `${path.node}|${path.address}|${path.asPath}`;
    const existing = seenPaths.get(key);
    if (existing) existing.valid ||= path.valid === true;
    else seenPaths.set(key, { path, valid: path.valid === true });
  }
  for (const { path, valid } of seenPaths.values()) {
    walk(path.node, anchors.get(`${path.node}|${addressKey(path.address)}`) ?? nodeVertex(path.node), path.asPath!, valid);
  }
  for (const [node, frr] of frrOf) if ((frr.externalNeighbors ?? []).some((n) => n.enabled !== false)) vertices.get(nodeVertex(node))!.edge = true;
  // Valid retained paths support a hop; invalid-only observations do not claim an operational link.
  for (const l of links.values()) if (l.kind !== "path") l.state = stateOf(l.sessions);
  return { asNames, internalAsn, vertices: [...vertices.values()], links: [...links.values()], bgpPaths, pathReports };
}
