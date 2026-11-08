// Report identities determine ownership. Display names never determine paths or restart targets.
export interface RuntimeStatus {
  id: string;
  kind: string;
  state: string;
  reason?: string;
  observedAt?: string;
  [key: string]: unknown;
}

export interface InstanceTreeNode {
  key: string;
  path: string;
  label: string;
  item?: RuntimeStatus;
  children: InstanceTreeNode[];
}

export function buildInstanceTree(items: readonly RuntimeStatus[]): InstanceTreeNode[] {
  const roots: InstanceTreeNode[] = [];
  const paths = new Map<string, InstanceTreeNode>();
  const ensure = (path: string): InstanceTreeNode => {
    const known = paths.get(path);
    if (known) return known;
    const split = path.lastIndexOf("/");
    const node: InstanceTreeNode = { key: `path:${path}`, path, label: path.slice(split + 1), children: [] };
    paths.set(path, node);
    (split < 0 ? roots : ensure(path.slice(0, split)).children).push(node);
    return node;
  };
  for (const item of items.filter((i) => i.kind !== "process")) {
    const node = ensure(item.id);
    if (!node.item) node.item = item;
    else node.children.push({ key: `status:${item.kind}:${item.id}`, path: item.id, label: node.label, item, children: [] });
    if (typeof item.name === "string" && item.name.trim()) node.label = item.name;
  }
  for (const item of items.filter((i) => i.kind === "process")) {
    // Reporter appends /process to the actual supervised process id. Its explicit owner is authoritative,
    // e.g. frr/bgpd belongs to frr/main, not to a fabricated bgpd instance.
    const process = item.id.endsWith("/process") ? item.id.slice(0, -8) : item.id;
    const owner = typeof item.instance === "string" && item.instance ? item.instance : process.split("/").slice(0, 2).join("/");
    const parent = ensure(owner);
    const label = process === owner ? "" : process.startsWith(`${owner}/`) ? process.slice(owner.length + 1) : process.split("/").pop()!;
    parent.children.push({ key: `process:${item.id}`, path: process, label, item, children: [] });
  }
  const sort = (nodes: InstanceTreeNode[]) => {
    nodes.sort((a, b) => a.key < b.key ? -1 : a.key > b.key ? 1 : 0);
    for (const node of nodes) sort(node.children);
  };
  sort(roots);
  return roots;
}

/** Older node reports omit names; Manager can fill them from published (never unsaved) configuration. */
export function withWireGuardNames(items: RuntimeStatus[], config: unknown): RuntimeStatus[] {
  const interfaces = (config as { instances?: Array<{ id: string; name?: string; peers?: Array<{ id: string; name?: string }> }> } | undefined)?.instances;
  if (!Array.isArray(interfaces)) return items;
  const names = new Map<string, { name?: string; interfaceName?: string }>();
  for (const intf of interfaces) {
    const path = `wireguard/${intf.id}`;
    names.set(path, { name: intf.name });
    for (const peer of intf.peers ?? []) names.set(`${path}/${peer.id}`, { name: peer.name, interfaceName: intf.name ?? intf.id });
  }
  return items.map((item) => {
    if (!item.kind.startsWith("wireguard-")) return item;
    const fallback = names.get(item.id);
    return fallback ? { ...fallback, ...item, name: item.name || fallback.name, interfaceName: item.interfaceName || fallback.interfaceName } : item;
  });
}
