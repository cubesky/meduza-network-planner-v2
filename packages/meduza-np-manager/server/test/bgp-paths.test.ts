import { test } from "node:test";
import assert from "node:assert/strict";
import { asnNumber, asPathSegments, pathPage, type BgpPath } from "../src/bgp-paths.js";

test("BGP paths: notation preserves ordered hops and unordered barriers including the suffix", () => {
  assert.equal(asnNumber("1.10"), 65546);
  for (const invalid of ["0", "-1", "4294967295", "65536.1", "1.65536", "1.1.1"]) assert.equal(asnNumber(invalid), undefined);
  assert.deepEqual(asPathSegments("65100 {65200,65201} 65300 (65400 1.10) [65500,65501] 65502"), [
    { ordered: true, asns: [65100] }, { ordered: false, asns: [65200, 65201] }, { ordered: true, asns: [65300] },
    { ordered: true, asns: [65400, 65546] }, { ordered: false, asns: [65500, 65501] }, { ordered: true, asns: [65502] },
  ]);
});

test("BGP paths: pagination/search keeps every candidate beyond earlier sample limits", () => {
  const rows: BgpPath[] = Array.from({ length: 123 }, (_, i) => ({ node: "edge", address: "203.0.113.1", neighbor: "uplink", prefix: `10.0.${i}.0/24`,
    family: "ipv4", asPath: `65100 ${65200 + i}`, valid: i % 2 === 0, best: i === 0, nextHops: ["203.0.113.1"] }));
  const all = [0, 1, 2].flatMap((i) => pathPage(rows, "", i).rows);
  assert.deepEqual(all, rows);
  assert.equal(pathPage(rows, "", 999).page, 2);
  assert.deepEqual(pathPage(rows, "10.0.122.0/24", 0).rows, [rows[122]]);
  assert.deepEqual(pathPage(rows, "edge transit", 0, { "65322": "Transit" }).rows, [rows[122]]);
  assert.equal(pathPage(rows, "not-found", 10).page, 0);
  assert.equal(pathPage(rows, "", 0).total, 123);
});
