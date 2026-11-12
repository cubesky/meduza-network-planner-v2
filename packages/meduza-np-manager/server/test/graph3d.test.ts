// Camera/drag regressions and cluster readability, exercising the same canvas controller the browser uses.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { ModuleKind, ScriptTarget, transpileModule } from "typescript";
import { REPO_ROOT } from "./helpers.js";

const source = readFileSync(join(REPO_ROOT, "packages/meduza-np-manager/web/src/graph3d.ts"), "utf8");
const compiled = transpileModule(source, { compilerOptions: { target: ScriptTarget.ES2023, module: ModuleKind.ESNext } }).outputText;
const { ForceGraph3D } = await import(`data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`);
Object.assign(globalThis, {
  window: { devicePixelRatio: 1 },
  ResizeObserver: class { observe() {} disconnect() {} },
  requestAnimationFrame: () => 1, cancelAnimationFrame: () => {},
});

function setup(width = 1200, height = 650) {
  const events = new Map<string, (event: any) => void>();
  const captured = new Set<number>();
  let clicks = 0;
  const labels: Array<{ text: string; x: number; y: number }> = [];
  const paints: Array<{ kind: string; color: string; alpha: number; text?: string; font?: string; align?: string }> = [];
  let clears = 0;
  const ctx: any = new Proxy({
    measureText: (s: string) => ({ width: s.length * 7 }),
    clearRect: () => { labels.length = 0; paints.length = 0; clears++; },
    fillText: (text: string, x: number, y: number) => { labels.push({ text, x, y }); paints.push({ kind: "text", color: ctx.fillStyle, alpha: ctx.globalAlpha, text, font: ctx.font, align: ctx.textAlign }); },
    fillRect: () => paints.push({ kind: "background", color: ctx.fillStyle, alpha: ctx.globalAlpha }),
    fill: () => paints.push({ kind: "fill", color: ctx.fillStyle, alpha: ctx.globalAlpha }),
    stroke: () => paints.push({ kind: "stroke", color: ctx.strokeStyle, alpha: ctx.globalAlpha }),
  }, { get: (target: any, key) => target[key] ?? (() => {}) });
  const canvas = {
    width, height, style: { cursor: "" }, getContext: () => ctx,
    getBoundingClientRect: () => ({ left: 0, top: 0, width, height }),
    addEventListener: (name: string, fn: (e: any) => void) => events.set(name, fn),
    removeEventListener: (name: string) => events.delete(name),
    setPointerCapture: (id: number) => captured.add(id), hasPointerCapture: (id: number) => captured.has(id),
    releasePointerCapture: (id: number) => captured.delete(id), focus: () => {},
  };
  const graph = new ForceGraph3D(canvas, { colors: () => ({ text: "#fff", background: "#000" }), onClick: () => clicks++ });
  const event = (name: string, fields: Record<string, unknown> = {}) => events.get(name)?.({
    type: name, pointerId: 1, button: 0, clientX: 5, clientY: 5, preventDefault() {}, ...fields,
  });
  return { graph, event, events, captured, canvas, labels, paints, clears: () => clears, clicks: () => clicks,
    resize: (w: number, h: number) => { width = w; height = h; graph.fit(); } };
}

function data() {
  const groups = Array.from({ length: 6 }, (_, i) => ({ id: `as${i}`, label: `AS${65000 + i}`, color: "#5cb", primary: i === 0 }));
  const vertices = groups.flatMap((group, i) => Array.from({ length: i === 0 ? 7 : 2 }, (_, n) => ({
    id: `${group.id}-${n}`, group: group.id, label: `router-${i}-${n}`, radius: 10, shape: "circle", color: "#5cb",
  })));
  const links = groups.slice(1).map((g, i) => ({ id: `edge${i}`, source: "as0-0", target: `${g.id}-0`, length: 420, width: 2, color: "#bc5", particles: 1 }));
  for (let i = 1; i < 7; i++) links.push({ id: `mesh${i}`, source: "as0-0", target: `as0-${i}`, length: 110, width: 1, color: "#5cb", particles: 1 });
  return { vertices, links, groups };
}

function populate(graph: any, ticks = 400) {
  const d = data();
  graph.setData(d.vertices, d.links, d.groups);
  for (let i = 0; i < ticks; i++) graph.tick();
  graph.draw();
  return d;
}

test("topology: cluster spheres stay separated in 3D and fit a wide viewport", () => {
  const { graph } = setup(1300, 560);
  populate(graph);
  graph.fitGraph();
  const bubbles = graph.bubbles().map((b: any) => ({ ...b, ...graph.project(b.x, b.y, b.z) }));
  for (let i = 0; i < bubbles.length; i++) {
    const a = bubbles[i]!;
    assert.ok(a.visible);
    assert.ok(a.sx - a.r * a.scale > 25 && a.sx + a.r * a.scale < 1275);
    assert.ok(a.sy - a.r * a.scale > 25 && a.sy + a.r * a.scale < 535);
    for (const b of bubbles.slice(i + 1)) assert.ok(Math.hypot(a.x - b.x, a.y - b.y, a.z - b.z) > a.r + b.r + 25, "cluster spheres have a real world-space gap");
  }
  const v = graph.vertices.get("as1-0");
  const before = Math.hypot(v.x, v.y, v.z);
  graph.setSpacing(3);
  for (let i = 0; i < 400; i++) graph.tick();
  assert.ok(Math.hypot(v.x, v.y, v.z) > before * 1.2);
  graph.destroy();
});

/** Rotation-independent volume of a point cloud: zero for any plane, including a tilted one. */
function volume(points: Array<{ x: number; y: number; z: number }>): number {
  const mean = (key: "x" | "y" | "z") => points.reduce((sum, p) => sum + p[key], 0) / points.length;
  const center = [mean("x"), mean("y"), mean("z")];
  const cov = Array.from({ length: 3 }, (_, i) => Array.from({ length: 3 }, (_, j) => points.reduce((sum, p) => {
    const v = [p.x, p.y, p.z];
    return sum + (v[i]! - center[i]!) * (v[j]! - center[j]!);
  }, 0) / points.length));
  const [a, b, c] = cov as [number[], number[], number[]];
  const determinant = a[0]! * (b[1]! * c[2]! - b[2]! * c[1]!) - a[1]! * (b[0]! * c[2]! - b[2]! * c[0]!) + a[2]! * (b[0]! * c[1]! - b[1]! * c[0]!);
  return determinant / Math.max(1, (a[0]! + b[1]! + c[2]!) ** 3);
}

test("topology: settled AS clusters and their members occupy a volume instead of any plane", () => {
  const { graph } = setup();
  const d = populate(graph, 600);
  assert.ok(volume(graph.bubbles()) > 0.002, "AS centers have substantial depth independent of camera rotation");
  const members = [...graph.vertices.values()].filter((v: any) => v.group === "as0");
  assert.ok(volume(members as any) > 0.001, "the seven members of an AS spread through all three dimensions");
  const positions = [...graph.vertices.values()].map((v: any) => [v.id, v.x, v.y, v.z]);
  graph.setData([...d.vertices].reverse(), [...d.links].reverse(), [...d.groups].reverse());
  for (let frame = 0; frame < 60; frame++) graph.tick();
  assert.deepEqual([...graph.vertices.values()].map((v: any) => [v.id, v.x, v.y, v.z]), positions, "polling does not restart the settled volume");
  graph.destroy();
});

test("topology: cluster collision separates bodies along depth and resolves exact coincidence deterministically", () => {
  const run = (coincident: boolean) => {
    const { graph } = setup();
    graph.setData(["a", "b"].map((id) => ({ id, group: id, label: id, radius: 10, shape: "circle", color: "#5cb" })), [], ["a", "b"].map((id) => ({ id, label: id, color: "#5cb" })));
    for (const [i, v] of [...graph.vertices.values()].entries()) Object.assign(v as object, { x: 0, y: 0, z: coincident ? 0 : i ? 10 : -10, vx: 0, vy: 0, vz: 0 });
    graph.anchors.set("group:a", { x: 0, y: 0, z: 0 }); graph.anchors.set("group:b", { x: 0, y: 0, z: 0 });
    for (let frame = 0; frame < 30; frame++) graph.tick();
    const a = graph.vertices.get("a"), b = graph.vertices.get("b");
    const result = [a.x, a.y, a.z, b.x, b.y, b.z];
    assert.ok(result.every(Number.isFinite));
    if (!coincident) {
      assert.ok(Math.abs(b.z - a.z) > 100, "depth-aligned overlapping spheres must separate along Z");
      assert.ok(Math.abs(a.x) + Math.abs(a.y) + Math.abs(b.x) + Math.abs(b.y) < 0.001, "a Z collision must not invent a sideways displacement");
    }
    graph.destroy();
    return result;
  };
  run(false);
  assert.deepEqual(run(true), run(true), "coincident bodies use a stable three-dimensional direction");
});

test("topology: node dragging follows the screen at every orbit angle, including after report refresh", () => {
  for (const yaw of [-3, -1, 0, 1, 3]) for (const pitch of [-0.9, 0.9]) {
    const { graph, event } = setup();
    const d = populate(graph);
    graph.yaw = yaw; graph.pitch = pitch; graph.fitGraph();
    const v = [...graph.vertices.values()].find((node: any) => {
      const p = graph.project(node.x, node.y, node.z);
      return p.visible && graph.hit(p.sx, p.sy)?.id === node.id;
    }) as any;
    assert.ok(v, "a visible foreground node is pickable at this orbit angle");
    const p = graph.project(v.x, v.y, v.z);
    event("pointerdown", { clientX: p.sx, clientY: p.sy });
    assert.equal(graph.drag?.kind, "vertex");
    graph.setData(d.vertices.map((node) => ({ ...node, label: node.label + " refreshed" })), d.links, d.groups);
    assert.equal(graph.drag.v, graph.vertices.get(v.id), "polling preserves the dragged object");
    event("pointermove", { clientX: p.sx + 72, clientY: p.sy + 29 });
    graph.tick();
    const moved = graph.project(v.x, v.y, v.z);
    assert.ok(Math.abs(moved.sx - p.sx - 72) < 0.001);
    assert.ok(Math.abs(moved.sy - p.sy - 29) < 0.001);
    event("pointerup");
    assert.equal(v.pinned, false);
    graph.destroy();
  }
});

test("topology: manual zoom stays put and stays anchored at the pointer", () => {
  const { graph, event } = setup();
  populate(graph);
  const x = 740, y = 350;
  const local = graph.unrotate((x - 600) * graph.distance / 600, (y - 325) * graph.distance / 600, 0);
  const world = { x: local[0] + graph.target.x, y: local[1] + graph.target.y, z: local[2] + graph.target.z };
  const before = graph.distance;
  event("wheel", { clientX: x, clientY: y, deltaY: -100, deltaMode: 0 });
  assert.ok(graph.distance < before, "wheel up zooms in");
  const anchor = graph.project(world.x, world.y, world.z);
  assert.ok(Math.abs(anchor.sx - x) < 0.001 && Math.abs(anchor.sy - y) < 0.001);
  const distance = graph.distance;
  for (let i = 0; i < 500; i++) graph.tick();
  assert.equal(graph.distance, distance, "auto-fit cannot undo the zoom");
  graph.destroy();
});

test("topology: selecting a node does not reheat the settled physical layout", () => {
  const { graph, event, clicks } = setup();
  populate(graph);
  const node = [...graph.vertices.values()].find((v: any) => {
    const p = graph.project(v.x, v.y, v.z);
    return p.visible && graph.hit(p.sx, p.sy)?.id === v.id;
  }) as any;
  const p = graph.project(node.x, node.y, node.z), alpha = graph.alpha;
  const positions = [...graph.vertices.values()].map((v: any) => [v.id, v.x, v.y, v.z]);
  event("pointerdown", { clientX: p.sx, clientY: p.sy });
  event("pointerup", { clientX: p.sx, clientY: p.sy });
  assert.equal(clicks(), 1);
  assert.equal(graph.alpha, alpha);
  assert.equal(graph.alphaTarget, 0);
  for (let i = 0; i < 30; i++) graph.tick();
  assert.deepEqual([...graph.vertices.values()].map((v: any) => [v.id, v.x, v.y, v.z]), positions);
  graph.destroy();
});

test("topology: panning has a fixed screen direction and orbit cannot cross a pole", () => {
  const { graph, event } = setup();
  populate(graph);
  const yaw = graph.yaw, pitch = graph.pitch;
  event("pointerdown", { button: 2, clientX: 20, clientY: 20 });
  event("pointermove", { clientX: 100, clientY: 50 });
  assert.equal(graph.panX, 80); assert.equal(graph.panY, 30);
  assert.equal(graph.yaw, yaw); assert.equal(graph.pitch, pitch);
  event("pointerup", { button: 2 });
  event("pointerdown"); event("pointermove", { clientY: 10000 }); event("pointerup");
  assert.ok(graph.pitch <= Math.PI / 3 && graph.pitch > 0);
  graph.destroy();
});

test("topology: touch pinch is zoom/pan, and cancelled or accumulated drags do not click", () => {
  const { graph, event, clicks, captured } = setup();
  populate(graph);
  event("pointerdown");
  for (let i = 1; i <= 8; i++) event("pointermove", { clientX: 5 + i });
  event("pointerup");
  assert.equal(clicks(), 0, "small pointer increments still add up to a drag");
  event("pointerdown"); event("pointercancel"); assert.equal(clicks(), 0);
  event("pointerdown", { pointerId: 1, clientX: 20, clientY: 20 });
  event("pointerdown", { pointerId: 2, clientX: 100, clientY: 20 });
  const distance = graph.distance;
  event("pointermove", { pointerId: 2, clientX: 150, clientY: 20 });
  assert.ok(graph.distance < distance);
  event("pointerup", { pointerId: 2 }); event("pointerup", { pointerId: 1 });
  assert.equal(clicks(), 0); assert.equal(captured.size, 0);
  graph.destroy();
});

test("topology: focusing and reset do not disturb settled layout; behind-camera nodes cannot be picked", () => {
  const { graph } = setup();
  populate(graph);
  const v = graph.vertices.get("as0-0");
  const alpha = graph.alpha;
  graph.focus({ kind: "vertex", id: v.id });
  assert.ok(graph.distance < 250, "node focus does not frame the whole AS");
  graph.resetView();
  assert.equal(graph.alpha, alpha);
  assert.equal(graph.autoRotate, false);
  graph.yaw = graph.pitch = 0;
  v.x = graph.target.x; v.y = graph.target.y; v.z = graph.target.z + graph.distance + 100;
  assert.equal(graph.project(v.x, v.y, v.z).visible, false);
  assert.notDeepEqual(graph.hit(600, 325), { kind: "vertex", id: v.id });
  graph.draw();
  graph.destroy();
});

test("topology: reordered heartbeat data updates status without reheating or moving settled vertices", () => {
  const { graph } = setup();
  const d = populate(graph, 500);
  const alpha = graph.alpha;
  const positions = [...graph.vertices.values()].map((v: any) => [v.id, v.x, v.y, v.z]);
  graph.setData([...d.vertices].reverse().map((v) => ({ ...v, ring: "#f00" })), [...d.links].reverse(), [...d.groups].reverse());
  assert.equal(graph.alpha, alpha, "array order and health colors are not structural changes");
  for (let frame = 0; frame < 30; frame++) graph.tick();
  for (const [id, x, y, z] of positions) {
    const v = graph.vertices.get(id);
    assert.deepEqual([v.x, v.y, v.z], [x, y, z]);
    assert.equal(v.ring, "#f00");
  }
  graph.setData(d.vertices, d.links.slice(1), d.groups);
  assert.ok(graph.alpha > alpha, "an actual removed link still updates layout");
  graph.destroy();
});

test("topology: labels retain their placement and remain visible through hover and depth crossings", () => {
  const { graph, labels } = setup(800, 500);
  graph.setData(["a", "b", "c"].map((id) => ({ id, label: `10.42.10.${id}`, radius: 10, shape: "circle", color: "#5cb" })), []);
  graph.yaw = graph.pitch = 0; graph.distance = 600; graph.target = { x: 0, y: 0, z: 0 };
  for (const [index, v] of [...graph.vertices.values()].entries()) Object.assign(v as object, { x: index * 20, y: 0, z: 0 });
  graph.draw();
  const initial = structuredClone(labels).sort((a, b) => a.text.localeCompare(b.text));
  assert.equal(initial.length, 3, "crowding cannot silently hide an IP address");
  graph.hover = { kind: "vertex", id: "c" }; graph.highlight = "c";
  graph.draw();
  assert.deepEqual([...labels].sort((a, b) => a.text.localeCompare(b.text)), initial, "selection cannot rearrange neighboring labels");
  graph.vertices.get("a").z = 0.001;
  graph.vertices.get("b").z = -0.001;
  graph.draw();
  assert.equal(labels.length, 3);
  for (const label of labels) {
    const before = initial.find((l) => l.text === label.text)!;
    assert.ok(Math.hypot(label.x - before.x, label.y - before.y) < 0.01, "a depth tie cannot flip label placement");
  }
  graph.destroy();
});

test("topology: viewport resizing refits a framed graph and does not erase an unchanged canvas", () => {
  const { graph, resize, clears } = setup(1200, 650);
  populate(graph);
  graph.fitGraph();
  const alpha = graph.alpha;
  const before = clears();
  resize(1200, 650);
  assert.equal(clears(), before, "unchanged ResizeObserver notifications cannot clear the bitmap");
  resize(480, 320);
  assert.ok(clears() > before, "a real resize redraws immediately");
  for (const v of graph.vertices.values()) {
    const p = graph.project(v.x, v.y, v.z);
    assert.ok(p.visible && p.sx >= 0 && p.sx <= 480 && p.sy >= 0 && p.sy <= 320, "framed nodes fit the new viewport");
  }
  assert.equal(graph.alpha, alpha, "resize must not restart the physical layout");
  graph.zoomBy(0.8); graph.panX = 20; graph.panY = 10;
  const distance = graph.distance;
  const target = { ...graph.target }, yaw = graph.yaw, pitch = graph.pitch;
  resize(960, 640);
  assert.equal(graph.distance, distance / 2, "manual zoom adapts in proportion to viewport size");
  assert.equal(graph.panX, 40); assert.equal(graph.panY, 20);
  assert.deepEqual(graph.target, target); assert.equal(graph.yaw, yaw); assert.equal(graph.pitch, pitch);
  resize(480, 320);
  assert.equal(graph.distance, distance, "resize round trips do not accumulate camera drift");
  graph.destroy();
});

test("topology: label panels are translucent and foreground nodes/links occlude distant labels", () => {
  const { graph, paints } = setup(800, 500);
  graph.setData([
    { id: "far", label: "Distant IP", radius: 10, shape: "circle", color: "#far" },
    { id: "near", label: "Near router", radius: 10, shape: "circle", color: "#near" },
    { id: "left", label: "Left", radius: 10, shape: "circle", color: "#left" },
    { id: "right", label: "Right", radius: 10, shape: "circle", color: "#right" },
  ], [{ id: "front", source: "left", target: "right", color: "#front-link", length: 100, width: 2, particles: 0 }]);
  graph.yaw = graph.pitch = 0; graph.distance = 600; graph.target = { x: 0, y: 0, z: 0 };
  Object.assign(graph.vertices.get("far"), { x: 0, y: 0, z: 0 });
  Object.assign(graph.vertices.get("near"), { x: 0, y: 16, z: 200 });
  Object.assign(graph.vertices.get("left"), { x: -100, y: 16, z: 200 });
  Object.assign(graph.vertices.get("right"), { x: 100, y: 16, z: 200 });
  graph.labelSlots.set("far", 0); // Force an overlap to exercise depth handling even when no clear slot exists.
  graph.draw();
  const label = paints.findIndex((p) => p.kind === "text" && p.text === "Distant IP");
  const node = paints.findIndex((p) => p.kind === "fill" && p.color === "#near");
  const link = paints.findIndex((p) => p.kind === "stroke" && p.color === "#front-link");
  assert.ok(label >= 0 && node > label, "a distant label cannot paint over a nearer router");
  assert.ok(link > label, "a distant label cannot erase a nearer link");
  const backgrounds = paints.filter((p) => p.kind === "background");
  assert.ok(backgrounds.length > 0 && backgrounds.every((p) => p.alpha > 0 && p.alpha <= 0.25), "background panels preserve the scene beneath them");
  graph.destroy();
});

test("topology: label placement avoids a crossing link and keeps its choice through hover", () => {
  const { graph, labels } = setup(800, 500);
  graph.setData(["a", "left", "right"].map((id) => ({ id, label: id, radius: 10, shape: "circle", color: "#5cb" })), [
    { id: "crossing", source: "left", target: "right", color: "#front-link", length: 100, width: 2, particles: 0 },
  ]);
  graph.yaw = graph.pitch = 0; graph.distance = 600; graph.target = { x: 0, y: 0, z: 0 };
  Object.assign(graph.vertices.get("a"), { x: 0, y: 0, z: 0 });
  Object.assign(graph.vertices.get("left"), { x: -200, y: 24.5, z: 0 });
  Object.assign(graph.vertices.get("right"), { x: 200, y: 24.5, z: 0 });
  graph.draw();
  const before = { ...labels.find((l) => l.text === "a")! };
  assert.ok(before.y < 250, "choose the clear side instead of covering the link below the router");
  graph.hover = { kind: "vertex", id: "a" }; graph.draw();
  assert.deepEqual(labels.find((l) => l.text === "a"), before);
  graph.destroy();
});

test("topology: depth-spanning links draw their far and near sections on either side of labels", () => {
  const { graph, paints, labels } = setup(800, 500);
  graph.setData(["a", "left", "right"].map((id) => ({ id, label: id, radius: 10, shape: "circle", color: "#5cb" })), [
    { id: "depth", source: "left", target: "right", color: "#depth-link", length: 100, width: 2, particles: 0, dash: [4, 4] },
  ]);
  graph.yaw = graph.pitch = 0; graph.distance = 600; graph.target = { x: 0, y: 0, z: 0 };
  Object.assign(graph.vertices.get("a"), { x: 0, y: 0, z: 0 });
  Object.assign(graph.vertices.get("left"), { x: -200, y: 24.5, z: -300 });
  Object.assign(graph.vertices.get("right"), { x: 200, y: 24.5, z: 300 });
  graph.draw();
  const label = paints.findIndex((p) => p.kind === "text" && p.text === "a");
  assert.ok(paints.slice(0, label).some((p) => p.kind === "stroke" && p.color === "#depth-link"));
  assert.ok(paints.slice(label + 1).some((p) => p.kind === "stroke" && p.color === "#depth-link"));
  graph.labelSlots.set("a", 2);
  graph.vertices.get("a").x = 360;
  graph.draw();
  const before = labels.find((l) => l.text === "a")!.x;
  graph.vertices.get("a").x += 10;
  graph.draw();
  assert.equal(labels.find((l) => l.text === "a")!.x - before, 10, "a label follows its 3D object instead of sticking to a viewport edge");
  graph.destroy();
});

test("topology: AS titles and names occupy separate lines in clusters and collapsed vertices", () => {
  const { graph, labels } = setup(800, 500);
  graph.setData([
    { id: "peer", label: "router.example", group: "asn:1", radius: 10, shape: "triangle", color: "#5cb" },
    { id: "as:2", label: "AS65200", subtitle: "Transit network", labelPriority: 1, radius: 10, shape: "diamond", color: "#5cb" },
  ], [], [{ id: "asn:1", label: "AS65100", subtitle: "Upstream network", color: "#5cb" }]);
  graph.yaw = graph.pitch = 0; graph.distance = 600; graph.target = { x: 0, y: 0, z: 0 };
  Object.assign(graph.vertices.get("peer"), { x: -120, y: 0, z: 0 });
  Object.assign(graph.vertices.get("as:2"), { x: 120, y: 0, z: 0 });
  graph.draw();
  for (const [title, name] of [["AS65100", "Upstream network"], ["AS65200", "Transit network"]]) {
    const a = labels.find((l) => l.text === title), b = labels.find((l) => l.text === name);
    assert.ok(a && b, "both AS number and name are drawn");
    assert.equal(a.x, b.x);
    assert.ok(b.y > a.y + 10, "name is below the AS number");
  }
  graph.destroy();
});

test("topology: cached router labels move out of AS title/name blocks when those blocks grow", () => {
  const { graph, labels } = setup(800, 500);
  graph.setData([
    { id: "a", label: "router.example", radius: 10, shape: "triangle", color: "#5cb" },
    { id: "b", label: "AS65100", subtitle: "Upstream network", labelPriority: 1, radius: 10, shape: "diamond", color: "#5cb" },
  ], []);
  graph.yaw = graph.pitch = 0; graph.distance = 600; graph.target = { x: 0, y: 0, z: 0 };
  Object.assign(graph.vertices.get("a"), { x: 0, y: 0, z: 0 });
  Object.assign(graph.vertices.get("b"), { x: 0, y: -25, z: 0 });
  graph.labelSlots.set("a", 1); // previously clear placement now occupied by a named AS
  graph.draw();
  const as = labels.find((l) => l.text === "AS65100")!, name = labels.find((l) => l.text === "Upstream network")!;
  const router = labels.find((l) => l.text === "router.example")!;
  assert.ok(as && name);
  const bounds = { left: as.x - 5, right: as.x + "Upstream network".length * 7 + 5, top: as.y - 10, bottom: name.y + 10 };
  assert.ok(router.x + "router.example".length * 7 < bounds.left || router.x > bounds.right || router.y + 9 < bounds.top || router.y - 9 > bounds.bottom,
    "router ID/IP/hostname text cannot overlap either AS line");
  const position = { ...router };
  graph.hover = { kind: "vertex", id: "b" }; graph.highlight = "b"; graph.draw();
  assert.deepEqual(labels.find((l) => l.text === "router.example"), position, "hover keeps the clear placement stable");
  graph.destroy();
});

test("topology: a renamed AS cluster invalidates overlapping router label slots without reheating layout", () => {
  const { graph, labels } = setup(800, 500);
  const vertices = [
    { id: "member", label: "member", group: "asn:1", radius: 10, shape: "circle", color: "#5cb" },
    { id: "external", label: "203.0.113.1", radius: 10, shape: "triangle", color: "#5cb" },
  ];
  graph.setData(vertices, [], [{ id: "asn:1", label: "AS65100", subtitle: "ISP", color: "#5cb" }]);
  graph.yaw = graph.pitch = 0; graph.distance = 600; graph.target = { x: 0, y: 0, z: 0 };
  Object.assign(graph.vertices.get("member"), { x: 0, y: 0, z: 0 });
  Object.assign(graph.vertices.get("external"), { x: 50, y: -69, z: 0 });
  graph.labelSlots.set("external", 2);
  graph.draw();
  assert.equal(graph.labelSlots.get("external"), 2, "short name leaves the previous slot clear");
  graph.alpha = 0;
  const name = "Long registered upstream network name";
  graph.setData(vertices, [], [{ id: "asn:1", label: "AS65100", subtitle: name, color: "#5cb" }]);
  graph.draw();
  assert.equal(graph.alpha, 0, "renaming metadata does not restart physics");
  assert.notEqual(graph.labelSlots.get("external"), 2, "a cached slot must avoid the longer subtitle");
  const title = labels.find((l) => l.text === "AS65100")!, subtitle = labels.find((l) => l.text === name)!;
  const router = labels.find((l) => l.text === "203.0.113.1")!;
  const left = title.x - name.length * 7 / 2 - 8, right = title.x + name.length * 7 / 2 + 8;
  assert.ok(router.x + "203.0.113.1".length * 7 < left || router.x > right || router.y + 9 < title.y - 12 || router.y - 9 > subtitle.y + 12);
  graph.destroy();
});

test("topology: node panels share AS rendering, reveal by zoom/hover/selection and never draw leader lines", () => {
  const { graph, labels, paints, event } = setup(1000, 650);
  graph.setData([
    { id: "node", label: "Managed router", primary: true, radius: 10, shape: "circle", color: "#node" },
    { id: "as", label: "AS65001", labelPriority: 1, radius: 10, shape: "diamond", color: "#as" },
  ], []);
  graph.yaw = graph.pitch = 0; graph.distance = 2000; graph.target = { x: 0, y: 0, z: 0 };
  Object.assign(graph.vertices.get("node"), { x: -200, y: 0, z: 0 });
  Object.assign(graph.vertices.get("as"), { x: 200, y: 0, z: 0 });
  graph.draw();
  assert.deepEqual(labels.map((l) => l.text), ["AS65001"], "an overview keeps AS landmarks without router-label clutter");
  const node = graph.vertices.get("node"), point = graph.project(node.x, node.y, node.z);
  event("pointermove", { clientX: point.sx, clientY: point.sy }); graph.draw();
  assert.ok(labels.some((l) => l.text === "Managed router"), "hover immediately reveals an overview node");
  const style = (text: string) => paints.find((p) => p.text === text)!;
  assert.equal(style("Managed router").font, style("AS65001").font);
  assert.equal(style("Managed router").align, "center");
  assert.ok(paints.filter((p) => p.kind === "background").every((p) => p.alpha <= 0.16));
  const panel = graph.labelHits.find((p: any) => p.item.kind === "vertex" && p.item.id === "node");
  event("pointermove", { clientX: panel.x + panel.w / 2, clientY: panel.y + panel.h / 2 }); graph.draw();
  assert.equal(graph.hover?.id, "node", "moving from a node onto its panel retains hover and detail visibility");
  let clicked: any = null;
  graph.opts.onClick = (item: any) => { clicked = item; };
  event("pointerdown", { clientX: panel.x + panel.w / 2, clientY: panel.y + panel.h / 2 }); graph.draw();
  assert.ok(labels.some((l) => l.text === "Managed router"), "a pointer press retains the panel until release");
  event("pointerup", { clientX: panel.x + panel.w / 2, clientY: panel.y + panel.h / 2 });
  assert.deepEqual(clicked, { kind: "vertex", id: "node" }, "press/release selects the label's owner even if a frame cleared hover");
  event("pointerleave"); graph.draw();
  assert.ok(!labels.some((l) => l.text === "Managed router"), "hover is not a persistent detail toggle");
  graph.highlight = "node"; graph.labelSlots.set("node", 10); graph.draw();
  assert.ok(labels.some((l) => l.text === "Managed router"), "click selection keeps the node panel visible");
  assert.equal(paints.filter((p) => p.kind === "stroke" && p.color === "#fff").length, 1, "only the selection ring is stroked, never a panel leader line");
  graph.highlight = null; graph.distance = 600; graph.draw();
  assert.ok(labels.some((l) => l.text === "Managed router"), "zooming in reveals ordinary node labels");
  graph.distance = 850; graph.draw();
  assert.ok(labels.some((l) => l.text === "Managed router"), "detail hysteresis prevents flicker around the zoom threshold");
  graph.distance = 1200; graph.draw();
  assert.ok(!labels.some((l) => l.text === "Managed router"), "zooming well out returns to the AS overview");
  graph.destroy();
});

test("topology: dragging a node by its visible label preserves the pointer offset", () => {
  const { graph, event } = setup(800, 500);
  graph.setData([{ id: "a", label: "Label drag target", primary: true, radius: 10, shape: "circle", color: "#5cb" }], []);
  const v = graph.vertices.get("a");
  graph.yaw = graph.pitch = 0; graph.distance = 600; graph.target = { x: 0, y: 0, z: 0 };
  Object.assign(v, { x: 0, y: 0, z: 0 }); graph.draw();
  const panel = graph.labelHits.find((p: any) => p.item.id === "a");
  const x = panel.x + panel.w / 2, y = panel.y + panel.h / 2;
  const before = graph.project(v.x, v.y, v.z);
  event("pointerdown", { clientX: x, clientY: y });
  assert.equal(graph.drag?.kind, "vertex");
  event("pointermove", { clientX: x + 50, clientY: y + 20 }); graph.tick();
  const after = graph.project(v.x, v.y, v.z);
  assert.ok(Math.abs(after.sx - before.sx - 50) < 0.001 && Math.abs(after.sy - before.sy - 20) < 0.001, "the router does not jump to the label's offset");
  event("pointerup"); graph.destroy();
});

test("topology: close AS clusters avoid each other's complete title/name panels", () => {
  const { graph } = setup(800, 500);
  graph.setData(["a", "b"].map((id) => ({ id, group: id, label: `router-${id}`, radius: 10, shape: "circle", color: "#5cb" })), [],
    ["a", "b"].map((id, i) => ({ id, label: `AS${65000+i}`, subtitle: `Registered network ${id}`, color: "#5cb" })));
  graph.yaw = graph.pitch = 0; graph.distance = 600; graph.target = { x: 0, y: 0, z: 0 };
  Object.assign(graph.vertices.get("a"), { x: 0, y: 0, z: 0 }); Object.assign(graph.vertices.get("b"), { x: 25, y: 0, z: 0 }); graph.draw();
  const panels = graph.labelHits.filter((p: any) => p.item.kind === "group");
  assert.equal(panels.length, 2);
  const [a, b] = panels;
  assert.ok(a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y, "neither AS line overlaps the other group panel");
  graph.hover = { kind: "group", id: "a" }; graph.draw();
  assert.deepEqual(graph.labelHits.filter((p: any) => p.item.kind === "group"), panels, "hover does not relocate the separated headings");
  graph.destroy();
});

test("topology: reset/default frames configured direct endpoints and isolated managed nodes; fit includes remote paths", () => {
  const { graph, resize, event } = setup(1100, 650);
  const vertices = ["root", "peer", "isolated", "remote1", "remote2"].map((id) => ({
    id, label: id, primary: id === "root" || id === "isolated", radius: 10, shape: "circle", color: "#5cb",
  }));
  const link = (id: string, source: string, target: string, direct: boolean) => ({ id, source, target, direct, color: "#5cb", width: 1, particles: 0, length: 400 });
  const links = [link("configured-down-peer", "root", "peer", true), link("path1", "peer", "remote1", false), link("path2", "remote1", "remote2", false)];
  graph.setData(vertices, links);
  assert.equal(graph.viewFrame, "direct", "first load starts in direct mode");
  for (const [id, x] of [["root", 0], ["peer", 400], ["isolated", -200], ["remote1", 3000], ["remote2", 8000]] as const)
    Object.assign(graph.vertices.get(id), { x, y: 0, z: 0 });
  graph.alpha = 0; graph.resetView();
  const directDistance = graph.distance, directTarget = { ...graph.target };
  assert.deepEqual(graph.framedVertices().map((v: any) => v.id).sort(), ["isolated", "peer", "root"]);
  assert.ok(graph.project(8000, 0, 0).sx > 1100, "remote paths do not shrink the default view");
  graph.fitGraph();
  assert.equal(graph.viewFrame, "all");
  assert.ok(graph.distance > directDistance * 5);
  for (const v of graph.vertices.values()) {
    const p = graph.project(v.x, v.y, v.z);
    assert.ok(p.visible && p.sx >= 0 && p.sx <= 1100, "fit really includes every remote AS");
  }
  graph.setData([...vertices].reverse(), [...links].reverse());
  assert.equal(graph.viewFrame, "all", "heartbeat refresh preserves the chosen all-graph frame");
  event("keydown", { key: "0" });
  assert.equal(graph.viewFrame, "direct");
  assert.equal(graph.distance, directDistance); assert.deepEqual(graph.target, directTarget);
  resize(500, 400);
  assert.equal(graph.viewFrame, "direct");
  assert.ok(graph.project(8000, 0, 0).sx > 500, "resize preserves direct scope");
  graph.setSpacing(2.6);
  assert.equal(graph.viewFrame, "direct", "spacing cannot silently switch to full-graph framing");
  graph.destroy();
});

test("topology: AS paths continue outward by hop count instead of folding into the local shell", () => {
  const { graph } = setup();
  const ids = ["managed", "peer:65001", "as:90", "as:40", "as:1", "branch"];
  graph.setData(ids.map((id) => ({ id, label: id, primary: id === "managed", radius: 10, shape: "circle", color: "#5cb" })), [
    ["managed", "peer:65001"], ["peer:65001", "as:90"], ["as:90", "as:40"], ["as:40", "as:1"], ["peer:65001", "branch"],
  ].map(([source, target], index) => ({ id: `edge${index}`, source, target, direct: index === 0, length: 200, width: 1, particles: 0, color: "#5cb" })));
  for (let tick = 0; tick < 600; tick++) graph.tick();
  const root = graph.vertices.get("managed");
  let previous = 0;
  for (const id of ids.slice(1, 5)) {
    const v = graph.vertices.get(id);
    const distance = Math.hypot(v.x - root.x, v.y - root.y, v.z - root.z);
    assert.ok(distance > previous + 300, `${id} remains further outward than its predecessor`);
    previous = distance;
  }
  const positions = [...graph.vertices.values()].map((v: any) => [v.id, v.x, v.y, v.z]);
  graph.setData([...graph.vertices.values()].reverse(), [...graph.links].reverse());
  for (let tick = 0; tick < 30; tick++) graph.tick();
  assert.deepEqual([...graph.vertices.values()].map((v: any) => [v.id, v.x, v.y, v.z]), positions, "unchanged reports preserve the outward layout");
  graph.destroy();
});
