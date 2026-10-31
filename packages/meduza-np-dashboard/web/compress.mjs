// Precompress text assets (gzip, level 9) next to the originals; the Rust server picks `.gz` when accepted.
import { readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { gzipSync } from "node:zlib";

const root = new URL("../dist/www/", import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, "$1");
const walk = (d) => readdirSync(d).flatMap((n) => (statSync(join(d, n)).isDirectory() ? walk(join(d, n)) : [join(d, n)]));
let total = 0;
for (const f of walk(root)) {
  if (!/\.(js|css|html|svg|json)$/.test(f)) continue;
  const gz = gzipSync(readFileSync(f), { level: 9 });
  writeFileSync(`${f}.gz`, gz);
  total += gz.length;
}
console.log(`precompressed assets: ${(total / 1024).toFixed(1)} KiB gzip`);
