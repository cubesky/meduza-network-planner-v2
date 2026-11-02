import { test } from "node:test";
import assert from "node:assert/strict";
import { adminHeaders, startManager } from "./helpers.js";

test("all binary providers share resource status fields while retaining their preparation controls", async () => {
  const { app, s } = await startManager();
  try {
    const headers = await adminHeaders(app);
    const response = await app.inject({ method: "GET", url: "/api/v1/packages", headers });
    assert.equal(response.statusCode, 200);
    for (const name of ["mihomo", "mosdns", "openvpn"]) {
      const pkg = `meduza-np-${name}-pkg`;
      const entry = response.json().find((p: any) => p.manifest.package === pkg);
      assert.equal(entry.manifest.role, "binary-provider");
      assert.equal(entry.ui.settings[0].path, `services.${name}-pkg`);
      assert.equal(entry.ui.status[0].instanceKind, `${name}-provider`);
      assert.equal(entry.ui.title, `${name === "mihomo" ? "Mihomo" : name === "mosdns" ? "MosDNS" : "OpenVPN"} 本体资源`);
      assert.deepEqual(entry.ui.status[0].columns, ["id", "state", "packageVersion", "upstreamVersion", "activeSource", "path", "reason"]);
      assert.ok(entry.manifest.providers.some((p: any) => p.slot === `${name}.binary`));
      const validate = s.catalog.get(pkg)!.validateConfig!;
      if (name === "mihomo") {
        assert.ok(entry.manifest.providers.some((p: any) => p.requiresPreparation));
        assert.deepEqual(entry.ui.actions.map((a: any) => a.id), ["check-local", "activate", "download"]);
        assert.ok(validate({mode:"manual"}));
        assert.ok(!validate({mode:"invalid"}));
        assert.ok(entry.ui.status[1].columns.includes("downloadStage"), "download preparation details remain visible");
      } else {
        assert.ok(!entry.manifest.entrypoint, "static resources need no extra daemon or plugin entrypoint");
        assert.ok(validate({enabled:true}));
        assert.ok(validate({enabled:false}));
        assert.ok(!validate({enabled:"yes"}), "invalid resource selection is rejected");
      }
    }
    await app.inject({ method:"POST",url:"/api/v1/nodes",headers,payload:{id:"gw-provider"} });
    const document = {schemaVersion:1,nodes:{"gw-provider":{services:{"mihomo-pkg":{enabled:true,mode:"manual"},"mosdns-pkg":{enabled:true},"openvpn-pkg":{enabled:true}}}}};
    const preview = s.config.preview(document);
    assert.deepEqual(preview.problems.filter((p) => p.severity !== "warning"), []);
  } finally { await app.close(); }
});
