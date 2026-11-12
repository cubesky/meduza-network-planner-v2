// Routing topology (D26): a 3D force-directed graph of the BGP sessions, built by the manager from the published
// configuration and the nodes' FRR reports. The nodes of one AS form a cluster labelled with the ASN (the nodes carry
// only their name); iBGP links mesh members, eBGP links an edge router to its peer: another managed node, or an
// unmanaged router (a triangle in its own AS cluster). ASes further away are known only from AS paths: a diamond per
// AS, dotted path links in path order. "Collapse external ASes" draws every AS other than the internal one as a
// single AS vertex, hiding its routers. Click a vertex, link or cluster for its sessions and the routes learned.

import { useEffect, useMemo, useRef, useState } from "preact/hooks";
import { Badge, Button, Card, Checkbox, Loadable, PageHead, Table, t, useAsync } from "@meduza-np/ui-kit";
import { get } from "../api.ts";
import { ForceGraph3D, type GraphGroup, type GraphItem, type GraphLink, type GraphVertex } from "../graph3d.ts";

type LinkState = "up" | "partial" | "down" | "unknown";

interface Vertex {
  id: string;
  kind: "node" | "peer" | "as";
  label: string;
  asn?: number;
  address?: string;
  addresses?: string[];
  routerId?: string;
  freshness?: string;
  behavior?: string;
  edge?: boolean;
  internal?: boolean;
  /** AS vertices standing for collapsed managed nodes */
  members?: string[];
  managed?: boolean;
}

interface Session {
  node: string;
  neighbor: string;
  address: string;
  remoteAs?: number;
  bgpState: string;
  uptime?: string;
  prefixesReceived?: number;
  prefixesSent?: number;
  routes?: string[];
  routesTruncated?: boolean;
  transport?: string;
  routerId?: string;
  hostname?: string;
  asPaths?: number[][];
}

interface Link {
  id: string;
  kind: "ibgp" | "ebgp" | "path";
  source: string;
  target: string;
  state: LinkState;
  sessions: Session[];
  via?: string[];
  paths?: number;
}

interface Topology {
  asNames?: Record<string, string>;
  internalAsn: number | null;
  vertices: Vertex[];
  links: Link[];
}

/** An AS cluster: the managed nodes and the unmanaged routers we peer with of one AS. */
interface Cluster {
  id: string;
  asn: number;
  internal: boolean;
  /** no managed node in it: only routers we peer with */
  external: boolean;
  members: string[];
}

/** What the graph shows: the topology with external ASes optionally collapsed, and the AS clusters. */
interface View extends Topology {
  clusters: Cluster[];
}

/** Group of the nodes that run no BGP. */
const NO_BGP = "no-bgp";

const css = (name: string) => getComputedStyle(document.documentElement).getPropertyValue(name).trim() || "#888";

const STATE_TONE: Record<LinkState, "ok" | "warn" | "bad" | "idle"> = { up: "ok", partial: "warn", down: "bad", unknown: "idle" };
const stateLabel = (s: LinkState) => ({ up: t("established"), partial: t("partly established"), down: t("down"), unknown: t("no report") })[s];
const namedAs = (asn: number, names?: Record<string, string>) => names?.[asn] ? `AS${asn} ${names[asn]}` : `AS${asn}`;
const asLabel = (asn: number, internal: boolean, names?: Record<string, string>) => (internal ? t("{as} (internal)", { as: namedAs(asn, names) }) : namedAs(asn, names));

function mergeState(a: LinkState, b: LinkState): LinkState {
  if (a === b) return a;
  if (a === "unknown") return b;
  if (b === "unknown") return a;
  return a === "up" || b === "up" || a === "partial" || b === "partial" ? "partial" : "down";
}

/** Clusters the nodes by AS; with `collapse`, the nodes of every AS but the internal one become one AS vertex. */
function present(topo: Topology, collapse: boolean): View {
  const into = new Map<string, string>(); // node vertex → AS vertex it is drawn as
  const vertices: Vertex[] = [];
  for (const v of topo.vertices) {
    if (collapse && v.kind !== "as" && v.asn !== undefined && v.asn !== topo.internalAsn) into.set(v.id, `as:${v.asn}`);
    else vertices.push(v.kind === "as" && v.asn !== undefined ? { ...v, label: namedAs(v.asn, topo.asNames) } : v);
  }
  for (const [id, asId] of into) {
    let as = vertices.find((v) => v.id === asId);
    if (!as) vertices.push((as = { id: asId, kind: "as", label: namedAs(Number(asId.slice(3)), topo.asNames), asn: Number(asId.slice(3)), members: [] }));
    const original = topo.vertices.find((v) => v.id === id)!;
    as.members = [...(as.members ?? []), original.label];
    if (original.kind === "node") as.managed = true;
  }
  const at = (id: string) => into.get(id) ?? id;
  const links = new Map<string, Link>();
  for (const l of topo.links) {
    const a = at(l.source);
    const b = at(l.target);
    if (a === b) continue; // inside a collapsed AS
    const [source, target] = l.kind === "path" ? [a, b] : a.startsWith("node:") && b.startsWith("node:") ? [a, b].sort() : a.startsWith("node:") ? [a, b] : [b, a];
    const id = `${l.kind}:${source}|${target}`;
    const seen = links.get(id);
    if (seen) {
      const via = [...new Set([...(seen.via ?? []), ...(l.via ?? [])])];
      links.set(id, { ...seen, sessions: [...seen.sessions, ...l.sessions], state: mergeState(seen.state, l.state), ...(l.via ? { via, paths: (seen.paths ?? 0) + (l.paths ?? 0) } : {}) });
    }
    else links.set(id, { ...l, id, source: source!, target: target!, sessions: [...l.sessions] });
  }
  const clusters = new Map<number, Cluster>();
  for (const v of vertices) {
    if (v.kind === "as" || v.asn === undefined) continue;
    const c = clusters.get(v.asn) ?? { id: `asn:${v.asn}`, asn: v.asn, internal: v.asn === topo.internalAsn, external: true, members: [] };
    c.members.push(v.id);
    if (v.kind === "node") c.external = false;
    clusters.set(v.asn, c);
  }
  return { asNames: topo.asNames, internalAsn: topo.internalAsn, vertices, links: [...links.values()], clusters: [...clusters.values()].sort((a, b) => a.asn - b.asn) };
}

/** Graph styling from the theme tokens (re-read when the theme changes). */
function toGraph(view: View, show: { ibgp: boolean; ebgp: boolean; paths: boolean }): { vertices: GraphVertex[]; links: GraphLink[]; groups: GraphGroup[] } {
  const ibgp = css("--accent");
  const ebgp = css("--ebgp");
  const fresh: Record<string, string> = { online: css("--ok"), stale: css("--warn") };
  // AS paths hang off the eBGP peers: hidden with them
  const links = view.links.filter((l) => (l.kind === "ibgp" ? show.ibgp : l.kind === "ebgp" ? show.ebgp : show.ebgp && show.paths));
  const used = new Set(links.flatMap((l) => [l.source, l.target]));
  const vertices = view.vertices
    .filter((v) => v.kind === "node" || used.has(v.id) || v.members?.length)
    .map<GraphVertex>((v) =>
      v.kind === "as"
        ? // collapsed ASes solid, ASes known only from AS paths hollow-looking (faint ring) and smaller
          { id: v.id, label: asLabel(v.asn!, !!v.internal), subtitle: view.asNames?.[v.asn!], labelPriority: 1, primary: v.managed, color: v.internal ? ibgp : ebgp, shape: "diamond", radius: v.members?.length ? 14 : 9, ...(v.members?.length ? {} : { ring: css("--faint"), dashedRing: true }) }
        : v.kind === "peer"
          ? { id: v.id, label: v.label, color: ebgp, ring: css("--faint"), shape: "triangle", radius: 10, group: `asn:${v.asn}` }
          : {
              id: v.id,
              label: v.label,
              primary: true,
              // nodes in the internal AS take the iBGP colour, nodes in other ASes the eBGP one
              color: v.asn !== undefined && v.asn !== view.internalAsn ? ebgp : ibgp,
              ring: fresh[v.freshness ?? ""] ?? css("--bad"),
              dashedRing: v.behavior === "roaming",
              // edge routers (eBGP sessions to other ASes) are squares
              shape: v.edge ? "square" : "circle",
              radius: 10,
              // nodes without BGP form a cluster of their own so they stay together instead of drifting away
              group: v.asn !== undefined ? `asn:${v.asn}` : NO_BGP,
            },
    );
  return {
    vertices,
    groups: [
      ...view.clusters.map((c) => ({ id: c.id, label: asLabel(c.asn, c.internal), subtitle: view.asNames?.[c.asn], color: c.internal ? ibgp : ebgp, primary: c.internal })),
      { id: NO_BGP, label: t("No BGP"), color: css("--faint") },
    ],
    links: links.map<GraphLink>((l) => {
      if (l.kind === "path") return { id: l.id, source: l.source, target: l.target, direct: false, color: css("--faint"), width: 1.2, dash: [2, 5], particles: 0, length: 200 };
      const base = l.kind === "ibgp" ? ibgp : ebgp;
      const color = l.state === "down" ? css("--bad") : l.state === "unknown" ? css("--faint") : base;
      return {
        id: l.id,
        source: l.source,
        target: l.target,
        direct: true,
        color,
        width: l.kind === "ibgp" ? 1.6 : 2.2,
        dash: l.state === "up" ? undefined : l.state === "partial" ? [8, 4] : [4, 4],
        particles: l.state === "up" ? 1 : 0,
        // short iBGP links inside an AS cluster, long eBGP links between the clusters
        length: l.kind === "ibgp" ? 110 : 420,
      };
    }),
  };
}

/** Re-renders when the light/dark theme changes (data-theme attribute or the system preference). */
function useThemeTick(): number {
  const [tick, setTick] = useState(0);
  useEffect(() => {
    const bump = () => setTick((x) => x + 1);
    const mo = new MutationObserver(bump);
    mo.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    mq.addEventListener("change", bump);
    return () => {
      mo.disconnect();
      mq.removeEventListener("change", bump);
    };
  }, []);
  return tick;
}

/** Remembered per browser: whether external ASes are collapsed. */
const COLLAPSE_KEY = "mnp.topology.collapse";
const SPACING_KEY = "mnp.topology.spacing";
const loadCollapse = () => {
  try {
    return localStorage.getItem(COLLAPSE_KEY) === "1";
  } catch {
    return false;
  }
};
const loadSpacing = () => {
  try {
    const value = Number(localStorage.getItem(SPACING_KEY));
    return value >= 1 && value <= 3 ? value : 2;
  } catch { return 2; }
};

export function TopologyPage() {
  const topo = useAsync(() => get<Topology>("/api/v1/topology"), [], 10_000);
  return (
    <div class="topology-page">
      <PageHead title={t("Topology")} sub={t("BGP sessions between the nodes, from the published configuration and the nodes' FRR reports.")} />
      <Loadable state={topo}>{(data) => <TopologyView topo={data} />}</Loadable>
    </div>
  );
}

function TopologyView(props: { topo: Topology }) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const graph = useRef<ForceGraph3D | null>(null);
  const [show, setShow] = useState({ ibgp: true, ebgp: true, paths: true });
  const [collapse, setCollapse] = useState(loadCollapse);
  const [rotate, setRotate] = useState(false);
  const [spacing, setSpacing] = useState(loadSpacing);
  const [tip, setTip] = useState<{ item: GraphItem; x: number; y: number } | null>(null);
  const [selected, setSelected] = useState<GraphItem | null>(null);
  const theme = useThemeTick();
  const view = useMemo(() => present(props.topo, collapse), [props.topo, collapse]);

  useEffect(() => {
    graph.current = new ForceGraph3D(canvas.current!, {
      colors: () => ({ text: css("--text"), background: css("--surface") }),
      onHover: (item, x, y) => setTip(item ? { item, x, y } : null),
      onClick: (item) => setSelected(item),
    });
    return () => graph.current?.destroy();
  }, []);
  useEffect(() => {
    const g = toGraph(view, show);
    graph.current?.setData(g.vertices, g.links, g.groups);
  }, [view, show.ibgp, show.ebgp, show.paths, theme]);
  useEffect(() => {
    // Learning a peer's router ID can replace its address-only vertex/link. Drop obsolete interaction targets
    // instead of leaving an empty details card or tooltip for a duplicate that has just merged away.
    const exists = (item: GraphItem) => item.kind === "vertex" ? view.vertices.some((v) => v.id === item.id)
      : item.kind === "link" ? view.links.some((l) => l.id === item.id) : view.clusters.some((c) => c.id === item.id);
    setSelected((item) => item && exists(item) ? item : null);
    setTip((tip) => tip && exists(tip.item) ? tip : null);
  }, [view]);
  useEffect(() => {
    if (graph.current) graph.current.autoRotate = rotate;
  }, [rotate]);
  useEffect(() => {
    graph.current?.setSpacing(spacing);
    try { localStorage.setItem(SPACING_KEY, String(spacing)); } catch { /* per-browser convenience */ }
  }, [spacing]);
  useEffect(() => {
    if (graph.current) graph.current.highlight = selected?.id ?? null;
  }, [selected]);
  const toggleCollapse = (v: boolean) => {
    setCollapse(v);
    setSelected(null);
    try {
      localStorage.setItem(COLLAPSE_KEY, v ? "1" : "0");
    } catch {
      // per-browser convenience only
    }
  };

  const vertex = (id: string) => view.vertices.find((v) => v.id === id);
  const link = (id: string) => view.links.find((l) => l.id === id);
  const counts = { ibgp: props.topo.links.filter((l) => l.kind === "ibgp"), ebgp: props.topo.links.filter((l) => l.kind === "ebgp") };
  const up = (ls: Link[]) => ls.filter((l) => l.state === "up").length;
  const hasPaths = props.topo.links.some((l) => l.kind === "path");

  return (
    <div class="topology-view">
      <div class="graph-toolbar">
        <div class="graph-controls">
          <Checkbox label={t("iBGP")} checked={show.ibgp} onChange={(v) => setShow({ ...show, ibgp: v })} />
          <Checkbox label={t("eBGP")} checked={show.ebgp} onChange={(v) => setShow({ ...show, ebgp: v })} />
          {hasPaths && <Checkbox label={t("Remote ASes (from AS paths)")} checked={show.paths} onChange={(v) => setShow({ ...show, paths: v })} />}
          <Checkbox label={t("Collapse external ASes")} checked={collapse} onChange={toggleCollapse} />
          <Checkbox label={t("Rotate automatically")} checked={rotate} onChange={setRotate} />
        </div>
        <div class="graph-controls graph-navigation">
          <label class="graph-spacing">
            <span>{t("Cluster spacing")}</span>
            <input type="range" min="1" max="3" step="0.1" value={spacing} onInput={(e) => setSpacing(e.currentTarget.valueAsNumber)} />
            <output>{spacing.toFixed(1)}×</output>
          </label>
          <span class="spacer" />
          <Button small onClick={() => graph.current?.zoomBy(0.8)}>{t("Zoom in")}</Button>
          <Button small onClick={() => graph.current?.zoomBy(1.25)}>{t("Zoom out")}</Button>
          <Button small disabled={!selected} onClick={() => selected && graph.current?.focus(selected)}>{t("Focus selection")}</Button>
          <Button small onClick={() => graph.current?.fitGraph()}>{t("Fit graph")}</Button>
          <Button small onClick={() => graph.current?.resetView()}>{t("Reset view")}</Button>
        </div>
        <p class="graph-help muted small" id="topology-controls">{t("Drag the background to rotate; Shift-drag or right-drag to pan; scroll to zoom. Double-click a cluster to focus. Touch: pinch to zoom, two fingers to pan. Keyboard: arrows to rotate, Shift+arrows to pan, +/− to zoom, 0 to reset.")}</p>
        <p class="graph-help muted small">{t("Reset view frames configured direct connections; Fit graph includes remote AS paths. Zoom in, hover or select to show node labels.")}</p>
        <div class="graph-legend">
          <span><span class="swatch" style={{ borderColor: "var(--accent)" }} />{t("iBGP (Tinc mesh)")} · {up(counts.ibgp)}/{counts.ibgp.length}</span>
          <span><span class="swatch" style={{ borderColor: "var(--ebgp)" }} />{t("eBGP (to the remote AS)")} · {up(counts.ebgp)}/{counts.ebgp.length}</span>
          {hasPaths && <span><span class="swatch dotted" style={{ borderColor: "var(--faint)" }} />{t("AS path (learned routes)")}</span>}
          <span><span class="swatch dashed" style={{ borderColor: "var(--bad)" }} />{t("down")}</span>
          <span><span class="swatch dashed" style={{ borderColor: "var(--faint)" }} />{t("no report")}</span>
        </div>
        <div class="graph-legend">
          <span>◯ {t("AS cluster")}</span>
          <span>● {t("node (ring: freshness, dashed: roaming)")}</span>
          <span>■ {t("edge router")}</span>
          <span>▲ {t("external router (direct neighbor)")}</span>
          <span>◆ {t("remote AS (only its number is known)")}</span>
        </div>
      </div>
      <div class="graph-wrap">
        <canvas ref={canvas} tabIndex={0} aria-label={t("Interactive routing topology")} aria-describedby="topology-controls" />
        {tip && (
          <div class="graph-tip" style={{ left: `${Math.max(8, Math.min(tip.x + 14, (canvas.current?.clientWidth ?? 600) - 330))}px`, top: `${Math.max(8, Math.min(tip.y + 14, (canvas.current?.clientHeight ?? 400) - 110))}px` }}>
            {tip.item.kind === "group" && tip.item.id === NO_BGP ? (
              <div><strong>{t("No BGP")}</strong> <span class="muted">{t("Nodes that run no BGP (not Tinc mesh members, no BGP sessions).")}</span></div>
            ) : tip.item.kind === "vertex" ? (
              <VertexTip v={vertex(tip.item.id)} names={view.asNames} />
            ) : tip.item.kind === "group" ? (
              <ClusterTip names={view.asNames} c={view.clusters.find((c) => c.id === tip.item.id)} label={(id) => vertex(id)?.label ?? id} />
            ) : (
              <LinkTip l={link(tip.item.id)} label={(id) => vertex(id)?.label ?? id} />
            )}
          </div>
        )}
      </div>
      {selected && <div class="topology-details"><Details item={selected} view={view} onClose={() => setSelected(null)} /></div>}
    </div>
  );
}

function VertexTip(props: { v: Vertex | undefined; names?: Record<string, string> }) {
  const v = props.v;
  if (!v) return null;
  if (v.kind === "as") {
    return (
      <div>
        <strong>{v.label}</strong>{" "}
        <span class="muted">{v.members?.length ? t("{count} router(s) collapsed: {nodes}", { count: v.members.length, nodes: v.members.join(", ") }) : t("remote AS, known only from AS paths")}</span>
      </div>
    );
  }
  if (v.kind === "peer") {
    return (
      <div>
        <strong>{v.label}</strong> <span class="muted">{namedAs(v.asn!, props.names)} · {t("external router")}</span>
        <div class="muted mono">{[v.addresses?.join(", ") || v.address, v.routerId && v.routerId !== v.label ? t("router ID {id}", { id: v.routerId }) : null].filter(Boolean).join(" · ")}</div>
      </div>
    );
  }
  return (
    <div>
      <strong>{v.label}</strong> <span class="muted">{v.asn !== undefined ? namedAs(v.asn, props.names) : t("no BGP")}{v.edge ? ` · ${t("edge router")}` : ""}</span>
      <div class="muted">{t(v.freshness ?? "unknown")} · {t(v.behavior ?? "static")}</div>
    </div>
  );
}

function ClusterTip(props: { names?: Record<string, string>; c: Cluster | undefined; label: (id: string) => string }) {
  if (!props.c) return null;
  return (
    <div>
      <strong>{asLabel(props.c.asn, props.c.internal, props.names)}</strong>{" "}
      <span class="muted">{props.c.external ? t("{count} external router(s)", { count: props.c.members.length }) : t("{count} node(s)", { count: props.c.members.length })}</span>
      <div class="muted">{props.c.members.map(props.label).join(", ")}</div>
    </div>
  );
}

function LinkTip(props: { l: Link | undefined; label: (id: string) => string }) {
  const l = props.l;
  if (!l) return null;
  if (l.kind === "path") {
    return (
      <div>
        <strong>{t("AS path")}</strong> {props.label(l.source)} → {props.label(l.target)}
        <div class="muted">{t("Routes learned by {nodes} pass this hop; how the two ASes connect is not known.", { nodes: (l.via ?? []).join(", ") })}</div>
      </div>
    );
  }
  return (
    <div>
      <strong>{l.kind === "ibgp" ? "iBGP" : "eBGP"}</strong> {props.label(l.source)} — {props.label(l.target)} <Badge tone={STATE_TONE[l.state]}>{stateLabel(l.state)}</Badge>
      {l.sessions.map((s) => (
        <div class="muted mono">
          {s.node} → {s.address}: {s.bgpState}
          {s.prefixesReceived !== undefined && ` · ↓${s.prefixesReceived} ↑${s.prefixesSent ?? 0}`}
        </div>
      ))}
    </div>
  );
}

function Details(props: { item: GraphItem; view: View; onClose: () => void }) {
  const label = (id: string) => props.view.vertices.find((v) => v.id === id)?.label ?? id;
  const v = props.item.kind === "vertex" ? props.view.vertices.find((x) => x.id === props.item.id) : undefined;
  const cluster = props.item.kind === "group" ? props.view.clusters.find((c) => c.id === props.item.id) : undefined;
  const touches = (l: Link, ids: string[]) => ids.includes(l.source) || ids.includes(l.target);
  const links =
    props.item.kind === "vertex"
      ? props.view.links.filter((l) => touches(l, [props.item.id]))
      : cluster
        ? props.view.links.filter((l) => touches(l, cluster.members))
        : props.view.links.filter((l) => l.id === props.item.id);
  const sessions = links.flatMap((l) => l.sessions.map((s) => ({ ...s, kind: l.kind, link: l.id })));
  const pathLink = props.item.kind === "link" && links[0]?.kind === "path" ? links[0] : undefined;
  const title = cluster
    ? asLabel(cluster.asn, cluster.internal, props.view.asNames)
    : v
      ? v.label
      : pathLink
        ? `${t("AS path")} ${label(pathLink.source)} → ${label(pathLink.target)}`
        : links[0]
          ? `${links[0].kind === "ibgp" ? "iBGP" : "eBGP"} ${label(links[0].source)} — ${label(links[0].target)}`
          : "";
  const about = cluster
    ? cluster.external
      ? t("Routers of this AS that our nodes peer with: {nodes}. Its other routers are not known.", { nodes: cluster.members.map(label).join(", ") })
      : t("{count} node(s): {nodes}", { count: cluster.members.length, nodes: cluster.members.map(label).join(", ") })
    : pathLink
      ? t("Routes learned by {nodes} pass this hop; how the two ASes connect is not known.", { nodes: (pathLink.via ?? []).join(", ") })
      : v?.kind === "as"
        ? v.members?.length
          ? t("Collapsed: {nodes}. Clear “Collapse external ASes” to show them.", { nodes: v.members.join(", ") })
          : `${t("Remote AS: seen only in the AS paths of learned routes, so only its number is known, not its routers or links.")} ${t("Reached through: {names}.", { names: links.map((l) => label(l.source === v.id ? l.target : l.source)).join(", ") })}`
        : v?.kind === "peer"
          ? [t("External router in AS{asn}, not managed by this console; session address {address}.", { asn: v.asn ?? "?", address: v.addresses?.join(", ") || v.address || "—" }), v.routerId && t("Router ID {id}.", { id: v.routerId })].filter(Boolean).join(" ")
          : v
            ? v.asn !== undefined
              ? [namedAs(v.asn, props.view.asNames), v.routerId && t("Router ID {id}.", { id: v.routerId })].filter(Boolean).join(" · ")
              : t("This node runs no BGP.")
            : null;
  // the AS paths of the routes an external router sends
  const asPaths = v?.kind === "peer" ? [...new Set(sessions.flatMap((s) => s.asPaths ?? []).map((p) => p.map((a) => namedAs(a, props.view.asNames)).join(" → ")))] : [];
  return (
    <Card
      title={title}
      actions={
        <>
          {v?.kind === "node" && <Button small onClick={() => (location.hash = `/nodes/${encodeURIComponent(v.id.slice(5))}`)}>{t("Open node")}</Button>}{" "}
          <Button small onClick={props.onClose}>{t("Close")}</Button>
        </>
      }
    >
      {about && <p class="small muted">{about}</p>}
      {asPaths.length > 0 && (
        <p class="small">
          <strong>{t("AS paths of its routes:")}</strong> <span class="mono">{asPaths.join(" · ")}</span>
        </p>
      )}
      {!pathLink && !(v?.kind === "as" && !v.members?.length) && <Table
        rows={sessions as unknown as Record<string, unknown>[]}
        rowKey={(r) => `${r.link}/${r.node}/${r.neighbor}`}
        empty={t("No BGP sessions.")}
        columns={[
          { key: "kind", label: t("Type"), render: (r) => (r.kind === "ibgp" ? "iBGP" : "eBGP") },
          { key: "node", label: t("Node"), render: (r) => String(r.node) },
          { key: "address", label: t("Neighbor"), render: (r) => <span class="mono">{String(r.address)}{r.remoteAs !== undefined ? ` (${namedAs(Number(r.remoteAs), props.view.asNames)})` : ""}</span> },
          { key: "bgpState", label: t("State"), render: (r) => <Badge tone={r.bgpState === "Established" ? "ok" : r.bgpState === "not reported" ? "idle" : "bad"}>{r.bgpState === "not reported" ? t("no report") : String(r.bgpState)}</Badge> },
          { key: "uptime", label: t("Up/down for"), render: (r) => String(r.uptime ?? "—") },
          { key: "prefixesReceived", label: t("Received"), render: (r) => String(r.prefixesReceived ?? "—") },
          { key: "prefixesSent", label: t("Sent"), render: (r) => String(r.prefixesSent ?? "—") },
          {
            key: "routes",
            label: t("Routes learned"),
            render: (r) => (Array.isArray(r.routes) ? <span class="mono small">{(r.routes as string[]).join(", ") || "—"}{r.routesTruncated ? " …" : ""}</span> : "—"),
          },
        ]}
      />}
    </Card>
  );
}
