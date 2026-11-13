import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { childSchema, evalWhen, fieldError, fieldInfo, getAt, itemSchema, setAt, type JsonSchema } from "../src/schema.ts";

const wg = JSON.parse(readFileSync(fileURLToPath(new URL("../../../packages/meduza-np-wireguard/schema/config.json", import.meta.url)), "utf8")) as JsonSchema;

test("classifies fields of a generated plugin schema", () => {
  const instances = childSchema(wg, "instances", wg);
  assert.equal(fieldInfo(instances, wg).kind, "object-list");
  const iface = itemSchema(instances, wg)!;
  const props = iface.properties!;
  assert.equal(fieldInfo(props.listenPort, wg).kind, "integer");
  assert.equal(fieldInfo(props.listenPort, wg).nullable, true);
  assert.equal(fieldInfo(props.enabled, wg).kind, "boolean");
  assert.equal(fieldInfo(props.addresses, wg).kind, "string-list", "IpNet arrays edit as lists");
  assert.equal(fieldInfo(props.id, wg).kind, "string");
  assert.equal(fieldInfo(props.peers, wg).kind, "object-list");
  const mtu = fieldInfo(props.mtu, wg);
  assert.equal(fieldError(mtu, 100), "must be ≥ 1280");
  assert.equal(fieldError(mtu, 1420), undefined);
});

test("classifies the Mihomo TPROXY fields: port lists and nested objects get their own editors", () => {
  const mihomo = JSON.parse(readFileSync(fileURLToPath(new URL("../../../packages/meduza-np-mihomo/schema/config.json", import.meta.url)), "utf8")) as JsonSchema;
  const tproxy = childSchema(mihomo, "tproxy", mihomo);
  assert.equal(fieldInfo(childSchema(tproxy, "excludePorts", mihomo), mihomo).kind, "number-list");
  const sources = fieldInfo(childSchema(tproxy, "sources", mihomo), mihomo);
  assert.equal(sources.kind, "object");
  assert.equal(sources.nullable, false);
  const side = fieldInfo(childSchema(tproxy, "sideRouter", mihomo), mihomo);
  assert.equal(side.kind, "object");
  assert.equal(side.nullable, true, "the side router is an optional block behind a switch");
  assert.deepEqual(Object.keys(side.schema.properties ?? {}), ["gateway", "replyPorts"]);
  assert.match(side.description ?? "", /side router/, "the property's own description, not the type's");
});

test("immutable set removes undefined keys and get follows paths", () => {
  const v = { services: { wireguard: { instances: [] as unknown[] } } };
  const w = setAt(v, ["services", "wireguard", "enabled"], false);
  assert.equal(getAt(w, ["services", "wireguard", "enabled"]), false);
  assert.equal(getAt(v, ["services", "wireguard", "enabled"]), undefined, "original untouched");
  const x = setAt(w, ["services", "wireguard", "enabled"], undefined);
  assert.ok(!("enabled" in (x.services.wireguard as Record<string, unknown>)));
});

test("OpenVPN static-key direction edits zero, one and legacy omission independently of TLS", () => {
  const root = new URL("../../../packages/meduza-np-openvpn/", import.meta.url);
  const schema = JSON.parse(readFileSync(new URL("schema/config.json", root), "utf8")) as JsonSchema;
  const descriptor = JSON.parse(readFileSync(new URL("ui/descriptor.json", root), "utf8"));
  const instance = itemSchema(childSchema(schema, "instances", schema), schema)!;
  const info = fieldInfo(childSchema(instance, "staticKeyDirection", schema), schema);
  assert.equal(info.kind, "integer");
  assert.equal(info.nullable, true);
  for (const value of [undefined, null, 0, 1]) assert.equal(fieldError(info, value), undefined);
  for (const value of [-1, 2, 0.5]) assert.ok(fieldError(info, value));
  let draft = { tls: { keyDirection: 1 } };
  for (const value of [0, 1, undefined]) {
    draft = setAt(draft, ["staticKeyDirection"], value);
    assert.equal(getAt(draft, ["staticKeyDirection"]), value);
    assert.equal(draft.tls.keyDirection, 1);
  }
  assert.equal(Object.hasOwn(draft, "staticKeyDirection"), false);
  assert.ok(descriptor.collections[0].sections.some((s: { fields: string[] }) => s.fields.includes("staticKeyDirection")));
  for (const locale of ["en", "zh-Hans", "zh-Hant"]) {
    assert.ok(descriptor.i18n[locale].fields.staticKeyDirection);
  }
});

test("when expressions", () => {
  assert.equal(evalWhen('kind == "access"', { kind: "access" }), true);
  assert.equal(evalWhen('kind != "access"', { kind: "access" }), false);
  assert.equal(evalWhen("tls.enabled == true", { tls: { enabled: true } }), true);
  assert.equal(evalWhen(undefined, {}), true);
});
