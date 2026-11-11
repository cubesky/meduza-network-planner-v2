// Cluster document and compilation into per-node snapshots (AGENT.md §7.2, §7.3, §8, docs/DECISIONS.md D5).
//
// Operators edit one typed cluster document. Publishing compiles it into one snapshot per node containing only
// that node's view: its own services, resolved peer data (public keys, addresses, neighbors) and the secrets
// it needs. Package-specific cross-node resolution lives in ./compilers.

import { canonicalDigest, canonicalJson } from "./canonical.js";
import type { PackageCatalog } from "./packages.js";
import { PREFIX, type CapabilitySwitch, type PackageManifest } from "./packages.js";
import { compileService, normalizeService, type CompileContext } from "./compilers/index.js";
import { connectionPolicy } from "./compilers/policy.js";
import { anycastProblems, frrOf, routingProblems, type AnycastCatalog, type BfdTimers } from "./compilers/frr.js";
import { mihomoProblems, type MihomoCatalog } from "./compilers/mihomo.js";
import { dnsLoopProblems } from "./compilers/dns.js";

export const CONFIG_SCHEMA_VERSION = 1;

export interface SyncSettings {
  heartbeatSeconds: number;
  pushKeepaliveSeconds: number;
  roamingPollSeconds: number;
  calibrationSeconds: number;
}

export interface UpdatePolicy {
  channel: string;
  automatic: boolean;
  maintenanceWindow?: string;
  timezone?: string;
  paused?: boolean;
  pinnedGroupVersion?: string;
}

export interface NodeSpec {
  displayName?: string;
  tags?: string[];
  behavior?: "static" | "roaming";
  routerId?: string;
  sync?: Partial<SyncSettings>;
  networks?: Record<string, unknown>;
  /**
   * `asn`: the node's own AS, only for nodes outside the internal network (Tinc members are in the internal AS);
   * `anycast`: the Anycast services this node runs (ids of the cluster's routing.anycast.services).
   */
  routing?: { originate?: Record<string, unknown>; asn?: number; anycast?: string[] };
  dns?: { hosts?: Array<{ hostname: string; ips: string[] }> };
  /** Keyed by package short name. Cross-node references are resolved by the package compiler. */
  services?: Record<string, Record<string, unknown>>;
  updatePolicy?: UpdatePolicy;
}

export interface ClusterDocument {
  /** Manager-only AS labels, never included in node snapshots. */
  asAddresses?: Array<{ name: string; asn: number }>;
  schemaVersion: number;
  /**
   * `anycast`: the cluster's Anycast pool and services (nodes select theirs in their routing.anycast); `bfd`: BFD
   * timers of the iBGP mesh and of eBGP sessions that enable BFD; `maximumPaths`: ECMP paths per prefix.
   */
  routing?: { internalAsn?: number; anycast?: AnycastCatalog; bfd?: BfdTimers; maximumPaths?: number; bgpFilter?: { in?: string[]; out?: string[] } };
  /** Mihomo subscriptions shared by every node (D29); a node's services.mihomo.subscription overrides the default. */
  mihomo?: MihomoCatalog;
  /** Cluster-wide DNS hosts (D58): every node receives them; a node's own entry for the same name wins. */
  dns?: { hosts?: Array<{ hostname: string; ips: string[] }> };
  /** Tinc mesh defaults (D58): cipher and digest every member uses unless it sets its own. */
  tinc?: { cipher?: string; digest?: string };
  /** MosDNS configuration shared by every node that enables services.mosdns (D58); node fields win. */
  mosdns?: Record<string, unknown>;
  defaults?: { updatePolicy?: UpdatePolicy; sync?: Partial<SyncSettings> };
  nodes: Record<string, NodeSpec>;
}

export interface Problem {
  path: string;
  message: string;
  node?: string;
  /** `warning`: shown to the operator, never blocks publishing. Absent: an error. */
  severity?: "warning";
}

export const isError = (p: Problem): boolean => p.severity !== "warning";

export interface SecretSource {
  /** Returns the value if `node` may receive it, `null` if the node itself holds it, or an error message. */
  resolve(ref: string, node: string): { kind: string; value: string } | null | { error: string };
  publicValue(ref: string): string | null;
  /** A node-held key requested but not generated yet (D28). */
  pending?(ref: string): boolean;
}

export interface NodeCapabilities {
  reported: boolean;
  /** Packages installed and registered on the node. */
  packages: Set<string>;
  capabilities: Set<string>;
  /** Container network mode the node declared in its report (`system.networkMode`). */
  networkMode?: "bridge" | "host" | "macvlan";
  /** Effective Linux capabilities of the node's planner container (`system.capabilities`); absent outside containers. */
  linuxCapabilities?: Set<string>;
}

export interface CompiledNode {
  nodeId: string;
  /** Snapshot without secret values (stored readable, used for diffs). */
  config: Record<string, unknown>;
  secrets: Record<string, { kind: string; value: string }>;
  /** Digest of config+secrets excluding the revision id: equal ⇒ nothing changed for this node. */
  contentDigest: string;
}

export interface CompileResult {
  nodes: CompiledNode[];
  problems: Problem[];
}

const ID_RE = /^[a-z0-9][a-z0-9_-]{0,62}$/;
const DEFAULT_SYNC: SyncSettings = { heartbeatSeconds: 15, pushKeepaliveSeconds: 30, roamingPollSeconds: 60, calibrationSeconds: 600 };

export function emptyDocument(): ClusterDocument {
  return { schemaVersion: CONFIG_SCHEMA_VERSION, nodes: {} };
}

/** Paths of empty `*Ref` fields inside enabled objects (a reference is required where the field exists). */
export function emptySecretRefs(value: unknown, path = "", out: string[] = []): string[] {
  if (Array.isArray(value)) value.forEach((v, i) => emptySecretRefs(v, `${path}/${i}`, out));
  else if (value && typeof value === "object") {
    const obj = value as Record<string, unknown>;
    if (obj.enabled === false) return out;
    for (const [k, v] of Object.entries(obj)) {
      if (k.endsWith("Ref") && v === "") out.push(`${path}/${k}`);
      else emptySecretRefs(v, `${path}/${k}`, out);
    }
  }
  return out;
}

/** Collects every `*Ref` string / `*Refs` array value in a service config, except inside disabled objects (the
 * plugins skip those, so a disabled tunnel imported without its key needs no secret, D58). */
export function collectSecretRefs(value: unknown, out = new Set<string>()): Set<string> {
  if (Array.isArray(value)) value.forEach((v) => collectSecretRefs(v, out));
  else if (value && typeof value === "object") {
    if ((value as Record<string, unknown>).enabled === false) return out;
    for (const [k, v] of Object.entries(value as Record<string, unknown>)) {
      if (k.endsWith("Ref") && typeof v === "string" && v) out.add(v);
      else if (k.endsWith("Refs") && Array.isArray(v)) v.filter((x): x is string => typeof x === "string" && !!x).forEach((x) => out.add(x));
      else collectSecretRefs(v, out);
    }
  }
  return out;
}

function pointerGet(value: unknown, pointer: string): unknown {
  let cur = value;
  for (const raw of pointer.split("/").slice(1)) {
    const key = raw.replace(/~1/g, "/").replace(/~0/g, "~");
    if (cur === null || typeof cur !== "object") return undefined;
    cur = (cur as Record<string, unknown>)[key];
  }
  return cur;
}

/** Same rule as `CapabilitySwitch::is_on` in crates/protocol: planner and manager must agree on what is enabled. */
export function switchOn(s: CapabilitySwitch, service: unknown): boolean {
  const v = pointerGet(service, s.pointer);
  if (s.item) {
    const item = s.item;
    return Array.isArray(v) && v.some((i) => !!i && typeof i === "object" && (i as Record<string, unknown>).enabled !== false &&
      Object.entries(item).every(([k, want]) => canonicalJson((i as Record<string, unknown>)[k] ?? null) === canonicalJson(want)));
  }
  if (v === undefined) return false;
  if (s.equals !== undefined) return canonicalJson(v) === canonicalJson(s.equals);
  if (typeof v === "boolean") return v;
  if (typeof v === "string" || Array.isArray(v)) return v.length > 0;
  if (v && typeof v === "object") return Object.keys(v).length > 0;
  return false;
}

/** Capabilities an enabled service turns on (crates/planner `enabled_capabilities`). */
export function enabledCapabilities(manifest: PackageManifest, service: unknown): string[] {
  const switches = manifest.capabilitySwitches ?? [];
  return manifest.provides.filter((cap) => switches.filter((s) => s.capability === cap).every((s) => switchOn(s, service)));
}

/**
 * D41 (§14.3.3): on a node whose planner runs in a bridge-network container, routing, TPROXY, mDNS and NAT act only
 * inside the container's namespace. Publishing stays possible (traffic may be routed into the container on purpose),
 * but the limitation is stated before publishing instead of the node later reporting "running" as if it were in
 * effect for the host/LAN.
 */
function bridgeNetworkWarnings(nodeId: string, services: Record<string, unknown>, networks: Record<string, unknown>, catalog: PackageCatalog): Problem[] {
  const out: Problem[] = [];
  const base = `/nodes/${nodeId}`;
  for (const [short, service] of Object.entries(services)) {
    const manifest = catalog.get(PREFIX + short)?.manifest;
    if (!manifest?.hostNetworkCapabilities?.length || !serviceEnabled(service as Record<string, unknown>)) continue;
    const host = enabledCapabilities(manifest, service).filter((c) => manifest.hostNetworkCapabilities!.includes(c));
    if (host.length) {
      out.push({
        path: `${base}/services/${short}`,
        node: nodeId,
        severity: "warning",
        message: `${nodeId} runs in bridge network mode: ${host.join(", ")} act only inside its container namespace; host/LAN traffic is affected only if it is routed into the container (or deploy the node with host networking)`,
      });
    }
  }
  const planner = catalog.get(PREFIX + "planner")?.manifest;
  for (const [key, cap] of [["mappings", "network.mappings"], ["portForwards", "network.port-forwards"]] as const) {
    const list = networks[key];
    const active = Array.isArray(list) && list.some((e) => !(e && typeof e === "object" && (e as Record<string, unknown>).enabled === false));
    if (active && (planner?.hostNetworkCapabilities ?? [cap]).includes(cap)) {
      out.push({
        path: `${base}/networks/${key}`,
        node: nodeId,
        severity: "warning",
        message: `${nodeId} runs in bridge network mode: ${cap} applies only to traffic that reaches its container namespace (published ports or routes into the container)`,
      });
    }
  }
  return out;
}

/**
 * Capabilities a service needs beyond the base set of every planner container (D44: CHOWN DAC_OVERRIDE FOWNER KILL
 * SETUID SETGID NET_ADMIN NET_RAW NET_BIND_SERVICE). The deployment adds them per component when the container is
 * created (run.sh, Quadlet `AddCapability=`), so a package the updater installs later (D20) can lack them until the
 * container is re-created. Not listed: avahi's SYS_CHROOT, whose absence only turns off avahi-daemon's own chroot.
 */
const CONTAINER_CAPABILITIES: Record<string, string[]> = { frr: ["SYS_ADMIN"] };

/** D43: an enabled service whose container lacks a capability it needs is refused by the node; say so before publishing. */
function containerCapabilityWarnings(nodeId: string, services: Record<string, unknown>, caps: NodeCapabilities): Problem[] {
  if (!caps.linuxCapabilities) return [];
  const out: Problem[] = [];
  for (const [short, needed] of Object.entries(CONTAINER_CAPABILITIES)) {
    const service = services[short] as Record<string, unknown> | undefined;
    if (!service || service.enabled === false) continue;
    const missing = needed.filter((c) => !caps.linuxCapabilities!.has(c));
    if (!missing.length) continue;
    out.push({
      path: `/nodes/${nodeId}/services/${short}`,
      node: nodeId,
      severity: "warning",
      message: `the planner container of ${nodeId} lacks ${missing.map((c) => `CAP_${c}`).join(", ")}, which ${short} needs: the node refuses this configuration until the container is re-created with the ${short} component (run.sh/MEDUZA_NP_COMPONENTS, or AddCapability=${missing.join(" ")} in the Quadlet unit)`,
    });
  }
  return out;
}

function serviceEnabled(service: Record<string, unknown>): boolean {
  if (service.enabled === false) return false;
  if (Array.isArray(service.instances)) {
    return service.instances.some((i) => i && typeof i === "object" && (i as Record<string, unknown>).enabled !== false) || service.enabled === true;
  }
  return true;
}

/**
 * Compiles the document for the given registered nodes. `knownNodes` are nodes that exist in the manager
 * (enrolled or pre-created); nodes in the document that are not known are reported as problems.
 */
export function compileCluster(
  doc: ClusterDocument,
  knownNodes: Set<string>,
  capabilitiesOf: (node: string) => NodeCapabilities,
  secrets: SecretSource,
  catalog: PackageCatalog,
): CompileResult {
  const problems: Problem[] = [];
  if (doc.schemaVersion !== CONFIG_SCHEMA_VERSION) {
    problems.push({ path: "/schemaVersion", message: `unsupported schemaVersion ${doc.schemaVersion}` });
    return { nodes: [], problems };
  }
  const asn = doc.routing?.internalAsn;
  if (doc.asAddresses !== undefined) {
    if (!Array.isArray(doc.asAddresses)) problems.push({ path: "/asAddresses", message: "AS addresses must be a list" });
    else {
      const seen = new Set<number>();
      doc.asAddresses.forEach((entry, i) => {
        const path = `/asAddresses/${i}`;
        if (!entry || typeof entry !== "object" || Array.isArray(entry)) {
          problems.push({ path, message: "AS address must contain a name and ASN" });
          return;
        }
        if (typeof entry.name !== "string" || !entry.name.trim() || entry.name.length > 128) problems.push({ path: `${path}/name`, message: "Name must contain 1..128 characters" });
        if (!Number.isInteger(entry.asn) || entry.asn < 1 || entry.asn > 4294967294) problems.push({ path: `${path}/asn`, message: "ASN must be 1..4294967294" });
        else if (seen.has(entry.asn)) problems.push({ path: `${path}/asn`, message: "ASN already has a name" });
        else seen.add(entry.asn);
      });
    }
  }
  if (asn !== undefined && (!Number.isInteger(asn) || asn < 1 || asn > 4294967294)) {
    problems.push({ path: "/routing/internalAsn", message: "ASN must be 1..4294967294" });
  }
  problems.push(...anycastProblems(doc), ...routingProblems(doc), ...mihomoProblems(doc));

  const nodeIds = Object.keys(doc.nodes).sort();
  for (const id of nodeIds) {
    if (!ID_RE.test(id)) problems.push({ path: `/nodes/${id}`, message: "invalid node id", node: id });
    else if (!knownNodes.has(id)) problems.push({ path: `/nodes/${id}`, message: "node is not registered in the manager", node: id });
  }

  const ctx: CompileContext = { doc, nodeIds, secrets, catalog, problems };
  const compiled: CompiledNode[] = [];

  for (const nodeId of nodeIds) {
    const spec = doc.nodes[nodeId]!;
    const base = `/nodes/${nodeId}`;
    const caps = capabilitiesOf(nodeId);
    const services: Record<string, unknown> = {};

    // D24: FRR is not written by operators; a node gets it when its other settings imply routing.
    const entries = Object.entries(spec.services ?? {});
    if (!spec.services?.frr && frrOf(nodeId, doc)) entries.push(["frr", {}]);
    for (const [short, written] of entries.sort(([a], [b]) => a.localeCompare(b))) {
      const raw = normalizeService(short, written);
      const pkg = PREFIX + short;
      const entry = catalog.get(pkg);
      const path = `${base}/services/${short}`;
      if (!entry) {
        problems.push({ path, message: `unknown package ${pkg}`, node: nodeId });
        continue;
      }
      if (entry.manifest.role !== "service-adapter" && entry.manifest.role !== "binary-provider") {
        problems.push({ path, message: `${pkg} has no service configuration`, node: nodeId });
        continue;
      }
      const enabled = serviceEnabled(raw);
      // D20: a feature may be configured before its package is on the node. The node's updater installs it (the
      // planner tells it what the configuration needs), otherwise an operator does; until then it only waits, so
      // this is a warning that never blocks publishing.
      if (enabled && !caps.packages.has(pkg)) {
        problems.push({
          path,
          node: nodeId,
          severity: "warning",
          message: caps.reported
            ? `${pkg} is not installed on ${nodeId} yet: its updater installs it, otherwise install it by hand; the settings apply then`
            : `${nodeId} has not reported its packages yet; these settings apply once ${pkg} is installed`,
        });
      }
      const policy = connectionPolicy(short, nodeId, raw);
      if (policy.length) {
        problems.push(...policy);
        continue;
      }
      if (entry.validateConfig && !entry.validateConfig(raw)) {
        for (const e of entry.validateConfig.errors ?? []) {
          problems.push({ path: `${path}${e.instancePath}`, message: e.message ?? "invalid", node: nodeId });
        }
        continue;
      }
      for (const p of emptySecretRefs(raw)) problems.push({ path: `${path}${p}`, message: "choose a secret or generate a key", node: nodeId });
      const out = compileService(short, nodeId, raw, ctx);
      if (out !== undefined) services[short] = out;
    }
    problems.push(...dnsLoopProblems(nodeId, services));
    if (caps.networkMode === "bridge") problems.push(...bridgeNetworkWarnings(nodeId, services, spec.networks ?? {}, catalog));
    problems.push(...containerCapabilityWarnings(nodeId, services, caps));

    const updatePolicy = spec.updatePolicy ?? doc.defaults?.updatePolicy;
    const config: Record<string, unknown> = {
      schemaVersion: CONFIG_SCHEMA_VERSION,
      revision: "rev-000000", // replaced at publish time
      node: {
        id: nodeId,
        ...(spec.displayName ? { displayName: spec.displayName } : {}),
        ...(spec.tags?.length ? { tags: spec.tags } : {}),
        behavior: spec.behavior ?? "static",
        ...(spec.routerId ? { routerId: spec.routerId } : {}),
        sync: { ...DEFAULT_SYNC, ...doc.defaults?.sync, ...spec.sync },
      },
      networks: { lan: [], privateLan: [], mappings: [], portForwards: [], healthPorts: [], ...spec.networks },
      services,
      ...(spec.routing || asn !== undefined
        ? { routing: { ...(asn !== undefined ? { internalAsn: asn } : {}), originate: { lan: false, privateLan: false, mappings: false, ...spec.routing?.originate } } }
        : {}),
      ...(spec.dns || doc.dns?.hosts?.length ? { dns: { hosts: mergeHosts(doc.dns?.hosts ?? [], spec.dns?.hosts ?? []) } } : {}),
      ...(updatePolicy ? { updatePolicy } : {}),
    };

    const nodeSecrets: Record<string, { kind: string; value: string }> = {};
    for (const ref of [...collectSecretRefs(services)].sort()) {
      const r = secrets.resolve(ref, nodeId);
      if (r === null) continue; // held by this node
      if ("error" in r) problems.push({ path: `${base}/services`, message: `secret ${ref}: ${r.error}`, node: nodeId });
      else nodeSecrets[ref] = r;
    }

    const schemaProblems = catalog.validate("node-config", { ...config, secrets: nodeSecrets });
    for (const p of schemaProblems) problems.push({ path: `${base}${p.path}`, message: p.message, node: nodeId });

    const { revision: _ignored, ...withoutRevision } = config;
    compiled.push({ nodeId, config, secrets: nodeSecrets, contentDigest: canonicalDigest({ ...withoutRevision, secrets: nodeSecrets }) });
  }

  return { nodes: compiled, problems };
}

/** Cluster-wide DNS hosts plus the node's own; the node's entry for a hostname replaces the cluster's. */
function mergeHosts(cluster: Array<{ hostname: string; ips: string[] }>, own: Array<{ hostname: string; ips: string[] }>) {
  const names = new Set(own.map((h) => h.hostname.toLowerCase()));
  return [...cluster.filter((h) => !names.has(h.hostname.toLowerCase())), ...own];
}

/** Full node config as delivered to the node, with revision and secrets. */
export function materialize(compiled: { config: Record<string, unknown>; secrets: Record<string, unknown> }, revision: string): Record<string, unknown> {
  const out: Record<string, unknown> = { ...compiled.config, revision };
  if (Object.keys(compiled.secrets).length) out.secrets = compiled.secrets;
  return out;
}

export function revisionId(n: number): string {
  return `rev-${String(n).padStart(6, "0")}`;
}

export function revisionNumber(rev: string): number | null {
  const m = /^rev-(\d{6,18})$/.exec(rev);
  return m ? Number(m[1]) : null;
}
