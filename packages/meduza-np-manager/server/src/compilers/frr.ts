// FRR compilation (AGENT.md §9.5, D17, D18): internal iBGP sessions between managed nodes run over the Tinc mesh,
// external eBGP sessions run over WireGuard/OpenVPN, OSPF exists only for Anycast. The iBGP neighbors of a Tinc mesh
// member are always exactly the other mesh members (D25), peering on their Tinc addresses with the mesh BFD profile;
// there is no opt-out and no other iBGP neighbor. A written `internalNeighbors` entry naming a member only tunes that
// member's session. ASN consistency is checked before publishing. Roaming members (D27) are in the mesh too; their
// sessions run without BFD on short timers, and what they only transit is the last resort.
//
// Anycast: the cluster declares a pool and its services (`routing.anycast`, one /30 each); every FRR node on the Tinc
// mesh joins the OSPF domain over its Tinc devices, and runs the services it selects in `anycastServices`.
// Roaming nodes (mobile, metered) never get BFD, and peers that are roaming nodes are marked `roaming` so their
// transit routes are de-preferred.
//
// D24: operators do not write FRR settings. A node runs FRR when its configuration implies routing: it is a Tinc mesh
// member and the cluster has an internal ASN (iBGP), a WireGuard peer or OpenVPN Site2Site instance carries a `bgp`
// session (eBGP over that connection), or it selects Anycast services (`routing.anycast` of the node). The node's
// own AS (`routing.asn`, only outside the internal network), BFD timers and ECMP paths (cluster `routing.bfd`,
// `routing.maximumPaths`) complete it. A written `services.frr` (raw JSON, earlier documents) still applies on top.

import { createHash } from "node:crypto";
import type { ClusterDocument, Problem } from "../cluster.js";
import type { CompileContext } from "./index.js";
import { tincDevice, tincMesh } from "./tinc.js";

interface FrrService {
  enabled?: boolean;
  localAsn?: number;
  maximumPaths?: number;
  bfd?: { profiles?: Array<Record<string, unknown> & { id: string }>; peers?: unknown[] };
  peeringAddress?: string;
  meshInternal?: boolean;
  meshDefaults?: { bfdProfile?: string };
  anycastServices?: string[];
  anycast?: Record<string, unknown> & { enabled?: boolean; instances?: unknown[] };
  internalNeighbors?: Array<Record<string, unknown> & { id: string; node?: string; address?: string; updateSource?: string; bfdProfile?: string }>;
  externalNeighbors?: Array<Record<string, unknown> & { id: string; address?: string; transport?: string; enabled?: boolean; bfdProfile?: string; roaming?: boolean }>;
}

/** `bgp` of a WireGuard peer or OpenVPN instance (meduza_np_protocol::bgp::BgpSession). */
export interface BgpSession {
  enabled?: boolean;
  remoteAsn: number;
  address?: string;
  accept?: string[];
  acceptDefault?: boolean;
  exportLan?: boolean;
  exportInternal?: boolean;
  exportMappings?: boolean;
  exportAccessPools?: boolean;
  exportDefaultRoute?: boolean;
  exportExtra?: string[];
  noTransit?: boolean;
  noForward?: boolean;
  bfd?: boolean;
  keepalive?: number;
  hold?: number;
  passwordRef?: string;
  localPreference?: number;
}

/** Cluster BFD timers (`routing.bfd`): iBGP over the mesh uses them unless disabled, eBGP sessions opt in. */
export interface BfdTimers {
  enabled?: boolean;
  transmitInterval?: number;
  receiveInterval?: number;
  detectMultiplier?: number;
}

/** The profile id the cluster BFD timers are compiled to. */
const BFD_PROFILE = "default";

/** Keepalive/hold of iBGP sessions with a roaming member: the hold timer replaces BFD (lab: withdrawal in ~7 s). */
export const ROAMING_TIMERS = { keepalive: 3, hold: 9 };

export interface AnycastService {
  id: string;
  description?: string;
  /** The service's /30 inside the pool; both host addresses go on the node's dummy device while healthy. */
  prefix: string;
  /** Probe of the local service (default host 127.0.0.1 — the VIPs are absent while it is unhealthy). */
  health: { kind: "tcp" | "dns" | "http"; port: number; host?: string; name?: string; path?: string };
  metric?: number;
}

export interface AnycastCatalog {
  pool: string;
  area?: string;
  services?: AnycastService[];
}

const ip4 = (a: string): number | null => {
  const m = /^(\d+)\.(\d+)\.(\d+)\.(\d+)$/.exec(a);
  if (!m || m.slice(1).some((o) => Number(o) > 255)) return null;
  return m.slice(1).reduce((acc, o) => acc * 256 + Number(o), 0);
};

/** IPv4 CIDR → [first address, prefix length], or null. */
function cidr(c: string): [number, number] | null {
  const [net, len] = c.split("/");
  const a = ip4(net ?? "");
  const bits = Number(len);
  return a === null || len === undefined || !Number.isInteger(bits) || bits < 0 || bits > 32 ? null : [a, bits];
}

/** True when IPv4 `addr` lies in the subnet of the interface address `cidr` (e.g. 10.98.0.2 in 10.98.0.1/24). */
function inSubnet(addr: string, c: string): boolean {
  const a = ip4(addr);
  const n = cidr(c);
  if (a === null || n === null) return false;
  const size = 2 ** (32 - n[1]);
  return Math.floor(a / size) === Math.floor(n[0] / size);
}

/** Same as meduza_np_protocol::ids::derive_ifname (interface names are at most 15 bytes). */
export function deriveIfname(prefix: string, id: string): string {
  const sanitized = `${prefix}${id}`.replace(/[^A-Za-z0-9_-]/g, "-");
  if (sanitized.length <= 15) return sanitized;
  return `${sanitized.slice(0, 10)}-${createHash("sha256").update(id).digest("hex").slice(0, 4)}`;
}

interface SessionSource {
  /** Document path of the `bgp` object. */
  path: string;
  /** `wireguard/<instance>` or `openvpn/<instance>` */
  transport: string;
  id: string;
  bgp: BgpSession;
  /** Neighbor address when `bgp.address` is absent: the peer's host route, or the OpenVPN remote address. */
  address?: string;
  /** WireGuard interface addresses the neighbor must be on. */
  subnets?: string[];
}

/** eBGP sessions defined on this node's external connections (enabled ones only). */
export function bgpSessions(nodeId: string, doc: ClusterDocument): SessionSource[] {
  const services = doc.nodes[nodeId]?.services ?? {};
  const out: SessionSource[] = [];
  type WgInstance = { id: string; enabled?: boolean; addresses?: string[]; peers?: Array<{ id: string; enabled?: boolean; allowedIps?: string[]; bgp?: BgpSession }> };
  const wg = services.wireguard as { enabled?: boolean; instances?: WgInstance[] } | undefined;
  if (wg?.enabled !== false) {
    for (const [i, inst] of (wg?.instances ?? []).entries()) {
      if (inst.enabled === false) continue;
      for (const [j, peer] of (inst.peers ?? []).entries()) {
        if (peer.enabled === false || !peer.bgp || peer.bgp.enabled === false) continue;
        const hosts = (peer.allowedIps ?? []).filter((c) => /\/(32|128)$/.test(c)).map((c) => c.split("/")[0]!);
        out.push({
          path: `/nodes/${nodeId}/services/wireguard/instances/${i}/peers/${j}/bgp`,
          transport: `wireguard/${inst.id}`,
          id: `${inst.id}-${peer.id}`.slice(0, 63),
          bgp: peer.bgp,
          address: hosts.find((h) => (inst.addresses ?? []).some((c) => inSubnet(h, c))) ?? hosts[0],
          subnets: inst.addresses ?? [],
        });
      }
    }
  }
  type OvpnInstance = { id: string; enabled?: boolean; kind?: string; ifconfig?: { remote?: string }; bgp?: BgpSession };
  const ovpn = services.openvpn as { enabled?: boolean; instances?: OvpnInstance[] } | undefined;
  if (ovpn?.enabled !== false) {
    for (const [i, inst] of (ovpn?.instances ?? []).entries()) {
      if (inst.enabled === false || !inst.bgp || inst.bgp.enabled === false) continue;
      out.push({ path: `/nodes/${nodeId}/services/openvpn/instances/${i}/bgp`, transport: `openvpn/${inst.id}`, id: inst.id, bgp: inst.bgp, address: inst.ifconfig?.remote });
    }
  }
  return out;
}

/** A prefix-filter rule (same syntax as meduza_np_frr::parse_filter_rule). */
export function filterRule(rule: string): boolean {
  const m = /^(permit|deny)\s+(\d+\.\d+\.\d+\.\d+\/\d+)((?:\s+(?:ge|le)\s+\d+){0,2})$/.exec(rule.trim());
  const net = m ? cidr(m[2]!) : null;
  if (!m || !net || net[0] % 2 ** (32 - net[1]) !== 0) return false;
  const bounds = [...m[3]!.matchAll(/(ge|le)\s+(\d+)/g)].map((x) => [x[1], Number(x[2])] as const);
  if (new Set(bounds.map((b) => b[0])).size !== bounds.length) return false;
  if (bounds.some(([, n]) => n < net[1] || n > 32)) return false;
  const ge = bounds.find((b) => b[0] === "ge")?.[1];
  const le = bounds.find((b) => b[0] === "le")?.[1];
  return ge === undefined || le === undefined || ge <= le;
}

function prefixRule(filter: string): { prefix: string; le?: number } | null {
  const m = /^(\d+\.\d+\.\d+\.\d+\/\d+)(?:\s+le\s+(\d+))?$/.exec(filter.trim());
  const net = m ? cidr(m[1]!) : null;
  if (!m || !net || net[0] % 2 ** (32 - net[1]) !== 0) return null;
  const le = m[2] === undefined ? undefined : Number(m[2]);
  if (le !== undefined && (le < net[1] || le > 32)) return null;
  return { prefix: m[1]!, ...(le !== undefined ? { le } : {}) };
}

/** FRR external neighbor for a session (address and checks are the caller's). */
function neighborOf(src: SessionSource, address: string): Record<string, unknown> & { id: string; address: string; roaming?: boolean } {
  const b = src.bgp;
  return {
    id: src.id,
    address,
    remoteAsn: b.remoteAsn,
    transport: src.transport,
    importPolicy: { allowDefault: !!b.acceptDefault, prefixes: (b.accept ?? []).map(prefixRule).filter((r) => r !== null) },
    exportPolicy: {
      lan: b.exportLan !== false,
      internal: !!b.exportInternal,
      mappings: !!b.exportMappings,
      accessPools: !!b.exportAccessPools,
      defaultRoute: !!b.exportDefaultRoute,
      extra: b.exportExtra ?? [],
    },
    noTransit: b.noTransit !== false,
    noForward: b.noForward !== false,
    ...(b.keepalive !== undefined && b.hold !== undefined ? { timers: { keepalive: b.keepalive, hold: b.hold } } : {}),
    ...(b.bfd ? { bfdProfile: BFD_PROFILE } : {}),
    ...(b.passwordRef ? { passwordRef: b.passwordRef } : {}),
    ...(b.localPreference !== undefined ? { localPreference: b.localPreference } : {}),
  };
}

/**
 * The node's FRR settings before cross-node resolution: its written `services.frr` (if any) over what the rest of
 * its configuration implies. Undefined when the node does no routing.
 */
export function frrOf(nodeId: string, doc: ClusterDocument): FrrService | undefined {
  const spec = doc.nodes[nodeId];
  if (!spec) return undefined;
  const own = spec.services?.frr as FrrService | undefined;
  if (own?.enabled === false) return own;
  const internal = doc.routing?.internalAsn;
  const member = !!tincMesh(spec.services?.tinc) && internal !== undefined;
  const nodeAsn = spec.routing?.asn;
  const anycast = spec.routing?.anycast ?? [];
  if (!own && !member && !anycast.length && !bgpSessions(nodeId, doc).length) return undefined;
  const out: FrrService = structuredClone(own ?? {});
  if (nodeAsn !== undefined && nodeAsn !== internal) out.localAsn ??= nodeAsn;
  out.meshInternal = member;
  if (anycast.length) out.anycastServices ??= anycast;
  if (doc.routing?.maximumPaths !== undefined) out.maximumPaths ??= doc.routing.maximumPaths;
  return out;
}

/** Cluster-level checks of the routing settings that are not package configuration (node AS, BFD timers, ECMP). */
export function routingProblems(doc: ClusterDocument): Problem[] {
  const problems: Problem[] = [];
  const int = (v: unknown, min: number, max: number) => v === undefined || (Number.isInteger(v) && (v as number) >= min && (v as number) <= max);
  const bfd = doc.routing?.bfd ?? {};
  if (!int(bfd.transmitInterval, 10, 60000)) problems.push({ path: "/routing/bfd/transmitInterval", message: "10..60000 ms" });
  if (!int(bfd.receiveInterval, 10, 60000)) problems.push({ path: "/routing/bfd/receiveInterval", message: "10..60000 ms" });
  if (!int(bfd.detectMultiplier, 2, 255)) problems.push({ path: "/routing/bfd/detectMultiplier", message: "2..255" });
  if (!int(doc.routing?.maximumPaths, 1, 64)) problems.push({ path: "/routing/maximumPaths", message: "1..64 paths" });
  for (const dir of ["in", "out"] as const) {
    for (const [i, r] of (doc.routing?.bgpFilter?.[dir] ?? []).entries()) {
      if (!filterRule(r)) problems.push({ path: `/routing/bgpFilter/${dir}/${i}`, message: "expected `permit|deny <IPv4 network> [ge N] [le N]`" });
    }
  }
  const internal = doc.routing?.internalAsn;
  const members = Object.keys(doc.nodes).filter((id) => tincMesh(doc.nodes[id]!.services?.tinc));
  if (internal === undefined && members.length > 1) {
    problems.push({ path: "/routing/internalAsn", severity: "warning", message: "the Tinc mesh members exchange their routes over iBGP once the internal ASN is set" });
  }
  for (const id of members) {
    if ((doc.nodes[id]!.services?.frr as FrrService | undefined)?.enabled === false) {
      problems.push({ path: `/nodes/${id}/services/frr/enabled`, node: id, message: "Tinc mesh members always run iBGP with each other; FRR cannot be disabled on a member" });
    }
  }
  for (const [id, spec] of Object.entries(doc.nodes)) {
    const asn = spec.routing?.asn;
    if (asn === undefined) continue;
    if (!int(asn, 1, 4294967294)) problems.push({ path: `/nodes/${id}/routing/asn`, node: id, message: "ASN must be 1..4294967294" });
    else if (asn !== internal && tincMesh(spec.services?.tinc)) {
      problems.push({ path: `/nodes/${id}/routing/asn`, node: id, message: `Tinc mesh members are in the internal AS${internal !== undefined ? ` ${internal}` : ""}; an own AS is for nodes outside the internal network` });
    }
  }
  return problems;
}

/** Cluster-level checks of the Anycast catalog (once per document, not per node). */
export function anycastProblems(doc: ClusterDocument): Problem[] {
  const cat = doc.routing?.anycast;
  if (!cat) return [];
  const problems: Problem[] = [];
  const base = "/routing/anycast";
  const pool = cidr(cat.pool ?? "");
  if (!pool || pool[0] % 2 ** (32 - pool[1]) !== 0) problems.push({ path: `${base}/pool`, message: "pool must be an IPv4 network, e.g. 10.255.0.0/24" });
  if (cat.area !== undefined && ip4(cat.area) === null && !/^\d+$/.test(cat.area)) problems.push({ path: `${base}/area`, message: "area is a dotted quad or integer" });
  const ids = new Set<string>();
  const used: number[] = [];
  for (const [i, s] of (cat.services ?? []).entries()) {
    const path = `${base}/services/${i}`;
    if (!/^[a-z0-9][a-z0-9_-]{0,62}$/.test(s.id ?? "") || ids.has(s.id)) problems.push({ path: `${path}/id`, message: "service ids are unique stable ids" });
    ids.add(s.id);
    const p = cidr(s.prefix ?? "");
    if (!p || p[1] !== 30 || p[0] % 4 !== 0) {
      problems.push({ path: `${path}/prefix`, message: "each Anycast service uses a /30 network, e.g. 10.255.0.4/30" });
    } else {
      if (pool && !inSubnet(s.prefix.split("/")[0]!, cat.pool)) problems.push({ path: `${path}/prefix`, message: `${s.prefix} is outside the pool ${cat.pool}` });
      if (used.includes(p[0])) problems.push({ path: `${path}/prefix`, message: `${s.prefix} is used by another service` });
      used.push(p[0]);
    }
    const h = s.health;
    if (!h || !["tcp", "dns", "http"].includes(h.kind) || !Number.isInteger(h.port) || h.port < 1 || h.port > 65535) {
      problems.push({ path: `${path}/health`, message: "health needs kind tcp|dns|http and a port" });
    } else if (h.kind === "dns" && !h.name) {
      problems.push({ path: `${path}/health/name`, message: "a DNS health check needs the name to resolve" });
    }
  }
  return problems;
}

/** Plugin form of a catalog service on one node. */
function anycastInstance(s: AnycastService): Record<string, unknown> {
  const host = s.health.host ?? "127.0.0.1";
  const health =
    s.health.kind === "dns"
      ? { kind: "dns", server: host, port: s.health.port, name: s.health.name }
      : s.health.kind === "http"
        ? { kind: "http", host, port: s.health.port, path: s.health.path ?? "/" }
        : { kind: "tcp", host, port: s.health.port };
  return { id: s.id, prefixes: [s.prefix], service: s.description ?? s.id, health, ...(s.metric !== undefined ? { metric: s.metric } : {}) };
}

/** Host part of every WireGuard/OpenVPN tunnel address of the cluster's roaming nodes. */
function roamingTunnelAddresses(doc: ClusterDocument): Set<string> {
  const out = new Set<string>();
  for (const spec of Object.values(doc.nodes)) {
    if (spec.behavior !== "roaming") continue;
    const wg = (spec.services?.wireguard as { instances?: Array<{ addresses?: string[] }> } | undefined)?.instances ?? [];
    for (const i of wg) for (const a of i.addresses ?? []) out.add(a.split("/")[0]!);
    const ovpn = (spec.services?.openvpn as { instances?: Array<{ ifconfig?: { local?: string } }> } | undefined)?.instances ?? [];
    for (const i of ovpn) if (i.ifconfig?.local) out.add(i.ifconfig.local.split("/")[0]!);
  }
  return out;
}

export function compileFrr(nodeId: string, service: Record<string, unknown>, ctx: CompileContext): Record<string, unknown> {
  if (service.enabled === false) return structuredClone(service);
  // `service` is the written services.frr, or {} when the node's other settings imply FRR (cluster.ts)
  const written = !!ctx.doc.nodes[nodeId]?.services?.frr;
  const spec = ctx.doc.nodes[nodeId]!;
  const out = frrOf(nodeId, { ...ctx.doc, nodes: { ...ctx.doc.nodes, [nodeId]: { ...spec, services: { ...spec.services, frr: service } } } })!;
  const base = `/nodes/${nodeId}/services/frr`;
  // §9.5: every routing node has a router id; the plugin refuses the whole revision without one, so catch it here
  const routerId = (service.routerId as string | undefined) ?? spec.routerId;
  if (routerId === undefined) {
    ctx.problems.push({ path: `/nodes/${nodeId}/routerId`, node: nodeId, message: "this node runs FRR (BGP/Anycast): set its routerId, an IPv4 address such as its mesh or tunnel address" });
  } else if (ip4(routerId) === null) {
    ctx.problems.push({ path: service.routerId !== undefined ? `${base}/routerId` : `/nodes/${nodeId}/routerId`, node: nodeId, message: "routerId is an IPv4 address" });
  }
  const internal = ctx.doc.routing?.internalAsn;
  const roamingNode = ctx.doc.nodes[nodeId]?.behavior === "roaming";
  // (a node AS that conflicts with Tinc membership is reported by routingProblems)
  if (written && internal !== undefined && out.localAsn !== undefined && out.localAsn !== internal && out.meshInternal) {
    ctx.problems.push({ path: `${base}/localAsn`, node: nodeId, message: `iBGP members must use the internal ASN ${internal}` });
  }
  // Written iBGP neighbors may only tune the session with another mesh member.
  const tuning = out.internalNeighbors ?? [];
  const meshAddr = (node: string) => tincMesh(ctx.doc.nodes[node]?.services?.tinc)?.address?.split("/")[0];
  for (const [i, n] of tuning.entries()) {
    if (!out.meshInternal || !n.node || n.node === nodeId || !tincMesh(ctx.doc.nodes[n.node]?.services?.tinc)) {
      ctx.problems.push({ path: `${base}/internalNeighbors/${i}`, node: nodeId, message: "iBGP neighbors are always the other Tinc mesh members, and only them" });
    }
  }
  const services = ctx.doc.nodes[nodeId]?.services ?? {};
  const mesh = tincMesh(services.tinc);
  const tinc = mesh ? [mesh] : [];

  // iBGP over the internal Tinc mesh: the peering address is this node's Tinc address.
  if (out.meshInternal) {
    const own = meshAddr(nodeId);
    if (out.peeringAddress !== undefined && out.peeringAddress !== own) {
      ctx.problems.push({
        path: `${base}/peeringAddress`,
        node: nodeId,
        message: `iBGP runs over the internal Tinc mesh: peeringAddress must be this node's Tinc address (${tinc.map((t) => t.address).filter(Boolean).join(", ") || "no Tinc mesh on this node"})`,
      });
    }
    out.peeringAddress = own;
  }

  // eBGP over an external connection of this node (WireGuard/OpenVPN instance).
  const roamingPeers = roamingTunnelAddresses(ctx.doc);
  for (const [i, n] of (out.externalNeighbors ?? []).entries()) {
    if (n.enabled === false) continue;
    const nb = `${base}/externalNeighbors/${i}`;
    // A peer that is a roaming node of the cluster: last-resort transit, no BFD.
    if (n.address && roamingPeers.has(n.address)) n.roaming = true;
    if (n.bfdProfile && (roamingNode || n.roaming)) {
      ctx.problems.push({ path: `${nb}/bfdProfile`, node: nodeId, message: "BFD is not used on sessions with a roaming node (it costs bandwidth)" });
    }
    if (!n.transport) continue; // the plugin schema/validation requires a transport
    const [pkg, inst] = n.transport.split("/");
    const path = `${nb}/transport`;
    const list = ((services[pkg ?? ""] as { instances?: Array<{ id: string; addresses?: string[]; enabled?: boolean }> } | undefined)?.instances ?? []);
    const t = list.find((x) => x.id === inst && x.enabled !== false);
    if (pkg !== "wireguard" && pkg !== "openvpn") {
      ctx.problems.push({ path, node: nodeId, message: "eBGP runs over WireGuard or OpenVPN; the Tinc mesh carries iBGP only" });
    } else if (!t) {
      ctx.problems.push({ path, node: nodeId, message: `${n.transport} is not an enabled ${pkg} instance of this node` });
    } else if (pkg === "wireguard" && n.address && !(t.addresses ?? []).some((c) => inSubnet(n.address!, c))) {
      ctx.problems.push({ path: `${nb}/address`, node: nodeId, message: `${n.address} is not on ${n.transport} (${(t.addresses ?? []).join(", ")})` });
    }
  }
  // eBGP sessions set on this node's WireGuard peers and OpenVPN instances.
  const ids = new Set((out.externalNeighbors ?? []).map((n) => n.id));
  for (const src of bgpSessions(nodeId, ctx.doc)) {
    const address = src.bgp.address ?? src.address;
    if (ids.has(src.id)) {
      ctx.problems.push({ path: src.path, node: nodeId, message: `another BGP neighbor of this node is called ${src.id}` });
      continue;
    }
    ids.add(src.id);
    if (!address) {
      ctx.problems.push({
        path: `${src.path}/address`,
        node: nodeId,
        message: src.transport.startsWith("wireguard/") ? "set the neighbor address (the peer's allowedIps have no host route to take it from)" : "set the neighbor address (the instance has no ifconfig remote address)",
      });
      continue;
    }
    if (src.subnets && !src.subnets.some((c) => inSubnet(address, c))) {
      ctx.problems.push({ path: `${src.path}/address`, node: nodeId, message: `${address} is not on ${src.transport} (${src.subnets.join(", ") || "no addresses"})` });
    }
    const n = neighborOf(src, address);
    if (roamingPeers.has(address)) n.roaming = true;
    if (src.bgp.bfd && (roamingNode || n.roaming)) {
      ctx.problems.push({ path: `${src.path}/bfd`, node: nodeId, message: "BFD is not used on sessions with a roaming node (it costs bandwidth)" });
    }
    (out.externalNeighbors ??= []).push(n);
  }

  // The cluster's prefix filters (D58) apply to every eBGP session that has none of its own.
  const filter = ctx.doc.routing?.bgpFilter;
  for (const n of out.externalNeighbors ?? []) {
    if (filter?.in?.length && !(n.filterIn as unknown[] | undefined)?.length) n.filterIn = [...filter.in];
    if (filter?.out?.length && !(n.filterOut as unknown[] | undefined)?.length) n.filterOut = [...filter.out];
  }

  // Cluster BFD timers: the iBGP mesh uses them (unless switched off or this node roams), eBGP sessions opt in.
  // A written services.frr may name its own mesh BFD profile.
  const timers = ctx.doc.routing?.bfd ?? {};
  if (out.meshInternal && !roamingNode && timers.enabled !== false) (out.meshDefaults ??= {}).bfdProfile ??= BFD_PROFILE;
  const md = out.meshDefaults ?? {};
  const usesProfile = md.bfdProfile === BFD_PROFILE || (out.externalNeighbors ?? []).some((n) => n.bfdProfile === BFD_PROFILE);
  if (usesProfile && !(out.bfd?.profiles ?? []).some((p) => p.id === BFD_PROFILE)) {
    out.bfd = {
      ...out.bfd,
      profiles: [
        ...(out.bfd?.profiles ?? []),
        { id: BFD_PROFILE, transmitInterval: timers.transmitInterval ?? 300, receiveInterval: timers.receiveInterval ?? 300, detectMultiplier: timers.detectMultiplier ?? 3 },
      ],
    };
  }
  if (roamingNode && (md.bfdProfile || tuning.some((n) => n.bfdProfile))) {
    ctx.problems.push({ path: `${base}/meshDefaults/bfdProfile`, node: nodeId, message: "BFD is not used on a roaming node (it costs bandwidth)" });
  }

  // Every other Tinc mesh member, always (D25).
  const neighbors: NonNullable<FrrService["internalNeighbors"]> = [];
  if (out.meshInternal) {
    if (!out.peeringAddress) {
      ctx.problems.push({ path: `/nodes/${nodeId}/services/tinc/address`, node: nodeId, message: "iBGP over the Tinc mesh needs this node's Tinc address" });
    } else {
      for (const other of ctx.nodeIds) {
        if (other === nodeId || !tincMesh(ctx.doc.nodes[other]?.services?.tinc)) continue;
        const address = meshAddr(other);
        if (!address) {
          ctx.problems.push({ path: base, node: nodeId, message: `mesh member ${other} has no Tinc address` });
          continue;
        }
        const { id: _id, node: _node, address: _address, updateSource: _src, ...tuned } = tuning.find((n) => n.node === other) ?? {};
        const roamingPeer = ctx.doc.nodes[other]?.behavior === "roaming";
        // no BFD with or on a roaming member: the short hold timer notices a lost peer instead
        const session = roamingPeer || roamingNode ? { timers: ROAMING_TIMERS, ...(roamingPeer ? { roaming: true } : {}) } : md.bfdProfile ? { bfdProfile: md.bfdProfile } : {};
        neighbors.push({ id: other.replace(/_/g, "-"), node: other, address, updateSource: out.peeringAddress, ...session, ...tuned });
      }
    }
  }
  out.internalNeighbors = neighbors;

  // Anycast: the OSPF domain is the Tinc mesh. Every member joins it (so traffic for the pool entering anywhere
  // reaches the nearest healthy service) and runs the services it selected.
  const cat = ctx.doc.routing?.anycast;
  const catalog = new Map((cat?.services ?? []).map((s) => [s.id, s]));
  const selected = out.anycastServices ?? [];
  delete out.anycastServices;
  // the selection is the node's routing.anycast unless a written services.frr sets it
  const selPath = (service as FrrService).anycastServices ? `${base}/anycastServices` : `/nodes/${nodeId}/routing/anycast`;
  for (const [i, id] of selected.entries()) {
    if (!catalog.has(id)) ctx.problems.push({ path: `${selPath}/${i}`, node: nodeId, message: `no Anycast service ${id} in routing.anycast.services` });
  }
  if (selected.length && (roamingNode || !tinc.length)) {
    ctx.problems.push({
      path: selPath,
      node: nodeId,
      message: roamingNode ? "a roaming node is not part of the Anycast (OSPF over Tinc) domain" : "Anycast runs over the Tinc mesh (OSPF): this node has no enabled Tinc instance",
    });
  }
  if (cat && catalog.size && tinc.length && !roamingNode) {
    const own = out.anycast?.enabled ? (out.anycast.instances ?? []) : [];
    out.anycast = {
      enabled: true,
      pool: cat.pool,
      area: cat.area ?? "0.0.0.0",
      interfaces: tinc.map((t) => ({ name: tincDevice(t), ...(md.bfdProfile ? { bfdProfile: md.bfdProfile } : {}) })),
      instances: [...own, ...selected.filter((id) => catalog.has(id)).map((id) => anycastInstance(catalog.get(id)!))],
    };
  }
  return out as Record<string, unknown>;
}
