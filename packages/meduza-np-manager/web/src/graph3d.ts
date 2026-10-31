// A small 3D force-directed graph on a 2D canvas (D26), without a WebGL dependency: the topology has tens of vertices,
// so an O(n²) simulation and painter's-algorithm drawing are plenty, and the console keeps working offline.
//
// Simulation (d3-force style): many-body repulsion, link springs, centering, velocity decay, cooling alpha.
// View: perspective camera orbiting the origin. Dragging a vertex moves it in the screen plane at its depth (it is
// pinned while held); dragging the background rotates; the wheel zooms; double-click resets the view. Links carry
// particles (data flowing both ways) while their session is up; the scene slowly turns while idle.
// Groups (the nodes of one AS) are drawn as translucent bubbles labelled with the group's name; members are pulled
// towards their group's centre and bubbles collide as whole bodies, so every AS forms a cluster of its own.

export interface GraphVertex {
  id: string;
  label: string;
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
  color: string;
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

export class ForceGraph3D {
  private ctx: CanvasRenderingContext2D;
  private vertices = new Map<string, SimVertex>();
  private links: GraphLink[] = [];
  private groups: GraphGroup[] = [];
  private alpha = 1;
  private alphaTarget = 0;
  private yaw = 0.6;
  private pitch = -0.35;
  private distance = 520;
  /** wheel zoom relative to the distance that fits the whole graph */
  private zoom = 1;
  private frame = 0;
  private raf = 0;
  private width = 0;
  private height = 0;
  private drag: null | { kind: "vertex"; v: SimVertex; depth: number; x: number; y: number; moved: boolean } | { kind: "rotate"; x: number; y: number; moved: boolean } = null;
  private hover: GraphItem | null = null;
  private interacted = 0;
  private resize: ResizeObserver;
  autoRotate = true;
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
    canvas.addEventListener("pointerleave", this.onLeave);
    canvas.addEventListener("wheel", this.onWheel, { passive: false });
    canvas.addEventListener("dblclick", this.onDblClick);
    const loop = () => {
      this.tick();
      this.draw();
      this.raf = requestAnimationFrame(loop);
    };
    this.raf = requestAnimationFrame(loop);
  }

  destroy(): void {
    cancelAnimationFrame(this.raf);
    this.resize.disconnect();
    this.canvas.removeEventListener("pointerdown", this.onDown);
    this.canvas.removeEventListener("pointermove", this.onMove);
    this.canvas.removeEventListener("pointerup", this.onUp);
    this.canvas.removeEventListener("pointercancel", this.onUp);
    this.canvas.removeEventListener("pointerleave", this.onLeave);
    this.canvas.removeEventListener("wheel", this.onWheel);
    this.canvas.removeEventListener("dblclick", this.onDblClick);
  }

  /** Replaces the graph; vertices that stay keep their position, new ones start next to a neighbour. */
  setData(vertices: GraphVertex[], links: GraphLink[], groups: GraphGroup[] = []): void {
    const next = new Map<string, SimVertex>();
    const structural =
      vertices.length !== this.vertices.size ||
      vertices.some((v) => !this.vertices.has(v.id) || this.vertices.get(v.id)!.group !== v.group) ||
      links.length !== this.links.length;
    this.groups = groups;
    for (const v of vertices) {
      const old = this.vertices.get(v.id);
      next.set(v.id, old ? { ...old, ...v } : { ...v, x: 0, y: 0, z: 0, vx: 0, vy: 0, vz: 0, pinned: false });
    }
    for (const v of next.values()) {
      if (this.vertices.has(v.id)) continue;
      // next to a member of its group, else next to a neighbour
      const near =
        [...this.vertices.values()].find((o) => v.group && o.group === v.group)?.id ??
        links.map((l) => (l.source === v.id ? l.target : l.target === v.id ? l.source : null)).find((n) => n && this.vertices.has(n));
      const base = near ? next.get(near)! : { x: 0, y: 0, z: 0 };
      const r = () => (Math.random() - 0.5) * 80;
      Object.assign(v, { x: base.x + r(), y: base.y + r(), z: base.z + r() });
    }
    this.vertices = next;
    this.links = links.filter((l) => next.has(l.source) && next.has(l.target));
    if (structural) this.alpha = Math.max(this.alpha, 0.8);
  }

  resetView(): void {
    this.yaw = 0.6;
    this.pitch = -0.35;
    this.zoom = 1;
    this.alpha = Math.max(this.alpha, 0.5);
  }

  // ---- simulation ---------------------------------------------------------------------------------------------

  private tick(): void {
    this.frame++;
    this.fitView();
    if (this.autoRotate && !this.drag && performance.now() - this.interacted > 2500) this.yaw += 0.0016;
    this.alpha += (this.alphaTarget - this.alpha) * 0.0228;
    if (this.alpha < 0.002) return;
    const vs = [...this.vertices.values()];
    const a = this.alpha;
    // many-body repulsion
    for (let i = 0; i < vs.length; i++) {
      for (let j = i + 1; j < vs.length; j++) {
        const p = vs[i]!;
        const q = vs[j]!;
        // only within a cluster: clusters are kept apart as whole bodies by the collision below (vertex repulsion
        // across clusters would let one cluster's nodes ring another instead of moving it)
        if (p.group !== q.group) continue;
        let dx = q.x - p.x, dy = q.y - p.y, dz = q.z - p.z;
        let l = dx * dx + dy * dy + dz * dz;
        if (l < 1) {
          dx = Math.random() - 0.5;
          dy = Math.random() - 0.5;
          dz = Math.random() - 0.5;
          l = 1;
        }
        const w = (-500 * a) / Math.max(l, 25);
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
    // bodies (cluster bubbles, lone vertices) do not overlap; a gentle gravity keeps unconnected ones in view
    const bodies = [
      ...this.bubbles().map((b) => ({ members: this.members().get(b.group.id)!, x: b.x, y: b.y, z: b.z, r: b.r })),
      ...vs.filter((v) => !v.group).map((v) => ({ members: [v], x: v.x, y: v.y, z: v.z, r: v.radius * 2.4 + 18 })),
    ];
    for (let i = 0; i < bodies.length; i++) {
      const p = bodies[i]!;
      for (const v of p.members) {
        v.vx -= p.x * 0.004 * a;
        v.vy -= p.y * 0.004 * a;
        v.vz -= p.z * 0.004 * a;
      }
      for (let j = i + 1; j < bodies.length; j++) {
        const q = bodies[j]!;
        const dx = q.x - p.x, dy = q.y - p.y, dz = q.z - p.z;
        const d = Math.hypot(dx, dy, dz) || 1;
        // a gap wide enough that bubbles rarely overlap in projection either
        const overlap = p.r + q.r + 70 - d;
        if (overlap <= 0) continue;
        const k = (overlap / d) * 0.35 * a;
        for (const v of p.members) {
          v.vx -= (dx * k) / p.members.length;
          v.vy -= (dy * k) / p.members.length;
          v.vz -= (dz * k) / p.members.length;
        }
        for (const v of q.members) {
          v.vx += (dx * k) / q.members.length;
          v.vy += (dy * k) / q.members.length;
          v.vz += (dz * k) / q.members.length;
        }
      }
    }
    // integrate with velocity decay; keep the centre of mass at the origin
    let cx = 0, cy = 0, cz = 0;
    for (const v of vs) {
      if (v.pinned) {
        v.vx = v.vy = v.vz = 0;
      } else {
        v.vx *= 0.6; v.vy *= 0.6; v.vz *= 0.6;
        v.x += v.vx; v.y += v.vy; v.z += v.vz;
      }
      cx += v.x; cy += v.y; cz += v.z;
    }
    if (vs.length && !this.drag) {
      cx /= vs.length; cy /= vs.length; cz /= vs.length;
      for (const v of vs) {
        v.x -= cx; v.y -= cy; v.z -= cz;
      }
    }
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

  /**
   * Eases the camera distance so the projected graph spans about 80% of the shorter side (times the wheel zoom):
   * measured on screen, so lopsided layouts fill the view as well as round ones.
   */
  private fitView(): void {
    if (!this.vertices.size || this.drag?.kind === "vertex" || !this.width) return;
    let extent = 0;
    for (const v of this.vertices.values()) {
      const p = this.project(v.x, v.y, v.z);
      extent = Math.max(extent, Math.abs(p.sx - this.width / 2) + v.radius * p.scale, Math.abs(p.sy - this.height / 2) + v.radius * p.scale + 16);
    }
    for (const b of this.bubbles()) {
      const p = this.project(b.x, b.y, b.z);
      extent = Math.max(extent, Math.abs(p.sx - this.width / 2) + b.r * p.scale, Math.abs(p.sy - this.height / 2) + b.r * p.scale + 18);
    }
    const want = (Math.min(this.width, this.height) / 2) * 0.8 / this.zoom;
    const target = Math.max(150, Math.min(6000, this.distance * (extent / Math.max(want, 40))));
    this.distance += (target - this.distance) * 0.06;
  }

  // ---- projection ---------------------------------------------------------------------------------------------

  private fit(): void {
    const r = this.canvas.getBoundingClientRect();
    const dpr = window.devicePixelRatio || 1;
    this.width = r.width;
    this.height = r.height;
    this.canvas.width = Math.max(1, Math.round(r.width * dpr));
    this.canvas.height = Math.max(1, Math.round(r.height * dpr));
    this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
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
  private project(x: number, y: number, z: number): { sx: number; sy: number; scale: number; depth: number } {
    const [xr, yr, zr] = this.rotate(x, y, z);
    const depth = Math.max(this.distance - zr, 20);
    const scale = FOCAL / depth;
    return { sx: this.width / 2 + xr * scale, sy: this.height / 2 + yr * scale, scale, depth };
  }

  // ---- drawing ------------------------------------------------------------------------------------------------

  private draw(): void {
    const ctx = this.ctx;
    const { text, background } = this.opts.colors();
    ctx.clearRect(0, 0, this.width, this.height);
    const proj = new Map([...this.vertices.values()].map((v) => [v.id, { v, ...this.project(v.x, v.y, v.z) }]));
    const fog = (depth: number) => Math.max(0.25, Math.min(1, 1.35 - depth / (this.distance * 2)));
    const lit = (id: string) => !this.highlight || this.highlight === id;
    const t = performance.now() / 1000;

    // AS clusters behind everything: translucent bubble, dashed outline, the AS label on top
    const bubbles = this.bubbles()
      .map((b) => ({ b, ...this.project(b.x, b.y, b.z) }))
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
      ctx.globalAlpha = fog(depth) * (on ? 1 : 0.4);
      ctx.fillStyle = b.group.color;
      ctx.font = `600 ${Math.round(Math.max(11, Math.min(16, 13 * scale)))}px system-ui, sans-serif`;
      ctx.textAlign = "center";
      ctx.textBaseline = "bottom";
      ctx.fillText(b.group.label, sx, sy - r - 3);
    }

    for (const l of this.links) {
      const s = proj.get(l.source)!;
      const e = proj.get(l.target)!;
      const linked = lit(l.id) || lit(l.source) || lit(l.target);
      ctx.globalAlpha = fog((s.depth + e.depth) / 2) * (linked ? 1 : 0.18);
      ctx.strokeStyle = l.color;
      ctx.lineWidth = Math.max(0.6, l.width * ((s.scale + e.scale) / 2)) * (this.hover?.id === l.id ? 2 : 1);
      ctx.setLineDash(l.dash ?? []);
      ctx.beginPath();
      ctx.moveTo(s.sx, s.sy);
      ctx.lineTo(e.sx, e.sy);
      ctx.stroke();
      ctx.setLineDash([]);
      if (l.particles > 0) {
        ctx.fillStyle = l.color;
        const r = Math.max(1.2, 2.2 * ((s.scale + e.scale) / 2));
        for (let i = 0; i < l.particles; i++) {
          for (const dir of [0, 1]) {
            const f = (t * 0.35 + i / l.particles + (dir ? 0.5 / l.particles : 0) + hash(l.id)) % 1;
            const p = dir ? 1 - f : f;
            ctx.beginPath();
            ctx.arc(s.sx + (e.sx - s.sx) * p, s.sy + (e.sy - s.sy) * p, r, 0, Math.PI * 2);
            ctx.fill();
          }
        }
      }
    }

    const order = [...proj.values()].sort((a, b) => b.depth - a.depth);
    for (const p of order) {
      const v = p.v;
      const r = v.radius * p.scale;
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
      ctx.fillStyle = text;
      ctx.font = `${Math.round(Math.max(10, Math.min(15, 12 * p.scale)))}px system-ui, sans-serif`;
      ctx.textAlign = "center";
      ctx.textBaseline = "top";
      ctx.fillText(v.label, p.sx, p.sy + r * 1.3 + 3);
    }
    ctx.globalAlpha = 1;
  }

  // ---- interaction --------------------------------------------------------------------------------------------

  private point(e: PointerEvent | MouseEvent): [number, number] {
    const r = this.canvas.getBoundingClientRect();
    return [e.clientX - r.left, e.clientY - r.top];
  }

  private hit(x: number, y: number): GraphItem | null {
    const proj = [...this.vertices.values()].map((v) => ({ v, ...this.project(v.x, v.y, v.z) })).sort((a, b) => a.depth - b.depth);
    for (const p of proj) {
      const r = p.v.radius * p.scale * (p.v.shape === "diamond" ? 1.25 : 1) + 4;
      if ((p.sx - x) ** 2 + (p.sy - y) ** 2 <= r * r) return { kind: "vertex", id: p.v.id };
    }
    const at = new Map(proj.map((p) => [p.v.id, p]));
    let best: { id: string; d: number } | null = null;
    for (const l of this.links) {
      const s = at.get(l.source)!;
      const e = at.get(l.target)!;
      const d = segmentDistance(x, y, s.sx, s.sy, e.sx, e.sy);
      if (d < 6 && (!best || d < best.d)) best = { id: l.id, d };
    }
    if (best) return { kind: "link", id: best.id };
    // inside a cluster bubble (the smallest one when they overlap)
    const inside = this.bubbles()
      .map((b) => ({ b, ...this.project(b.x, b.y, b.z) }))
      .filter((p) => (p.sx - x) ** 2 + (p.sy - y) ** 2 <= (p.b.r * p.scale) ** 2)
      .sort((p, q) => p.b.r * p.scale - q.b.r * q.scale)[0];
    return inside ? { kind: "group", id: inside.b.group.id } : null;
  }

  private onDown = (e: PointerEvent) => {
    const [x, y] = this.point(e);
    this.canvas.setPointerCapture(e.pointerId);
    this.interacted = performance.now();
    const h = this.hit(x, y);
    if (h?.kind === "vertex") {
      const v = this.vertices.get(h.id)!;
      v.pinned = true;
      this.drag = { kind: "vertex", v, depth: this.project(v.x, v.y, v.z).depth, x, y, moved: false };
      this.alphaTarget = 0.25;
      this.alpha = Math.max(this.alpha, 0.25);
    } else {
      this.drag = { kind: "rotate", x, y, moved: false };
    }
  };

  private onMove = (e: PointerEvent) => {
    const [x, y] = this.point(e);
    if (!this.drag) {
      const h = this.hit(x, y);
      if (h?.id !== this.hover?.id) this.canvas.style.cursor = h ? "pointer" : "grab";
      this.hover = h;
      this.opts.onHover?.(h, x, y);
      return;
    }
    this.interacted = performance.now();
    if (this.drag.kind === "vertex") {
      // a click jitters by a pixel or two: only a real drag moves the vertex (and is no click)
      if (!this.drag.moved && Math.abs(x - this.drag.x) + Math.abs(y - this.drag.y) <= 3) return;
      const d = this.drag.depth;
      const xr = ((x - this.width / 2) * d) / FOCAL;
      const yr = ((y - this.height / 2) * d) / FOCAL;
      const [wx, wy, wz] = this.unrotate(xr, yr, this.distance - d);
      Object.assign(this.drag.v, { x: wx, y: wy, z: wz });
      this.drag.moved = true;
    } else {
      const dx = x - this.drag.x;
      const dy = y - this.drag.y;
      if (Math.abs(dx) + Math.abs(dy) > 2) this.drag.moved = true;
      this.yaw += dx * 0.008;
      this.pitch = Math.max(-1.5, Math.min(1.5, this.pitch + dy * 0.008));
      this.drag.x = x;
      this.drag.y = y;
    }
  };

  private onUp = (e: PointerEvent) => {
    const d = this.drag;
    this.drag = null;
    this.alphaTarget = 0;
    if (!d) return;
    if (d.kind === "vertex") d.v.pinned = false;
    if (!d.moved) {
      const [x, y] = this.point(e);
      this.opts.onClick?.(this.hit(x, y));
    }
  };

  private onLeave = () => {
    if (this.drag) return;
    this.hover = null;
    this.opts.onHover?.(null, 0, 0);
  };

  private onWheel = (e: WheelEvent) => {
    e.preventDefault();
    this.interacted = performance.now();
    this.zoom = Math.max(0.2, Math.min(5, this.zoom * Math.exp(e.deltaY * 0.001)));
  };

  private onDblClick = (e: MouseEvent) => {
    const [x, y] = this.point(e);
    if (!this.hit(x, y)) this.resetView();
  };
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

function centroid(vs: Array<{ x: number; y: number; z: number }>): { x: number; y: number; z: number } {
  const n = vs.length || 1;
  return { x: vs.reduce((a, v) => a + v.x, 0) / n, y: vs.reduce((a, v) => a + v.y, 0) / n, z: vs.reduce((a, v) => a + v.z, 0) / n };
}
