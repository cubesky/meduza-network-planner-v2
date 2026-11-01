import { test } from "node:test";
import assert from "node:assert/strict";
import { adminHeaders, startManager } from "./helpers.js";

test("MosDNS and OpenVPN packaged binaries have discoverable, editable resource settings", async () => {
  const { app, s } = await startManager();
  try {
    const headers = await adminHeaders(app);
    const response = await app.inject({ method: "GET", url: "/api/v1/packages", headers });
    assert.equal(response.statusCode, 200);
    for (const name of ["mosdns", "openvpn"]) {
      const pkg = `meduza-np-${name}-pkg`;
      const entry = response.json().find((p: any) => p.manifest.package === pkg);
      assert.equal(entry.manifest.role, "binary-provider");
      assert.ok(!entry.manifest.entrypoint, "resource selection needs no extra daemon or plugin entrypoint");
      assert.equal(entry.ui.settings[0].path, `services.${name}-pkg`);
      assert.equal(entry.ui.status[0].instanceKind, `${name}-provider`);
      assert.ok(entry.manifest.providers.some((p: any) => p.slot === `${name}.binary`));
      const validate = s.catalog.get(pkg)!.validateConfig!;
      assert.ok(validate({enabled:true}));
      assert.ok(validate({enabled:false}));
      assert.ok(!validate({enabled:"yes"}), "invalid resource selection is rejected");
    }
    await app.inject({ method:"POST",url:"/api/v1/nodes",headers,payload:{id:"gw-provider"} });
    const document = {schemaVersion:1,nodes:{"gw-provider":{services:{"mosdns-pkg":{enabled:true},"openvpn-pkg":{enabled:true}}}}};
    const preview = s.config.preview(document);
    assert.deepEqual(preview.problems.filter((p) => p.severity !== "warning"), []);
  } finally { await app.close(); }
});
