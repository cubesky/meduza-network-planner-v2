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
  const ctx = new Proxy({ measureText: (s: string) => ({ width: s.length * 7 }) }, { get: (target: any, key) => target[key] ?? (() => {}) });
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
  return { graph, event, events, captured, clicks: () => clicks };
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

test("topology: clusters remain visibly separated and fit a wide viewport", () => {
  const { graph } = setup(1300, 560);
  populate(graph);
  graph.fitGraph();
  const bubbles = graph.bubbles().map((b: any) => ({ ...graph.project(b.x, b.y, b.z), r: b.r }));
  for (let i = 0; i < bubbles.length; i++) {
    const a = bubbles[i]!;
    assert.ok(a.visible);
    assert.ok(a.sx - a.r * a.scale > 25 && a.sx + a.r * a.scale < 1275);
    assert.ok(a.sy - a.r * a.scale > 25 && a.sy + a.r * a.scale < 535);
    for (const b of bubbles.slice(i + 1)) assert.ok(Math.hypot(a.sx - b.sx, a.sy - b.sy) > a.r * a.scale + b.r * b.scale + 12, "clusters have a real screen-space gap");
  }
  const before = Math.hypot(graph.vertices.get("as1-0").x, graph.vertices.get("as1-0").y);
  graph.setSpacing(2.5);
  for (let i = 0; i < 400; i++) graph.tick();
  assert.ok(Math.hypot(graph.vertices.get("as1-0").x, graph.vertices.get("as1-0").y) > before * 1.2);
  graph.destroy();
});

test("topology: node dragging follows the screen at every orbit angle, including after report refresh", () => {
  for (const yaw of [-3, -1, 0, 1, 3]) for (const pitch of [-0.9, 0.9]) {
    const { graph, event } = setup();
    const d = populate(graph);
    graph.yaw = yaw; graph.pitch = pitch; graph.fitGraph();
    const v = graph.vertices.get("as0-0");
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
