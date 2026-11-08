import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { ModuleKind, ScriptTarget, transpileModule } from "typescript";
import { REPO_ROOT } from "./helpers.js";

// Exercise the model shared by both frontends without introducing a second tree implementation in tests.
const source = readFileSync(join(REPO_ROOT, "web/ui-kit/src/instance-tree-model.ts"), "utf8");
const compiled = transpileModule(source, { compilerOptions: { target: ScriptTarget.ES2023, module: ModuleKind.ESNext } }).outputText;
const { buildInstanceTree, withWireGuardNames } = await import(`data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`);
interface Tree { key: string; path: string; label: string; item?: Record<string, unknown>; children: Tree[] }
const flat = (roots: Tree[]): Tree[] => roots.flatMap((node) => [node, ...flat(node.children)]);

test("status tree: repeated endpoint helpers belong to their named WireGuard interfaces", () => {
  const reports = [
    { id: "wireguard/branch/endpoints/process", kind: "process", instance: "wireguard/branch", state: "up" },
    { id: "wireguard/home/endpoints/process", kind: "process", instance: "wireguard/home", state: "up" },
    { id: "wireguard/branch", kind: "wireguard-interface", name: "Branch office", state: "up" },
    { id: "wireguard/home", kind: "wireguard-interface", name: "Home router", state: "down" },
    // A peer may have the same id as the endpoint helper; both independent states must survive.
    { id: "wireguard/branch/endpoints", kind: "wireguard-peer", name: "Upstream router", state: "down", reason: "Name does not resolve" },
  ];
  const tree: Tree[] = buildInstanceTree(reports);
  assert.equal(tree.length, 1);
  const [branch, home] = tree[0]!.children;
  assert.equal(branch!.label, "Branch office");
  assert.equal(home!.label, "Home router");
  assert.deepEqual(branch!.children.map((node) => node.item?.kind).sort(), ["process", "wireguard-peer"]);
  assert.equal(home!.children[0]!.item, reports[1]);
  const rows = flat(tree).filter((node) => node.item);
  assert.equal(rows.length, reports.length, "no status is hidden by grouping or a process/peer id collision");
  assert.equal(new Set(flat(tree).map((node) => node.key)).size, flat(tree).length);
  assert.deepEqual(buildInstanceTree([...reports].reverse()), tree, "polling order cannot reshuffle branches");
  assert.equal(reports[0]!.id, "wireguard/branch/endpoints/process", "display names never rewrite restart identities");
});

test("status tree: explicit process ownership and missing probes retain all independent states", () => {
  const reports = [
    { id: "frr/bgpd/process", kind: "process", instance: "frr/main", state: "up" },
    { id: "mosdns/main/process", kind: "process", instance: "mosdns/main", state: "up" },
    { id: "mosdns/main", kind: "mosdns", state: "down", reason: "DNS not answering" },
    { id: "wireguard/starting/endpoints/process", kind: "process", state: "starting" },
  ];
  const tree: Tree[] = buildInstanceTree(reports);
  const nodes = flat(tree);
  const frr = nodes.find((node) => node.path === "frr/main")!;
  assert.equal(frr.item, undefined, "a virtual owner is not a fabricated healthy service");
  assert.equal(frr.children[0]!.path, "frr/bgpd");
  assert.equal(nodes.filter((node) => node.path === "frr/bgpd").length, 1);
  const mosdns = nodes.find((node) => node.path === "mosdns/main")!;
  assert.equal(mosdns.item?.state, "down");
  assert.equal(mosdns.children[0]!.item?.state, "up", "a running process does not imply a healthy resolver");
  const pending = nodes.find((node) => node.path === "wireguard/starting")!;
  assert.equal(pending.children[0]!.item?.id, reports[3]!.id, "old reports without owner metadata remain visible");
  assert.equal(nodes.filter((node) => node.item).length, reports.length);
  assert.deepEqual(buildInstanceTree([]), []);
});

test("status tree: old WireGuard reports use published names, while reported names and ids remain authoritative", () => {
  const reports = [
    { id: "wireguard/branch", kind: "wireguard-interface", state: "up" },
    { id: "wireguard/branch/main", kind: "wireguard-peer", name: "Observed peer", state: "down" },
    { id: "wireguard/branch/endpoints/process", kind: "process", instance: "wireguard/branch", state: "up" },
  ];
  const config = { instances: [{ id: "branch", name: "Published office", peers: [{ id: "main", name: "Published peer" }] }] };
  const rows = withWireGuardNames(reports, config);
  assert.equal(rows[0].name, "Published office");
  assert.equal(rows[1].name, "Observed peer");
  assert.equal(rows[1].interfaceName, "Published office");
  assert.equal(rows[2], reports[2], "do not rename process identities");
  assert.deepEqual(rows.map((row: Record<string, unknown>) => row.id), reports.map((row) => row.id));
  assert.equal("name" in reports[0]!, false, "do not mutate the original diagnostic report");
  assert.equal(withWireGuardNames(reports, undefined), reports);
});
