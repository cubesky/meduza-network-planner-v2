import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { ModuleKind, ScriptTarget, transpileModule } from "typescript";
import { REPO_ROOT } from "./helpers.js";

const source = readFileSync(join(REPO_ROOT, "packages/meduza-np-manager/web/src/node-status.ts"), "utf8");
const compiled = transpileModule(source, { compilerOptions: { target: ScriptTarget.ES2023, module: ModuleKind.ESNext } }).outputText;
const { needsAttention, presenceTone, isOffline } = await import(`data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`);
const node = { behavior: "roaming", freshness: "offline", problems: [], desiredRevision: "rev-1", failedRevision: null };

test("roaming absence stays neutral through stale/offline/never without raising an outage alert", () => {
  for (const freshness of ["stale", "offline", "never"]) {
    const n = { ...node, freshness };
    assert.equal(needsAttention(n), false);
    assert.equal(presenceTone(n), "idle");
  }
  assert.equal(presenceTone({ ...node, freshness: "online" }), undefined);
  assert.equal(needsAttention({ ...node, behavior: "static" }), true);
  assert.equal(presenceTone({ ...node, behavior: "static" }), undefined);
  assert.equal(presenceTone({ ...node, freshness: "revoked" }), undefined);
});

test("roaming does not suppress explicit installation/service errors or current apply failures", () => {
  for (const freshness of ["online", "stale", "offline"]) {
    assert.equal(needsAttention({ ...node, freshness, problems: [{ id: "updater.installation" }] }), true);
    assert.equal(needsAttention({ ...node, freshness, failedRevision: "rev-1" }), true);
    assert.equal(needsAttention({ ...node, freshness, failedRevision: "rev-0" }), false);
  }
});

test("offline filtering agrees with overview counts including enrolled nodes with no contact", () => {
  for (const freshness of ["offline", "never"]) assert.equal(isOffline({ freshness }), true);
  for (const freshness of ["online", "stale", "not-enrolled", "revoked"]) assert.equal(isOffline({ freshness }), false);
});
