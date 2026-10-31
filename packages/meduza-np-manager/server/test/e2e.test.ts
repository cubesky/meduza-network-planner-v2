// End-to-end: the real planner binary enrolls with the real manager, opens the push channel, receives a
// published configuration, applies it and ACKs, and reports its packages. Requires `cargo build -p
// meduza-np-planner` first; CI fails instead of skipping when the binary is missing.

import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { cpSync, existsSync, mkdirSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { startManager, REPO_ROOT } from "./helpers.js";

const EXE = process.platform === "win32" ? ".exe" : "";
const planner = join(REPO_ROOT, "target", "debug", `meduza-np-planner${EXE}`);

async function until<T>(what: string, fn: () => T | undefined | null | false, timeoutMs = 60_000): Promise<T> {
  const start = Date.now();
  for (;;) {
    const v = fn();
    if (v) return v as T;
    if (Date.now() - start > timeoutMs) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 250));
  }
}

test("planner enrolls, syncs over the push channel, applies and reports", { timeout: 120_000 }, async (t) => {
  if (process.env.MEDUZA_NP_SKIP_E2E) {
    t.skip("MEDUZA_NP_SKIP_E2E set (run by the e2e CI job)");
    return;
  }
  if (!existsSync(planner)) {
    assert.ok(!process.env.CI, `planner binary missing in CI: ${planner}`);
    t.skip(`planner binary not built (${planner})`);
    return;
  }
  const { app, s } = await startManager();
  await app.listen({ port: 0, host: "127.0.0.1" });
  const port = (app.server.address() as { port: number }).port;

  s.nodes.createNode({ id: "gw-e2e", behavior: "static" }, "test");
  const { token } = s.nodes.createEnrollmentToken("gw-e2e", "test");

  const root = mkdtempSync(join(tmpdir(), "mnp-e2e-"));
  const pkgs = join(root, "packages");
  mkdirSync(join(pkgs, "meduza-np-planner"), { recursive: true });
  cpSync(join(REPO_ROOT, "packages/meduza-np-planner/manifest.json"), join(pkgs, "meduza-np-planner/manifest.json"));

  const logs: string[] = [];
  const child = spawn(planner, ["run"], {
    env: {
      ...process.env,
      MEDUZA_NP_ETC_DIR: join(root, "etc"),
      MEDUZA_NP_STATE_DIR: join(root, "state"),
      MEDUZA_NP_RUN_DIR: join(root, "run"),
      MEDUZA_NP_PACKAGES_DIR: pkgs,
      MEDUZA_NP_LIBEXEC_DIR: join(root, "libexec"),
      MEDUZA_NP_ENROLL_URL: `http://127.0.0.1:${port}`,
      MEDUZA_NP_ENROLL_TOKEN: token,
      MEDUZA_NP_ENROLL_INSECURE: "1",
      MEDUZA_NP_DRY_RUN_NETWORK: "1",
      MEDUZA_NP_BACKOFF_SPREAD_SECONDS: "0",
      MEDUZA_NP_LOG: "debug",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stderr.on("data", (d) => logs.push(String(d)));
  try {
    await until("enrollment", () => s.nodes.get("gw-e2e")?.public_key);
    await until("push channel", () => s.hub.connected("gw-e2e"));

    const pub = s.config.publish({ schemaVersion: 1, nodes: { "gw-e2e": { networks: { lan: ["10.42.10.0/24"] } } } }, "test");
    const applied = await until("applied ACK", () => s.nodes.syncState("gw-e2e")?.applied_revision === pub.revision);
    assert.ok(applied);
    assert.equal(s.config.publishResults(pub.revision)[0]!.stage, "applied");

    const report = await until("report with packages", () => {
      const r = s.nodes.latestReport("gw-e2e")?.report as { packages?: Array<{ name: string; registration: string }>; sync?: { appliedRevision?: string } } | undefined;
      return r?.sync?.appliedRevision === pub.revision ? r : undefined;
    }, 45_000);
    assert.ok(report.packages?.some((p) => p.name === "meduza-np-planner" && p.registration === "registered"));

    // A second publish for the node arrives through the push notification as well.
    const pub2 = s.config.publish({ schemaVersion: 1, nodes: { "gw-e2e": { networks: { lan: ["10.42.20.0/24"] } } } }, "test");
    await until("second applied ACK", () => s.nodes.syncState("gw-e2e")?.applied_revision === pub2.revision, 30_000);
  } catch (e) {
    console.error(logs.join(""));
    throw e;
  } finally {
    child.kill();
    await app.close();
  }
});
