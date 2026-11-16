import { test } from "node:test";
import assert from "node:assert/strict";
import { accessPoolIds, normalizeAccessRouting } from "../src/access-routing.js";
import type { ClusterDocument } from "../src/cluster.js";
import { startManager } from "./helpers.js";
import { generateWireGuardKeyPair } from "../src/keys.js";

test("Access routing migration preserves old requests, removes per-connection switches and honors explicit off", () => {
  for (const source of ["openvpn", "wireguard", "pool-list"]) {
    const doc: ClusterDocument = { schemaVersion: 1, nodes: { gw: {
      services: {
        openvpn: { instances: [{ id: "site", kind: "site2site", bgp: { remoteAsn: 65020, exportAccessPools: source === "openvpn" } }] },
        wireguard: { instances: [{ id: "uplink", peers: [{ id: "peer", bgp: { remoteAsn: 65030, exportAccessPools: source === "wireguard" } }] }] },
      },
      ...(source === "pool-list" ? { routing: { originate: { accessPools: ["vpn"] } } } : {}),
    } } };
    const normalized = normalizeAccessRouting(doc);
    assert.equal(normalized.nodes.gw!.routing?.originate?.access, true, source);
    assert.ok(!JSON.stringify(normalized).includes("exportAccessPools"));
    assert.ok(!JSON.stringify(normalized).includes("accessPools"));
    assert.ok(JSON.stringify(doc).includes("exportAccessPools"), "input/historical documents are not mutated");
    doc.nodes.gw!.routing = { originate: { access: false } };
    assert.equal(normalizeAccessRouting(doc).nodes.gw!.routing?.originate?.access, false, "off wins over stale switches");
  }
  assert.deepEqual(normalizeAccessRouting({ schemaVersion: 1, nodes: { gw: {} } }), { schemaVersion: 1, nodes: { gw: {} } }, "default is off");
});

test("Access pool selection follows enabled servers, not Site2Site routes or routes pushed to clients", () => {
  const spec = { routing: { originate: { access: true } }, services: { openvpn: { instances: [
    { id: "vpn", kind: "access", access: { pool: "10.88.0.0/24", pushRoutes: { extra: ["10.42.0.0/16"] } } },
    { id: "disabled", enabled: false, kind: "access", access: { pool: "10.89.0.0/24" } },
    { id: "site", kind: "site2site", routes: ["10.90.0.0/24"] },
    { id: "empty", kind: "access" },
  ] } } };
  assert.deepEqual(accessPoolIds(spec), ["vpn"]);
  assert.deepEqual(accessPoolIds({ ...spec, routing: { originate: { access: false } } }), []);
  assert.deepEqual(accessPoolIds({ ...spec, services: { openvpn: { ...spec.services.openvpn, enabled: false } } }), []);
});

test("publishing the Network switch originates active Access pools and exports them over every BGP transport", async () => {
  const { app, s } = await startManager();
  try {
    s.nodes.createNode({ id: "gw" }, "test");
    const keys = generateWireGuardKeyPair();
    s.secrets.put({ id: "wg-key", kind: "wireguard-private-key", value: keys.privateKey, publicValue: keys.publicKey, nodeScope: ["gw"] });
    const doc: ClusterDocument = { schemaVersion: 1, routing: { internalAsn: 65010 }, nodes: { gw: {
      routerId: "10.98.0.1",
      routing: { originate: { access: true } },
      services: {
        openvpn: { instances: [
          { id: "vpn", kind: "access", access: { pool: "10.88.0.0/24" } },
          { id: "off", enabled: false, kind: "access", access: { pool: "10.89.0.0/24" } },
          { id: "site", kind: "site2site", ifconfig: { local: "10.98.1.1", remote: "10.98.1.2" }, bgp: { remoteAsn: 65020 } },
        ] },
        wireguard: { instances: [{ id: "uplink", privateKeyRef: "wg-key", addresses: ["10.98.0.1/24"], peers: [
          { id: "peer", publicKey: generateWireGuardKeyPair().publicKey, allowedIps: ["10.98.0.2/32"], bgp: { remoteAsn: 65030 } },
        ] }] },
      },
    } } };
    const snapshot = () => {
      const pub = s.config.publish(doc, "test");
      return s.config.snapshot("gw", pub.revision)!.config as any;
    };
    const on = snapshot();
    assert.deepEqual(on.routing.originate.accessPools, ["vpn"]);
    assert.equal(on.routing.originate.access, undefined, "expand to the existing FRR protocol, no node upgrade needed");
    assert.deepEqual(on.services.frr.externalNeighbors.map((n: any) => [n.transport, n.exportPolicy.accessPools]).sort(), [["openvpn/site", true], ["wireguard/uplink", true]]);
    assert.equal(on.services.openvpn.instances[2].bgp, undefined);
    assert.equal(on.services.wireguard.instances[0].peers[0].bgp, undefined);
    (doc.nodes.gw!.services!.openvpn!.instances as any[]).push({ id: "new", kind: "access", access: { pool: "10.91.0.0/24" } });
    assert.deepEqual(snapshot().routing.originate.accessPools, ["new", "vpn"], "new enabled servers follow the switch automatically");
    doc.nodes.gw!.routing!.originate!.access = false;
    const off = snapshot();
    assert.deepEqual(off.routing.originate.accessPools, []);
    assert.ok(off.services.frr.externalNeighbors.every((n: any) => n.exportPolicy.accessPools === false));
    const legacy = structuredClone(doc);
    delete legacy.nodes.gw!.routing!.originate!.access;
    ((legacy.nodes.gw!.services!.openvpn!.instances as any[])[2].bgp as any).exportAccessPools = true;
    const draft = s.config.createDraft("test", legacy);
    assert.equal(s.config.getDraft(draft.id)!.document.nodes.gw!.routing!.originate!.access, true);
    s.config.publish(legacy, "test");
    const revision = s.config.currentDocument().revision!;
    s.db.run("UPDATE cluster_revisions SET document = ? WHERE revision = ?", JSON.stringify(legacy), revision);
    assert.equal(s.config.currentDocument().document.nodes.gw!.routing!.originate!.access, true);
    assert.ok(!JSON.stringify(s.config.currentDocument().document).includes("exportAccessPools"));
    assert.equal(s.db.get<{ document: string }>("SELECT document FROM cluster_revisions WHERE revision = ?", revision)!.document, JSON.stringify(legacy), "reading old revisions does not rewrite history");
    doc.nodes.gw!.routing!.originate!.access = "true";
    assert.ok(s.config.preview(doc).problems.some((p) => p.path === "/nodes/gw/routing/originate/access" && p.message === "must be boolean"));
  } finally {
    await app.close();
  }
});
