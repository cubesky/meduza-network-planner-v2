// A small 3D force-directed graph on a 2D canvas (D26), without a WebGL dependency: the topology has tens of vertices,
// so an O(n²) simulation and painter's-algorithm drawing are plenty, and the console keeps working offline.
//
// Simulation (d3-force style): many-body repulsion, link springs, centering, velocity decay, cooling alpha.
// View: perspective camera orbiting the origin. Dragging a vertex moves it in the screen plane at its depth (it is
// pinned while held); background drag orbits with a fixed up direction, Shift/right drag pans, and the wheel zooms
// towards the pointer. The camera fits during initial layout only; manual navigation is never auto-corrected.
// Groups (the nodes of one AS) are drawn as translucent bubbles labelled with the group's name; members are pulled
// towards their group's centre and bubbles collide as whole bodies, so every AS forms a cluster of its own.

export interface GraphVertex {
  id: string;
  label: string;
  subtitle?: string;
  /** AS landmark label: always visible and avoids the fixed node/router panels. */
  labelPriority?: number;
  /** A managed node (also retained by a collapsed managed AS): root of the outward layout. */
  primary?: boolean;
  /** fill colour */
  color: string;
  /** ring colour (e.g. freshness), optional */
  ring?: string;
  dashedRing?: boolean;
  shape: "circle" | "diamond" | "square" | "triangle";
  radius: number;
  /** id of the group (cluster) the vertex belongs to */
  group?: string;
}

export interface GraphGroup {
  id: string;
  label: string;
  subtitle?: string;
  color: string;
  primary?: boolean;
}

export interface GraphLink {
  id: string;
  source: string;
  target: string;
  color: string;
  width: number;
  dash?: number[];
  /** particles per direction; 0 for none */
  particles: number;
  /** preferred length */
  length: number;
  /** spring strength factor (default 1); weak links only nudge their ends */
  strength?: number;
  /** Configured session, including down/unknown sessions; false for learned AS-path edges. */
  direct?: boolean;
}

interface SimVertex extends GraphVertex {
  x: number;
  y: number;
  z: number;
  vx: number;
  vy: number;
  vz: number;
  pinned: boolean;
}

export type GraphItem = { kind: "vertex"; id: string } | { kind: "link"; id: string } | { kind: "group"; id: string };

export interface GraphOptions {
  onHover?: (item: GraphItem | null, x: number, y: number) => void;
  onClick?: (item: GraphItem | null) => void;
  /** text colour for labels and the fog (background) colour */
  colors: () => { text: string; background: string };
}

const FOCAL = 600;
const NEAR = 20;
const MAX_PITCH = Math.PI / 3;
const DEFAULT_PITCH = -0.15;
type Point = { x: number; y: number; z: number };
type Drag =
  | { kind: "vertex"; pointer: number; v: SimVertex; depth: number; x: number; y: number; offsetX: number; offsetY: number; moved: boolean }
  | { kind: "rotate" | "pan"; pointer: number; x: number; y: number; startX: number; startY: number; moved: boolean }
  | { kind: "gesture"; x: number; y: number; distance: number; moved: true };

export class ForceGraph3D {
  private ctx: CanvasRenderingContext2D;
  private vertices = new Map<string, SimVertex>();
  private links: GraphLink[] = [];
  private groups: GraphGroup[] = [];
  private alpha = 1;
  private alphaTarget = 0;
  private yaw = 0;
  private pitch = DEFAULT_PITCH;
  private distance = 520;
  private target: Point = { x: 0, y: 0, z: 0 };
  private panX = 0;
  private panY = 0;
  private spacing = 2;
  private anchors = new Map<string, Point>();
  private fitting = true;
  private userView = false;
  private viewFrame: "all" | "direct" | string[] | null = "direct";
  private labelSlots = new Map<string, number>();
  private groupLabelSlots = new Map<string, number>();
  private labelHits: Array<{ item: GraphItem; depth: number; x: number; y: number; w: number; h: number }> = [];
  private detailedLabels = new Set<string>();
  private frame = 0;
  private raf = 0;
  private width = 0;
  private height = 0;
  private drag: Drag | null = null;
  private pointers = new Map<number, { x: number; y: number }>();
  private hover: GraphItem | null = null;
  private interacted = 0;
  private resize: ResizeObserver;
  autoRotate = false;
  highlight: string | null = null;

  constructor(private canvas: HTMLCanvasElement, private opts: GraphOptions) {
    this.ctx = canvas.getContext("2d")!;
    this.resize = new ResizeObserver(() => this.fit());
    this.resize.observe(canvas);
    this.fit();
    canvas.addEventListener("pointerdown", this.onDown);
    canvas.addEventListener("pointermove", this.onMove);
    canvas.addEventListener("pointerup", this.onUp);
    canvas.addEventListener("pointercancel", this.onUp);
    canvas.addEventListener("lostpointercapture", this.onUp);
    canvas.addEventListener("pointerleave", this.onLeave);
    canvas.addEventListener("wheel", this.onWheel, { passive: false });
    canvas.addEventListener("dblclick", this.onDblClick);
    canvas.addEventListener("contextmenu", this.onContextMenu);
    canvas.addEventListener("keydown", this.onKey);
    const loop = () => {
      this.tick();
      this.draw();
      this.raf = requestAnimationFrame(loop);
    };
    this.raf = requestAnimationFrame(loop);
  }

  destroy(): void {
    this.cancelDrag();
    cancelAnimationFrame(this.raf);
    this.resize.disconnect();
    this.canvas.removeEventListener("pointerdown", this.onDown);
    this.canvas.removeEventListener("pointermove", this.onMove);
    this.canvas.removeEventListener("pointerup", this.onUp);
    this.canvas.removeEventListener("pointercancel", this.onUp);
    this.canvas.removeEventListener("lostpointercapture", this.onUp);
    this.canvas.removeEventListener("pointerleave", this.onLeave);
    this.canvas.removeEventListener("wheel", this.onWheel);
    this.canvas.removeEventListener("dblclick", this.onDblClick);
    this.canvas.removeEventListener("contextmenu", this.onContextMenu);
    this.canvas.removeEventListener("keydown", this.onKey);
  }

  /** Replaces the graph; vertices that stay keep their position, new ones start next to a neighbour. */
  setData(vertices: GraphVertex[], links: GraphLink[], groups: GraphGroup[] = []): void {
    vertices = [...vertices].sort((a, b) => a.id.localeCompare(b.id));
    const ids = new Set(vertices.map((v) => v.id));
    links = links.filter((l) => ids.has(l.source) && ids.has(l.target)).sort((a, b) => a.id.localeCompare(b.id));
    const previousLinks = new Map(this.links.map((l) => [l.id, l]));
    const next = new Map<string, SimVertex>();
    const structural =
      vertices.length !== this.vertices.size ||
      vertices.some((v) => !this.vertices.has(v.id) || this.vertices.get(v.id)!.group !== v.group || this.vertices.get(v.id)!.primary !== v.primary) ||
      links.length !== this.links.length ||
      links.some((l) => {
        const old = previousLinks.get(l.id);
        return !old || l.source !== old.source || l.target !== old.target || l.length !== old.length || l.strength !== old.strength || l.direct !== old.direct;
      });
    const previous = this.vertices;
    this.groups = [...groups].sort((a, b) => a.id.localeCompare(b.id));
    for (const v of vertices) {
      const old = this.vertices.get(v.id);
      // Reports/theme changes must keep the actual object being dragged, not copy it out from under the pointer.
      next.set(v.id, old ? Object.assign(old, v) : { ...v, x: 0, y: 0, z: 0, vx: 0, vy: 0, vz: 0, pinned: false });
    }
    this.vertices = next;
    this.links = links;
    this.updateAnchors();
    const indices = new Map<string, number>();
    const sizes = new Map<string, number>();
    for (const v of next.values()) sizes.set(this.bodyKey(v), (sizes.get(this.bodyKey(v)) ?? 0) + 1);
    for (const v of next.values()) {
      const key = this.bodyKey(v);
      const index = indices.get(key) ?? 0;
      indices.set(key, index + 1);
      if (previous.has(v.id)) continue;
      const existing = [...previous.values()].filter((o) => v.group && o.group === v.group);
      const base = existing.length ? centroid(existing) : this.anchors.get(key)!;
      const count = sizes.get(key)!;
      const direction = spherePoint(index, count, hash(key) * Math.PI * 2);
      const radius = count > 1 ? 30 + Math.cbrt(count) * 22 : 0;
      Object.assign(v, { x: base.x + direction.x * radius, y: base.y + direction.y * radius, z: base.z + direction.z * radius });
    }
    if (this.drag?.kind === "vertex" && !next.has(this.drag.v.id)) this.cancelDrag();
    for (const id of this.labelSlots.keys()) if (!next.has(id)) this.labelSlots.delete(id);
    for (const id of this.detailedLabels) if (!next.has(id)) this.detailedLabels.delete(id);
    for (const id of this.groupLabelSlots.keys()) if (!this.groups.some((g) => g.id === id)) this.groupLabelSlots.delete(id);
    if (Array.isArray(this.viewFrame) && !this.viewFrame.some((id) => next.has(id))) {
      this.viewFrame = "direct";
      this.userView = false;
    }
    if (structural) this.alpha = Math.max(this.alpha, 0.8);
    if (!previous.size || structural && (!this.userView || this.viewFrame !== null)) this.fitting = true;
  }

  resetView(): void {
    this.yaw = 0;
    this.pitch = DEFAULT_PITCH;
    this.viewFrame = "direct";
    this.userView = false;
    this.fitting = this.alpha >= 0.002;
    this.labelSlots.clear();
    this.frameVertices(this.framedVertices());
  }

  fitGraph(): void {
    this.fitting = this.alpha >= 0.002;
    this.userView = true;
    this.viewFrame = "all";
    this.labelSlots.clear();
    this.frameVertices([...this.vertices.values()]);
  }

  focus(item: GraphItem): void {
    let vertices: SimVertex[];
    if (item.kind === "group") vertices = [...this.vertices.values()].filter((v) => v.group === item.id);
    else if (item.kind === "link") {
      const link = this.links.find((l) => l.id === item.id);
      vertices = link ? [this.vertices.get(link.source)!, this.vertices.get(link.target)!] : [];
    } else vertices = this.vertices.has(item.id) ? [this.vertices.get(item.id)!] : [];
    if (!vertices.length) return;
    this.fitting = false;
    this.userView = true;
    this.viewFrame = vertices.map((v) => v.id);
    this.labelSlots.clear();
    this.frameVertices(vertices);
  }

  setSpacing(value: number): void {
    const next = Math.max(1, Math.min(3, value));
    if (this.spacing === next) return;
    this.spacing = next;
    this.updateAnchors();
    this.alpha = Math.max(this.alpha, 0.9);
    this.userView = false;
    this.viewFrame = this.viewFrame ?? "direct";
    this.fitting = true;
  }

  zoomBy(factor: number, x = this.width / 2, y = this.height / 2): void {
    const next = Math.max(80, Math.min(100_000, this.distance * factor));
    const ratio = this.distance / next;
    this.panX = x - this.width / 2 - (x - this.width / 2 - this.panX) * ratio;
    this.panY = y - this.height / 2 - (y - this.height / 2 - this.panY) * ratio;
    this.distance = next;
    this.manualView();
  }

  // ---- simulation ---------------------------------------------------------------------------------------------

  private tick(): void {
    this.frame++;
    if (this.autoRotate && !this.drag && performance.now() - this.interacted > 2500) this.yaw += 0.0016;
    if (this.drag && this.drag.kind !== "vertex") return;
    this.alpha += (this.alphaTarget - this.alpha) * 0.0228;
    if (this.alpha < 0.002) {
      this.fitting = false;
      return;
    }
    const vs = [...this.vertices.values()];
    const a = this.alpha;
    // many-body repulsion
    for (let i = 0; i < vs.length; i++) {
      for (let j = i + 1; j < vs.length; j++) {
        const p = vs[i]!;
        const q = vs[j]!;
        // only within a cluster: clusters are kept apart as whole bodies by the collision below (vertex repulsion
        // across clusters would let one cluster's nodes ring another instead of moving it)
        if (this.bodyKey(p) !== this.bodyKey(q)) continue;
        let dx = q.x - p.x, dy = q.y - p.y, dz = q.z - p.z;
        let l = dx * dx + dy * dy + dz * dz;
        if (l < 1) {
          const direction = stableDirection(`${p.id}|${q.id}`);
          dx = direction.x; dy = direction.y; dz = direction.z;
          l = 1;
        }
        const distance = Math.sqrt(l);
        const collision = Math.max(0, p.radius + q.radius + 28 - distance) * 0.25;
        const w = -a * (500 / Math.max(l, 25) + collision / distance);
        p.vx += dx * w; p.vy += dy * w; p.vz += dz * w;
        q.vx -= dx * w; q.vy -= dy * w; q.vz -= dz * w;
      }
    }
    // link springs
    const degree = new Map<string, number>();
    for (const l of this.links) {
      degree.set(l.source, (degree.get(l.source) ?? 0) + 1);
      degree.set(l.target, (degree.get(l.target) ?? 0) + 1);
    }
    for (const l of this.links) {
      const s = this.vertices.get(l.source)!;
      const t = this.vertices.get(l.target)!;
      const dx = t.x + t.vx - s.x - s.vx, dy = t.y + t.vy - s.y - s.vy, dz = t.z + t.vz - s.z - s.vz;
      const d = Math.sqrt(dx * dx + dy * dy + dz * dz) || 1;
      // Cross-AS links describe connectivity, not a demand to pull the routers out of their own clusters.
      // Apply a weak translation to both whole bodies, leaving their local arrangement intact.
      if (this.bodyKey(s) !== this.bodyKey(t)) {
        const sa = this.anchors.get(this.bodyKey(s))!, ta = this.anchors.get(this.bodyKey(t))!;
        const length = Math.max(l.length * this.spacing, Math.hypot(sa.x - ta.x, sa.y - ta.y, sa.z - ta.z) * 0.9);
        const k = ((d - length) / d) * a * 0.012 * (l.strength ?? 1) / Math.max(degree.get(l.source)!, degree.get(l.target)!);
        const left = this.bodyMembers(s), right = this.bodyMembers(t);
        for (const v of left) { v.vx += dx * k; v.vy += dy * k; v.vz += dz * k; }
        for (const v of right) { v.vx -= dx * k; v.vy -= dy * k; v.vz -= dz * k; }
        continue;
      }
      const strength = (l.strength ?? 1) / Math.min(degree.get(l.source)!, degree.get(l.target)!);
      const k = ((d - l.length) / d) * a * strength;
      const ds = degree.get(l.source)!, dt = degree.get(l.target)!;
      const bias = ds / (ds + dt);
      t.vx -= dx * k * bias; t.vy -= dy * k * bias; t.vz -= dz * k * bias;
      s.vx += dx * k * (1 - bias); s.vy += dy * k * (1 - bias); s.vz += dz * k * (1 - bias);
    }
    // group cohesion: members drift towards their group's centre
    for (const [, members] of this.members()) {
      if (members.length < 2) continue;
      const c = centroid(members);
      for (const v of members) {
        v.vx += (c.x - v.x) * 0.15 * a;
        v.vy += (c.y - v.y) * 0.15 * a;
        v.vz += (c.z - v.z) * 0.15 * a;
      }
    }
    // Every AS is a spherical force body. Cohesion, anchor attraction, charge and collision all act in XYZ;
    // a large cluster and a small one translate equally, without flattening their members or pulling one out.
    const bodies = [
      ...this.bubbles().map((b) => ({ members: this.members().get(b.group.id)!, x: b.x, y: b.y, z: b.z, r: b.r })),
      ...vs.filter((v) => !v.group).map((v) => ({ members: [v], x: v.x, y: v.y, z: v.z, r: v.radius * 2.4 + 18 })),
    ];
    for (let i = 0; i < bodies.length; i++) {
      const p = bodies[i]!;
      const anchor = this.anchors.get(this.bodyKey(p.members[0]!))!;
      for (const v of p.members) {
        v.vx += (anchor.x - p.x) * 0.035 * a;
        v.vy += (anchor.y - p.y) * 0.035 * a;
        v.vz += (anchor.z - p.z) * 0.035 * a;
      }
      for (let j = i + 1; j < bodies.length; j++) {
        const q = bodies[j]!;
        let dx = q.x - p.x, dy = q.y - p.y, dz = q.z - p.z;
        let d = Math.hypot(dx, dy, dz);
        if (d < 0.01) {
          const direction = stableDirection(`${this.bodyKey(p.members[0]!)}|${this.bodyKey(q.members[0]!)}`);
          dx = direction.x; dy = direction.y; dz = direction.z; d = 1;
        }
        const overlap = p.r + q.r + 140 * this.spacing - d;
        const k = (Math.max(0, overlap) * 0.35 / d + 1800 / Math.max(d * d, 25)) * a;
        for (const v of p.members) {
          v.vx -= dx * k;
          v.vy -= dy * k;
          v.vz -= dz * k;
        }
        for (const v of q.members) {
          v.vx += dx * k;
          v.vy += dy * k;
          v.vz += dz * k;
        }
      }
    }
    // Integrate with velocity decay. Do not recenter the simulation under the user's pointer/camera.
    for (const v of vs) {
      if (v.pinned) {
        v.vx = v.vy = v.vz = 0;
      } else {
        v.vx *= 0.6; v.vy *= 0.6; v.vz *= 0.6;
        v.x += v.vx; v.y += v.vy; v.z += v.vz;
      }
    }
    if (this.fitting && !this.drag) {
      this.frameVertices(this.framedVertices(), 0.14);
    }
  }

  private bodyKey(v: SimVertex): string {
    return v.group ? `group:${v.group}` : `vertex:${v.id}`;
  }

  private bodyMembers(v: SimVertex): SimVertex[] {
    return v.group ? [...this.vertices.values()].filter((o) => o.group === v.group) : [v];
  }

  private updateAnchors(): void {
    const bodies = new Map<string, SimVertex[]>();
    for (const v of this.vertices.values()) {
      const key = this.bodyKey(v);
      bodies.set(key, [...(bodies.get(key) ?? []), v]);
    }
    const primary = new Set(this.groups.filter((g) => g.primary).map((g) => `group:${g.id}`));
    for (const v of this.vertices.values()) if (v.primary) primary.add(this.bodyKey(v));
    const keys = [...bodies.keys()].sort((a, b) => Number(primary.has(b)) - Number(primary.has(a)) || a.localeCompare(b));
    const maxRadius = Math.max(65, ...[...bodies.values()].map((members) => members.length > 1 ? Math.sqrt(members.length) * 45 + 35 : 65));
    const step = (maxRadius * 2 + 220) * this.spacing;
    // Breadth-first layers follow actual connectivity. Lexical AS order must not bring a distant AS back
    // into the local shell. Keep each branch pointing away from its nearest managed root in all three axes.
    const adjacency = new Map(keys.map((key) => [key, new Set<string>()]));
    for (const link of this.links) {
      const a = this.bodyKey(this.vertices.get(link.source)!), b = this.bodyKey(this.vertices.get(link.target)!);
      if (a !== b) { adjacency.get(a)!.add(b); adjacency.get(b)!.add(a); }
    }
    const roots = keys.filter((key) => primary.has(key));
    if (!roots.length && keys.length) roots.push(keys[0]!);
    const depths = new Map(roots.map((key) => [key, 0]));
    const parents = new Map<string, string>();
    const queue = [...roots];
    for (let i = 0; i < queue.length; i++) {
      const key = queue[i]!;
      for (const neighbor of [...adjacency.get(key)!].sort()) if (!depths.has(neighbor)) {
        depths.set(neighbor, depths.get(key)! + 1);
        parents.set(neighbor, key);
        queue.push(neighbor);
      }
    }
    const disconnected = Math.max(0, ...depths.values()) + 1;
    for (const key of keys) if (!depths.has(key)) depths.set(key, disconnected);
    this.anchors.clear();
    for (let depth = 0; depth <= disconnected; depth++) {
      const layer = keys.filter((key) => depths.get(key) === depth);
      for (const [index, key] of layer.entries()) {
        let direction = spherePoint(index, layer.length, 0.35);
        const parent = this.anchors.get(parents.get(key) ?? "");
        if (depth > 1 && parent) {
          const length = Math.hypot(parent.x, parent.y, parent.z) || 1;
          const branch = stableDirection(key);
          const x = parent.x / length + branch.x * 0.3, y = parent.y / length + branch.y * 0.3, z = parent.z / length + branch.z * 0.3;
          const norm = Math.hypot(x, y, z);
          direction = { x: x / norm, y: y / norm, z: z / norm };
        }
        const radius = depth ? depth * step : roots.length > 1 ? step * 0.45 : 0;
        this.anchors.set(key, { x: direction.x * radius, y: direction.y * radius, z: direction.z * radius });
      }
    }
  }

  /** Default view includes every configured session, regardless of observed state, plus isolated managed nodes. */
  private framedVertices(): SimVertex[] {
    const vertices = [...this.vertices.values()];
    if (this.viewFrame === "all" || this.viewFrame === null) return vertices;
    if (this.viewFrame !== "direct") {
      const ids = this.viewFrame;
      return vertices.filter((v) => ids.includes(v.id));
    }
    const ids = new Set(this.links.filter((l) => l.direct !== false).flatMap((l) => [l.source, l.target]));
    const primaryGroups = new Set(this.groups.filter((g) => g.primary).map((g) => g.id));
    const direct = vertices.filter((v) => ids.has(v.id) || v.primary || primaryGroups.has(v.group ?? ""));
    return direct.length ? direct : vertices;
  }

  /** Members of each group present in the graph. */
  private members(): Map<string, SimVertex[]> {
    const out = new Map<string, SimVertex[]>();
    for (const v of this.vertices.values()) if (v.group) out.set(v.group, [...(out.get(v.group) ?? []), v]);
    return out;
  }

  /** World centre and radius of each group's bubble. */
  private bubbles(): Array<{ group: GraphGroup; x: number; y: number; z: number; r: number }> {
    const members = this.members();
    const out = [];
    for (const g of this.groups) {
      const vs = members.get(g.id);
      if (!vs?.length) continue;
      const c = centroid(vs);
      const r = Math.max(...vs.map((v) => Math.hypot(v.x - c.x, v.y - c.y, v.z - c.z) + v.radius * 2.6)) + 22;
      out.push({ group: g, ...c, r });
    }
    return out;
  }

  private frameVertices(vertices: SimVertex[], ease = 1): void {
    if (!vertices.length || !this.width || !this.height) return;
    const bounds = (key: "x" | "y" | "z") => (Math.min(...vertices.map((v) => v[key])) + Math.max(...vertices.map((v) => v[key]))) / 2;
    this.target = { x: bounds("x"), y: bounds("y"), z: bounds("z") };
    this.panX = this.panY = 0;
    const ids = new Set(vertices.map((v) => v.id));
    const points = [...vertices.map((v) => ({ ...v, r: v.radius + 28 })),
      ...this.bubbles().filter((b) => [...this.vertices.values()].filter((v) => v.group === b.group.id).every((v) => ids.has(v.id)))];
    const halfWidth = Math.max(40, this.width / 2 - 65), halfHeight = Math.max(40, this.height / 2 - 65);
    let distance = 180;
    for (const p of points) {
      const [x, y, z] = this.rotate(p.x - this.target.x, p.y - this.target.y, p.z - this.target.z);
      distance = Math.max(distance, z + p.r + NEAR,
        z + p.r + (FOCAL * (Math.abs(x) + p.r)) / halfWidth,
        z + p.r + (FOCAL * (Math.abs(y) + p.r)) / halfHeight);
    }
    this.distance = ease === 1 ? distance : this.distance + (distance - this.distance) * ease;
  }

  // ---- projection ---------------------------------------------------------------------------------------------

  private fit(): void {
    const r = this.canvas.getBoundingClientRect();
    const dpr = window.devicePixelRatio || 1;
    const bitmapWidth = Math.max(1, Math.round(r.width * dpr));
    const bitmapHeight = Math.max(1, Math.round(r.height * dpr));
    if (this.width === r.width && this.height === r.height && this.canvas.width === bitmapWidth && this.canvas.height === bitmapHeight) return;
    const oldWidth = this.width, oldHeight = this.height;
    this.width = r.width;
    this.height = r.height;
    if (this.canvas.width !== bitmapWidth) this.canvas.width = bitmapWidth;
    if (this.canvas.height !== bitmapHeight) this.canvas.height = bitmapHeight;
    this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    if (this.viewFrame) {
      this.frameVertices(this.framedVertices());
    } else if (oldWidth > 0 && oldHeight > 0 && this.width > 0 && this.height > 0) {
      this.distance *= Math.min(oldWidth, oldHeight) / Math.min(this.width, this.height);
      this.panX *= this.width / oldWidth;
      this.panY *= this.height / oldHeight;
    }
    this.labelSlots.clear();
    // Assigning canvas dimensions clears its bitmap; redraw in this resize callback, not a later frame.
    this.draw();
  }

  /** World → camera space (rotation only). */
  private rotate(x: number, y: number, z: number): [number, number, number] {
    const cy = Math.cos(this.yaw), sy = Math.sin(this.yaw), cp = Math.cos(this.pitch), sp = Math.sin(this.pitch);
    const x1 = x * cy + z * sy;
    const z1 = -x * sy + z * cy;
    const y2 = y * cp - z1 * sp;
    const z2 = y * sp + z1 * cp;
    return [x1, y2, z2];
  }

  /** Camera space → world (inverse rotation). */
  private unrotate(x: number, y: number, z: number): [number, number, number] {
    const cy = Math.cos(this.yaw), sy = Math.sin(this.yaw), cp = Math.cos(this.pitch), sp = Math.sin(this.pitch);
    const y1 = y * cp + z * sp;
    const z1 = -y * sp + z * cp;
    return [x * cy - z1 * sy, y1, x * sy + z1 * cy];
  }

  /** Screen position, perspective scale and depth (distance from the camera) of a world point. */
  private project(x: number, y: number, z: number): { sx: number; sy: number; scale: number; depth: number; visible: boolean } {
    const [xr, yr, zr] = this.rotate(x - this.target.x, y - this.target.y, z - this.target.z);
    const depth = this.distance - zr;
    const scale = FOCAL / Math.max(NEAR, depth);
    return { sx: this.width / 2 + this.panX + xr * scale, sy: this.height / 2 + this.panY + yr * scale, scale, depth, visible: depth > NEAR };
  }

  // ---- drawing ------------------------------------------------------------------------------------------------

  private draw(): void {
    const ctx = this.ctx;
    const { text, background } = this.opts.colors();
    ctx.clearRect(0, 0, this.width, this.height);
    this.labelHits = [];
    const proj = new Map([...this.vertices.values()].map((v) => [v.id, { v, ...this.project(v.x, v.y, v.z) }]));
    const fog = (depth: number) => Math.max(0.25, Math.min(1, 1.35 - depth / (this.distance * 2)));
    const selectedNodes = new Set([...this.vertices.values()].filter((v) => v.id === this.highlight || v.group === this.highlight).map((v) => v.id));
    const activeLinks = new Set(this.links.filter((l) => l.id === this.highlight || selectedNodes.has(l.source) || selectedNodes.has(l.target)).map((l) => l.id));
    const activeNodes = new Set([...selectedNodes, ...this.links.filter((l) => activeLinks.has(l.id)).flatMap((l) => [l.source, l.target])]);
    const hoveredNodes = new Set([...this.vertices.values()].filter((v) => this.hover && (v.id === this.hover.id || v.group === this.hover.id)).map((v) => v.id));
    for (const link of this.links) if (link.id === this.hover?.id) { hoveredNodes.add(link.source); hoveredNodes.add(link.target); }
    const lit = (id: string) => !this.highlight || activeNodes.has(id) || activeLinks.has(id);
    const t = performance.now() / 1000;
    const occupied: Array<{ x: number; y: number; w: number; h: number; weight?: number }> = [];
    const reservedLabels: Array<{ x: number; y: number; w: number; h: number }> = [];
    // Camera-facing text belongs to its object's depth plane, just like the AS labels. Geometry at the same
    // depth wins ties; split long depth-spanning links so their foreground sections stay in front of text.
    const scene: Array<{ depth: number; layer: number; draw: () => void }> = [];
    const edges: Array<{ sx: number; sy: number; ex: number; ey: number; width: number }> = [];
    // One camera-facing translucent panel renderer for AS headings and router/node labels. Panels share
    // the object's depth plane; there are no label-to-object leader lines or final screen-overlay pass.
    const labelSize = (label: string, subtitle?: string) => {
      ctx.font = "600 14px system-ui, sans-serif";
      const titleWidth = ctx.measureText(label).width;
      ctx.font = "12px system-ui, sans-serif";
      return { w: Math.max(titleWidth, subtitle ? ctx.measureText(subtitle).width : 0) + 16, h: subtitle ? 42 : 24 };
    };
    const labelPanel = (label: string, subtitle: string | undefined, color: string, depth: number, opacity: number, box: { x: number; y: number; w: number; h: number }, item: GraphItem) => {
      this.labelHits.push({ item, depth, ...box });
      scene.push({ depth, layer: 0, draw: () => {
        ctx.globalAlpha = opacity * 0.16;
        ctx.fillStyle = background;
        ctx.fillRect(box.x, box.y, box.w, box.h);
        ctx.globalAlpha = opacity;
        ctx.fillStyle = color;
        ctx.font = "600 14px system-ui, sans-serif";
        ctx.textAlign = "center";
        ctx.textBaseline = "middle";
        ctx.fillText(label, box.x + box.w / 2, box.y + 12);
        if (subtitle) {
          ctx.font = "12px system-ui, sans-serif";
          ctx.fillText(subtitle, box.x + box.w / 2, box.y + 30);
        }
      } });
    };

    // Node/router panels have one fixed attachment: just to the right of their projected glyph, vertically
    // centered. Never score, cache, flip or displace them in response to nearby labels/links or viewport edges.
    // Reserve their visible bounds first so AS headings yield to the node, rather than moving its label away.
    const nodePanels = new Map<string, { x: number; y: number; w: number; h: number }>();
    const emphasized = (id: string) => activeNodes.has(id) || hoveredNodes.has(id) || this.drag?.kind === "vertex" && this.drag.v.id === id;
    for (const p of proj.values()) {
      if (p.v.labelPriority || !p.visible) continue;
      const r = p.v.radius * p.scale;
      if (p.sx + r < 0 || p.sx - r > this.width || p.sy + r < 0 || p.sy - r > this.height) continue;
      if (r >= 8) this.detailedLabels.add(p.v.id);
      else if (r < 6) this.detailedLabels.delete(p.v.id);
      if (!this.detailedLabels.has(p.v.id) && !emphasized(p.v.id)) continue;
      const { w, h } = labelSize(p.v.label, p.v.subtitle);
      const extent = r * (p.v.shape === "triangle" ? 1.1 : p.v.shape === "square" ? 0.9 : 1);
      const box = { x: p.sx + extent + 6, y: p.sy - h / 2, w, h };
      nodePanels.set(p.v.id, box);
      occupied.push({ ...box, weight: 16 });
      reservedLabels.push(box);
    }

    // AS bubbles sit behind the scene; their labels join nodes and links at the cluster's projected depth.
    const bubbles = this.bubbles()
      .map((b) => ({ b, ...this.project(b.x, b.y, b.z) }))
      .filter((p) => p.visible && p.b.r * p.scale < Math.max(this.width, this.height) * 2)
      .sort((x, y) => y.depth - x.depth);
    for (const { b, sx, sy, scale, depth } of bubbles) {
      const r = b.r * scale;
      const on = !this.highlight || this.highlight === b.group.id || [...this.vertices.values()].some((v) => v.group === b.group.id && v.id === this.highlight);
      ctx.globalAlpha = fog(depth) * (on ? 1 : 0.35);
      ctx.fillStyle = b.group.color;
      ctx.globalAlpha *= this.hover?.id === b.group.id ? 0.16 : 0.08;
      ctx.beginPath();
      ctx.arc(sx, sy, r, 0, Math.PI * 2);
      ctx.fill();
      ctx.globalAlpha = fog(depth) * (on ? 0.7 : 0.25);
      ctx.strokeStyle = b.group.color;
      ctx.lineWidth = this.hover?.id === b.group.id || this.highlight === b.group.id ? 2 : 1.2;
      ctx.setLineDash([6, 5]);
      ctx.stroke();
      ctx.setLineDash([]);
      const { w, h } = labelSize(b.group.label, b.group.subtitle);
      const candidates = Array.from({ length: 8 }, (_, i) => i * 18).flatMap((offset) => [
        { x: sx - w / 2, y: sy - r - h - 6 - offset, w, h },
        { x: sx - w / 2, y: sy + r + 6 + offset, w, h },
        { x: sx + r + 6 + offset, y: sy - h / 2, w, h },
        { x: sx - r - w - 6 - offset, y: sy - h / 2, w, h },
      ]);
      const clear = (box: typeof candidates[number]) => !reservedLabels.some((a) =>
        box.x < a.x + a.w + 4 && box.x + w + 4 > a.x && box.y < a.y + a.h + 4 && box.y + h + 4 > a.y);
      let slot = this.groupLabelSlots.get(b.group.id);
      if (slot === undefined || !candidates[slot] || !clear(candidates[slot]!)) {
        let eligible = candidates.map((_, i) => i).filter((i) => clear(candidates[i]!));
        if (!eligible.length) {
          candidates.push({ x: sx - w / 2, y: Math.min(...reservedLabels.map((a) => a.y)) - h - 6, w, h });
          eligible = [candidates.length - 1];
        }
        const score = (index: number) => {
          const box = candidates[index]!;
          const outside = Math.max(0, -box.x) + Math.max(0, -box.y) + Math.max(0, box.x + w - this.width) + Math.max(0, box.y + h - this.height);
          return outside * 1000 + Math.hypot(box.x + w / 2 - sx, box.y + h / 2 - sy);
        };
        slot = eligible.reduce((best, i) => score(i) < score(best) ? i : best, eligible[0]!);
      }
      this.groupLabelSlots.set(b.group.id, slot);
      const box = candidates[slot]!;
      const opacity = fog(depth) * (on ? 1 : 0.4);
      labelPanel(b.group.label, b.group.subtitle, b.group.color, depth, opacity, box, { kind: "group", id: b.group.id });
      occupied.push({ ...box, weight: 16 });
      reservedLabels.push(box);
    }

    for (const l of this.links) {
      const s = proj.get(l.source)!;
      const e = proj.get(l.target)!;
      if (!s.visible || !e.visible) continue;
      const linked = !this.highlight || activeLinks.has(l.id);
      const point = (fraction: number) => this.project(s.v.x + (e.v.x - s.v.x) * fraction, s.v.y + (e.v.y - s.v.y) * fraction, s.v.z + (e.v.z - s.v.z) * fraction);
      const pieces = Math.min(24, Math.max(1, Math.ceil(Math.abs(s.depth - e.depth) / 60)));
      for (let piece = 0; piece < pieces; piece++) {
        const from = piece / pieces, to = (piece + 1) / pieces;
        const start = point(from), end = point(to);
        const depth = (start.depth + end.depth) / 2;
        const scale = (start.scale + end.scale) / 2;
        edges.push({ sx: start.sx, sy: start.sy, ex: end.sx, ey: end.sy, width: Math.max(2, l.width * scale) });
        scene.push({ depth, layer: 1, draw: () => {
          ctx.globalAlpha = fog(depth) * (linked ? (this.highlight || this.hover?.id === l.id ? 0.95 : 0.55) : 0.1);
          ctx.strokeStyle = l.color;
          ctx.lineWidth = Math.max(0.6, l.width * scale) * (this.hover?.id === l.id ? 2 : 1);
          ctx.setLineDash(l.dash ?? []);
          ctx.lineDashOffset = -Math.hypot(start.sx - s.sx, start.sy - s.sy);
          ctx.beginPath();
          ctx.moveTo(start.sx, start.sy);
          ctx.lineTo(end.sx, end.sy);
          ctx.stroke();
          ctx.setLineDash([]);
          ctx.lineDashOffset = 0;
          if (l.particles > 0) {
            ctx.fillStyle = l.color;
            const r = Math.max(1.2, 2.2 * scale);
            for (let i = 0; i < l.particles; i++) for (const dir of [0, 1]) {
              const f = (t * 0.35 + i / l.particles + (dir ? 0.5 / l.particles : 0) + hash(l.id)) % 1;
              const fraction = dir ? 1 - f : f;
              if (fraction < from || fraction >= to) continue;
              const p = point(fraction);
              ctx.beginPath();
              ctx.arc(p.sx, p.sy, r, 0, Math.PI * 2);
              ctx.fill();
            }
          }
        } });
      }
    }

    const order = [...proj.values()].filter((p) => p.visible).sort((a, b) => b.depth - a.depth);
    for (const p of order) {
      const v = p.v;
      const r = v.radius * p.scale;
      scene.push({ depth: p.depth, layer: 2, draw: () => {
        ctx.globalAlpha = fog(p.depth) * (lit(v.id) || this.links.some((l) => (l.source === v.id || l.target === v.id) && lit(l.id)) ? 1 : 0.25);
        ctx.fillStyle = v.color;
        ctx.beginPath();
        if (v.shape === "diamond") {
          ctx.moveTo(p.sx, p.sy - r * 1.25);
          ctx.lineTo(p.sx + r * 1.25, p.sy);
          ctx.lineTo(p.sx, p.sy + r * 1.25);
          ctx.lineTo(p.sx - r * 1.25, p.sy);
          ctx.closePath();
        } else if (v.shape === "square") {
          ctx.rect(p.sx - r * 0.9, p.sy - r * 0.9, r * 1.8, r * 1.8);
        } else if (v.shape === "triangle") {
          ctx.moveTo(p.sx, p.sy - r * 1.2);
          ctx.lineTo(p.sx + r * 1.1, p.sy + r * 0.8);
          ctx.lineTo(p.sx - r * 1.1, p.sy + r * 0.8);
          ctx.closePath();
        } else {
          ctx.arc(p.sx, p.sy, r, 0, Math.PI * 2);
        }
        ctx.fill();
        ctx.lineWidth = Math.max(1, 2.2 * p.scale);
        ctx.strokeStyle = v.ring ?? background;
        ctx.setLineDash(v.dashedRing ? [3, 3] : []);
        ctx.stroke();
        ctx.setLineDash([]);
        if (this.hover?.id === v.id || this.highlight === v.id) {
          ctx.strokeStyle = text;
          ctx.lineWidth = 1.5;
          ctx.beginPath();
          ctx.arc(p.sx, p.sy, r * 1.25 + 4, 0, Math.PI * 2);
          ctx.stroke();
        }
      } });
      occupied.push({ x: p.sx - r - 3, y: p.sy - r - 3, w: (r + 3) * 2, h: (r + 3) * 2, weight: 30 });
    }
    // Only AS vertices choose an avoidance slot. Node/router labels stay attached to their glyph regardless
    // of AS title changes, depth-order crossings, hover, selection or overlapping links.
    order.sort((a, b) => (b.v.labelPriority ?? 0) - (a.v.labelPriority ?? 0) || a.v.id.localeCompare(b.v.id));
    for (const p of order) {
      const r = p.v.radius * p.scale;
      if (p.sx + r < 0 || p.sx - r > this.width || p.sy + r < 0 || p.sy - r > this.height) continue;
      const opacity = fog(p.depth) * (lit(p.v.id) || emphasized(p.v.id) ? 1 : 0.3);
      if (!p.v.labelPriority) {
        const box = nodePanels.get(p.v.id);
        if (box) labelPanel(p.v.label, p.v.subtitle, text, p.depth, opacity, box, { kind: "vertex", id: p.v.id });
        continue;
      }
      const { w, h } = labelSize(p.v.label, p.v.subtitle);
      // Keep slot indexes stable, including when a long AS name requires a more distant position.
      const candidates = Array.from({ length: 16 }, (_, i) => i * 22).flatMap((offset) => [
        { x: p.sx - w / 2, y: p.sy + r + 5 + offset, w, h },
        { x: p.sx - w / 2, y: p.sy - r - h - 5 - offset, w, h },
        { x: p.sx + r + 5 + offset, y: p.sy - h / 2, w, h },
        { x: p.sx - r - w - 5 - offset, y: p.sy - h / 2, w, h },
        { x: p.sx + r + 5 + offset, y: p.sy + r + 5 + offset, w, h },
        { x: p.sx - r - w - 5 - offset, y: p.sy + r + 5 + offset, w, h },
        { x: p.sx + r + 5 + offset, y: p.sy - r - h - 5 - offset, w, h },
        { x: p.sx - r - w - 5 - offset, y: p.sy - r - h - 5 - offset, w, h },
      ]);
      const score = (b: typeof candidates[number]) => {
        const outside = Math.max(0, 2 - b.x) + Math.max(0, 2 - b.y) + Math.max(0, b.x + w - this.width + 2) + Math.max(0, b.y + h - this.height + 2);
        return outside * 1000 + occupied.reduce((sum, o) => sum +
          Math.max(0, Math.min(b.x + w + 2, o.x + o.w + 2) - Math.max(b.x, o.x)) *
          Math.max(0, Math.min(b.y + h + 2, o.y + o.h + 2) - Math.max(b.y, o.y)) * (o.weight ?? 1), 0) +
          edges.reduce((sum, e) => sum + segmentBoxOverlap(e.sx, e.sy, e.ex, e.ey, b) * e.width * 4, 0);
      };
      const previous = this.labelSlots.get(p.v.id);
      const clearOfAs = (b: typeof candidates[number]) => !reservedLabels.some((a) =>
        b.x < a.x + a.w + 4 && b.x + b.w + 4 > a.x && b.y < a.y + a.h + 4 && b.y + b.h + 4 > a.y);
      let slot = previous;
      if (slot === undefined || !candidates[slot] || !clearOfAs(candidates[slot]!)) {
        let eligible = candidates.map((_, i) => i).filter((i) => clearOfAs(candidates[i]!));
        // In very dense views extend beyond every AS block rather than accepting an overlapping label.
        if (!eligible.length) {
          const y = Math.max(...reservedLabels.map((a) => a.y + a.h)) + 8;
          candidates.push({ x: p.sx - w / 2, y, w, h });
          eligible = [candidates.length - 1];
        }
        slot = eligible.reduce((best, i) => score(candidates[i]!) < score(candidates[best]!) ? i : best, eligible[0]!);
      }
      this.labelSlots.set(p.v.id, slot);
      // Like AS labels, follow the projected object; let the canvas clip at its edge instead of pinning text
      // to the viewport as an overlay while the router continues to move through the scene.
      const box = candidates[slot]!;
      occupied.push({ ...box, weight: 16 });
      reservedLabels.push(box);
      labelPanel(p.v.label, p.v.subtitle, p.v.labelPriority ? p.v.color : text, p.depth, opacity, box, { kind: "vertex", id: p.v.id });
    }
    scene.sort((a, b) => b.depth - a.depth || a.layer - b.layer);
    for (const item of scene) item.draw();
    ctx.globalAlpha = 1;
  }

  // ---- interaction --------------------------------------------------------------------------------------------

  private point(e: PointerEvent | MouseEvent): [number, number] {
    const r = this.canvas.getBoundingClientRect();
    return [e.clientX - r.left, e.clientY - r.top];
  }

  private hit(x: number, y: number): GraphItem | null {
    const proj = [...this.vertices.values()].map((v) => ({ v, ...this.project(v.x, v.y, v.z) })).filter((p) => p.visible).sort((a, b) => a.depth - b.depth);
    for (const p of proj) {
      const r = p.v.radius * p.scale * (p.v.shape === "diamond" ? 1.25 : 1) + 4;
      if ((p.sx - x) ** 2 + (p.sy - y) ** 2 <= r * r) return { kind: "vertex", id: p.v.id };
    }
    const panel = this.labelHits.filter((p) => x >= p.x && x <= p.x + p.w && y >= p.y && y <= p.y + p.h).sort((a, b) => a.depth - b.depth)[0];
    if (panel) return panel.item;
    const at = new Map(proj.map((p) => [p.v.id, p]));
    let best: { id: string; d: number } | null = null;
    for (const l of this.links) {
      const s = at.get(l.source);
      const e = at.get(l.target);
      if (!s || !e) continue;
      const d = segmentDistance(x, y, s.sx, s.sy, e.sx, e.sy);
      if (d < 6 && (!best || d < best.d)) best = { id: l.id, d };
    }
    if (best) return { kind: "link", id: best.id };
    // inside a cluster bubble (the smallest one when they overlap)
    const inside = this.bubbles()
      .map((b) => ({ b, ...this.project(b.x, b.y, b.z) }))
      .filter((p) => p.visible && p.b.r * p.scale < Math.max(this.width, this.height) * 2 && (p.sx - x) ** 2 + (p.sy - y) ** 2 <= (p.b.r * p.scale) ** 2)
      .sort((p, q) => p.b.r * p.scale - q.b.r * q.scale)[0];
    return inside ? { kind: "group", id: inside.b.group.id } : null;
  }

  private onDown = (e: PointerEvent) => {
    if (e.button !== 0 && e.button !== 1 && e.button !== 2) return;
    e.preventDefault();
    const [x, y] = this.point(e);
    this.canvas.setPointerCapture(e.pointerId);
    this.canvas.focus({ preventScroll: true });
    this.manualView();
    this.pointers.set(e.pointerId, { x, y });
    this.hover = null;
    this.opts.onHover?.(null, x, y);
    this.canvas.style.cursor = "grabbing";
    if (this.pointers.size > 1) {
      if (this.drag?.kind === "vertex") this.drag.v.pinned = false;
      this.alphaTarget = 0;
      this.drag = { kind: "gesture", ...this.gesture(), moved: true };
      return;
    }
    const h = this.hit(x, y);
    if (e.shiftKey || e.button === 1 || e.button === 2) {
      this.drag = { kind: "pan", pointer: e.pointerId, x, y, startX: x, startY: y, moved: false };
    } else if (h?.kind === "vertex") {
      const v = this.vertices.get(h.id)!;
      v.pinned = true;
      const p = this.project(v.x, v.y, v.z);
      this.drag = { kind: "vertex", pointer: e.pointerId, v, depth: p.depth, x, y, offsetX: p.sx - x, offsetY: p.sy - y, moved: false };
    } else {
      this.drag = { kind: "rotate", pointer: e.pointerId, x, y, startX: x, startY: y, moved: false };
    }
  };

  private onMove = (e: PointerEvent) => {
    const [x, y] = this.point(e);
    if (!this.drag) {
      const h = this.hit(x, y);
      if (h?.id !== this.hover?.id) this.canvas.style.cursor = h ? "pointer" : "grab";
      this.hover = h;
      this.opts.onHover?.(h, x, y);
      if (h) this.interacted = performance.now(); // inspecting a tooltip pauses optional auto-rotation
      return;
    }
    if (!this.pointers.has(e.pointerId)) return;
    this.pointers.set(e.pointerId, { x, y });
    this.interacted = performance.now();
    if (this.drag.kind === "gesture") {
      const next = this.gesture();
      this.panX += next.x - this.drag.x;
      this.panY += next.y - this.drag.y;
      this.zoomBy(this.drag.distance / next.distance, next.x, next.y);
      this.drag = { kind: "gesture", ...next, moved: true };
    } else if (e.pointerId !== this.drag.pointer) return;
    else if (this.drag.kind === "vertex") {
      // a click jitters by a pixel or two: only a real drag moves the vertex (and is no click)
      if (!this.drag.moved && Math.abs(x - this.drag.x) + Math.abs(y - this.drag.y) <= 3) return;
      this.alphaTarget = 0.25;
      this.alpha = Math.max(this.alpha, 0.25);
      const d = this.drag.depth;
      const xr = ((x + this.drag.offsetX - this.width / 2 - this.panX) * d) / FOCAL;
      const yr = ((y + this.drag.offsetY - this.height / 2 - this.panY) * d) / FOCAL;
      const [wx, wy, wz] = this.unrotate(xr, yr, this.distance - d);
      Object.assign(this.drag.v, { x: wx + this.target.x, y: wy + this.target.y, z: wz + this.target.z });
      this.drag.moved = true;
    } else {
      const dx = x - this.drag.x;
      const dy = y - this.drag.y;
      if (Math.hypot(x - this.drag.startX, y - this.drag.startY) > 3) this.drag.moved = true;
      if (this.drag.kind === "pan") {
        this.panX += dx;
        this.panY += dy;
      } else {
        this.yaw = (this.yaw + dx * 0.005) % (Math.PI * 2);
        this.pitch = Math.max(-MAX_PITCH, Math.min(MAX_PITCH, this.pitch + dy * 0.005));
      }
      this.drag.x = x;
      this.drag.y = y;
    }
  };

  private onUp = (e: PointerEvent) => {
    const d = this.drag;
    if (!this.pointers.has(e.pointerId)) return;
    this.pointers.delete(e.pointerId);
    if (this.canvas.hasPointerCapture(e.pointerId)) this.canvas.releasePointerCapture(e.pointerId);
    if (d?.kind === "gesture" && this.pointers.size > 1) {
      this.drag = { kind: "gesture", ...this.gesture(), moved: true };
      return;
    }
    if (d?.kind === "gesture" && this.pointers.size) {
      const [pointer, point] = [...this.pointers][0]!;
      this.drag = { kind: "rotate", pointer, ...point, startX: point.x, startY: point.y, moved: true };
      return;
    }
    this.drag = null;
    this.alphaTarget = 0;
    if (!d) return;
    if (d.kind === "vertex") d.v.pinned = false;
    this.canvas.style.cursor = "grab";
    if (!d.moved && d.kind !== "pan" && e.type === "pointerup") {
      const [x, y] = this.point(e);
      this.opts.onClick?.(d.kind === "vertex" && this.vertices.has(d.v.id) ? { kind: "vertex", id: d.v.id } : this.hit(x, y));
    }
  };

  private onLeave = () => {
    if (this.drag) return;
    this.hover = null;
    this.opts.onHover?.(null, 0, 0);
  };

  private onWheel = (e: WheelEvent) => {
    e.preventDefault();
    const [x, y] = this.point(e);
    const unit = e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? this.height : 1;
    this.zoomBy(Math.exp(Math.max(-160, Math.min(160, e.deltaY * unit)) * 0.002), x, y);
  };

  private onDblClick = (e: MouseEvent) => {
    const [x, y] = this.point(e);
    const item = this.hit(x, y);
    if (item) this.focus(item);
    else this.fitGraph();
  };

  private gesture(): { x: number; y: number; distance: number } {
    const [a, b] = [...this.pointers.values()];
    return { x: (a!.x + b!.x) / 2, y: (a!.y + b!.y) / 2, distance: Math.max(10, Math.hypot(a!.x - b!.x, a!.y - b!.y)) };
  }

  private manualView(): void {
    this.interacted = performance.now();
    this.userView = true;
    this.fitting = false;
    this.viewFrame = null;
  }

  private cancelDrag(): void {
    if (this.drag?.kind === "vertex") this.drag.v.pinned = false;
    this.drag = null;
    this.alphaTarget = 0;
    for (const id of this.pointers.keys()) if (this.canvas.hasPointerCapture(id)) this.canvas.releasePointerCapture(id);
    this.pointers.clear();
    this.canvas.style.cursor = "grab";
  }

  private onContextMenu = (e: Event) => e.preventDefault();

  private onKey = (e: KeyboardEvent) => {
    if (e.key === "+" || e.key === "=") this.zoomBy(0.8);
    else if (e.key === "-") this.zoomBy(1.25);
    else if (e.key === "0") this.resetView();
    else if (["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown"].includes(e.key)) {
      const x = e.key === "ArrowLeft" ? -1 : e.key === "ArrowRight" ? 1 : 0;
      const y = e.key === "ArrowUp" ? -1 : e.key === "ArrowDown" ? 1 : 0;
      if (e.shiftKey) { this.panX += x * 25; this.panY += y * 25; }
      else { this.yaw += x * 0.08; this.pitch = Math.max(-MAX_PITCH, Math.min(MAX_PITCH, this.pitch + y * 0.08)); }
      this.manualView();
    } else return;
    e.preventDefault();
  };
}

/** Length of a projected link inside a padded label box (Liang-Barsky clipping). */
function segmentBoxOverlap(ax: number, ay: number, bx: number, by: number, box: { x: number; y: number; w: number; h: number }): number {
  const dx = bx - ax, dy = by - ay;
  let from = 0, to = 1;
  for (const [p, q] of [[-dx, ax - box.x + 2], [dx, box.x + box.w + 2 - ax], [-dy, ay - box.y + 2], [dy, box.y + box.h + 2 - ay]] as const) {
    if (p === 0) { if (q < 0) return 0; else continue; }
    const t = q / p;
    if (p < 0) from = Math.max(from, t); else to = Math.min(to, t);
    if (from > to) return 0;
  }
  return Math.hypot(dx, dy) * (to - from);
}

function segmentDistance(px: number, py: number, ax: number, ay: number, bx: number, by: number): number {
  const dx = bx - ax, dy = by - ay;
  const l = dx * dx + dy * dy;
  const t = l ? Math.max(0, Math.min(1, ((px - ax) * dx + (py - ay) * dy) / l)) : 0;
  return Math.hypot(px - (ax + t * dx), py - (ay + t * dy));
}

/** Stable phase offset per link so particles of different links do not move in lockstep. */
function hash(s: string): number {
  let h = 0;
  for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) | 0;
  return ((h >>> 0) % 1000) / 1000;
}

/** Fibonacci sphere seeds a volume deterministically; the solver remains free to move in all three axes. */
function spherePoint(index: number, count: number, phase: number): Point {
  const y = 1 - 2 * (index + 0.5) / count;
  const radial = Math.sqrt(Math.max(0, 1 - y * y));
  const angle = index * Math.PI * (3 - Math.sqrt(5)) + phase;
  return { x: Math.cos(angle) * radial, y, z: Math.sin(angle) * radial };
}

function stableDirection(key: string): Point {
  const y = 2 * hash(`${key}:y`) - 1;
  const radial = Math.sqrt(Math.max(0, 1 - y * y));
  const angle = hash(`${key}:angle`) * Math.PI * 2;
  return { x: Math.cos(angle) * radial, y, z: Math.sin(angle) * radial };
}

function centroid(vs: Array<{ x: number; y: number; z: number }>): { x: number; y: number; z: number } {
  const n = vs.length || 1;
  return { x: vs.reduce((a, v) => a + v.x, 0) / n, y: vs.reduce((a, v) => a + v.y, 0) / n, z: vs.reduce((a, v) => a + v.z, 0) / n };
}
