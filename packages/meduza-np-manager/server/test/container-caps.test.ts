// D43: FRR needs CAP_SYS_ADMIN in a planner container; a node whose report lacks it (FRR installed by the updater
// after the container was created without the frr component) gets a pre-publish warning. FRR without a router id
// is refused before publishing instead of by the node.

import { test } from "node:test";
import assert from "node:assert/strict";
import { adminHeaders, startManager } from "./helpers.js";

type P = { node?: string; path: string; message: string; severity?: string };

test("FRR on a container without CAP_SYS_ADMIN is warned about, a missing routerId is an error", async () => {
  const { app, s } = await startManager();
  const headers = await adminHeaders(app);
  for (const id of ["no-sys", "with-sys", "native"]) {
    assert.equal((await app.inject({ method: "POST", url: "/api/v1/nodes", headers, payload: { id } })).statusCode, 200);
  }
  const report = (capabilities?: string[]) =>
    JSON.stringify({
      packages: [
        { name: "meduza-np-planner", registration: "registered", provides: ["planner.core"] },
        { name: "meduza-np-frr", registration: "registered", provides: ["frr.bgp", "frr.bfd", "frr.anycast-ospf"] },
      ],
      system: { arch: "x86_64", os: "linux", container: capabilities !== undefined, ...(capabilities ? { capabilities } : {}) },
    });
  const now = new Date().toISOString();
  const base = ["NET_BIND_SERVICE", "NET_ADMIN", "NET_RAW"];
  for (const [id, caps] of [["no-sys", base], ["with-sys", [...base, "SYS_ADMIN"]], ["native", undefined]] as const) {
    s.db.run(
      "INSERT INTO node_reports (node_id, boot_id, sequence, observed_at, received_at, report) VALUES (?, 'boot-1', 1, ?, ?, ?)",
      id, now, now, report(caps as string[] | undefined),
    );
  }
  const frrNode = (routerId?: string) => ({ ...(routerId ? { routerId } : {}), networks: { lan: ["10.65.0.0/24"] }, services: { frr: { localAsn: 65020 } } });
  const validate = async (doc: unknown) => {
    const res = await app.inject({ method: "POST", url: "/api/v1/validate", headers, payload: { document: doc } });
    assert.equal(res.statusCode, 200, res.body);
    return res.json().problems as P[];
  };

  let problems = await validate({ schemaVersion: 1, nodes: { "no-sys": frrNode("10.65.0.1"), "with-sys": frrNode("10.65.0.2"), native: frrNode("10.65.0.3") } });
  const caps = problems.filter((p) => p.message.includes("CAP_SYS_ADMIN"));
  assert.equal(caps.length, 1, JSON.stringify(problems));
  assert.equal(caps[0]!.node, "no-sys");
  assert.equal(caps[0]!.path, "/nodes/no-sys/services/frr");
  assert.equal(caps[0]!.severity, "warning");
  assert.ok(!problems.some((p) => p.path.endsWith("/routerId")), JSON.stringify(problems));

  problems = await validate({ schemaVersion: 1, nodes: { "with-sys": frrNode() } });
  const rid = problems.filter((p) => p.path === "/nodes/with-sys/routerId");
  assert.equal(rid.length, 1, JSON.stringify(problems));
  assert.notEqual(rid[0]!.severity, "warning");

  problems = await validate({ schemaVersion: 1, nodes: { "with-sys": frrNode("not-an-ip") } });
  assert.ok(problems.some((p) => p.path === "/nodes/with-sys/routerId" && p.message.includes("IPv4")), JSON.stringify(problems));
  await app.close();
});
