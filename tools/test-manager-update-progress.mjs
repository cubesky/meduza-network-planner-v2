// Browser regression for the full manager node/list/overview pages, including the v0.1.5 completed report.
// Run with Node >=24; CHROME_BIN may override the Chromium/Chrome executable.
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { createServer } from "../packages/meduza-np-manager/node_modules/vite/dist/node/index.js";

const repo = fileURLToPath(new URL("../", import.meta.url));
const browser = process.env.CHROME_BIN ?? (process.platform === "win32" ? "C:/Program Files/Google/Chrome/Application/chrome.exe" : "chromium");
mkdirSync(resolve(repo, "target"), { recursive: true });
const root = mkdtempSync(resolve(repo, "target/manager-update-browser-"));
writeFileSync(resolve(root, "index.html"), '<!doctype html><html><head><meta charset="utf-8"></head><body><main id="app"></main><script type="module" src="/preview.tsx"></script></body></html>');
writeFileSync(resolve(root, "preview.tsx"), `
import { render } from "preact";
import { setLocale } from "/web/ui-kit/src/index.tsx";
import { NodePage } from "/packages/meduza-np-manager/web/src/pages/Node.tsx";
import { NodesPage } from "/packages/meduza-np-manager/web/src/pages/Nodes.tsx";
import { OverviewPage } from "/packages/meduza-np-manager/web/src/pages/Overview.tsx";
import "/packages/meduza-np-manager/web/src/messages.ts";
import "/web/ui-kit/src/styles.css";
setLocale("zh-Hans");
const packages = ["planner", "mihomo-pkg", "mosdns-pkg", "avahi", "cli", "dashboard", "dnsmasq", "frr", "mihomo", "mosdns", "openvpn", "tinc", "wireguard", "updater"].map(p => "meduza-np-" + p);
const completed = { active:false, phase:"done", groupVersion:"v0.1.5", currentPackage:"meduza-np-updater",
  bytesTotal:14514061, downloadedBytes:14514061, completedPackages:14, totalPackages:14, packages,
  pendingPackages:[], error:null, updatedAt:"2026-11-03T06:57:20Z", nextCheckUnix:1793711427 };
const fixtures = {
  done: completed,
  downloading:{...completed,phase:"downloading",active:true,downloadedBytes:7000000,completedPackages:7},
  retrying:{...completed,phase:"retrying",active:true,downloadedBytes:7000000,completedPackages:7,retryError:"TLS EOF",attempt:2,maxAttempts:4,retryAtUnix:1793711427},
  verifying:{...completed,phase:"verifying",active:true},
  failed:{...completed,phase:"failed",error:"registration failed",downloadedBytes:7000000,completedPackages:7},
  missing:null,
  summary:completed,
  metadata:{...completed,installedGroupVersion:"v0.1.5",availableGroupVersion:"v0.1.6",lastCheck:"2026-11-03T09:02:47Z",
    lastResult:{groupVersion:"v0.1.5",outcome:"done",at:"2026-11-03T06:57:20Z"}},
  rolledback:{...completed,phase:"rolled-back",error:"previous version restored",lastResult:{groupVersion:"v0.1.5",outcome:"rolledback"}},
  live:{...completed,phase:"downloading",active:true,downloadedBytes:7000000,completedPackages:7},
};
const reads = {};
const detail = (name) => {
  reads[name] = (reads[name] ?? 0) + 1;
  const update = name === "live" && reads[name] > 1 ? completed : fixtures[name];
  return {id:"gw-"+name,displayName:name,behavior:"static",freshness:"online",arch:"x86_64",tags:[],
    lastSeenAt:"2026-11-03T09:02:47Z",streamConnected:true,desiredRevision:"rev-000008",appliedRevision:"rev-000003",
    failedRevision:"rev-000008",lastError:"process-failed: openvpn/lemon",inSync:false,enabledFeatures:[],problems:[],
    resources:null,maintenance:null,packages:[],jobs:[],update,
    sync:{state:"apply-failed",mode:"push",transport:"websocket"},
    report:{nodeId:"gw-"+name,observedAt:"2026-11-03T09:02:47Z",packages:[],instances:[],features:[],
      update:name === "summary" ? undefined : update}};
};
window.fetch = async (path) => {
  const url = String(path);
  let data;
  if (url === "/api/v1/config") data = {revision:"rev-000008",document:{schemaVersion:1,nodes:{}}};
  else if (url === "/api/v1/drafts/mine") data = null;
  else if (url === "/api/v1/nodes") data = [detail("done"),detail("retrying"),detail("missing")];
  else if (url === "/api/v1/overview") data = {revision:"rev-000008",nodes:[detail("done"),detail("retrying")],counts:{total:2,online:2,offline:0,outOfSync:2},server:{connectedStreams:2,inFlight:0}};
  else if (url.startsWith("/api/v1/nodes/gw-")) data = detail(url.slice("/api/v1/nodes/gw-".length));
  else throw Error("Unexpected request: " + url);
  return new Response(JSON.stringify(data), {headers:{"content-type":"application/json"}});
};
const screenshot = new URL(location.href).searchParams.has("screenshot");
render(<div style={{maxWidth:"1150px",margin:"20px auto"}}>
  {(screenshot ? ["done"] : Object.keys(fixtures)).map(name => <section data-scenario={name}><NodePage id={"gw-"+name} role="viewer" /></section>)}
  <section data-list><NodesPage role="viewer" /></section>
  {!screenshot && <section data-overview><OverviewPage /></section>}
</div>,document.getElementById("app"));
function verify() {
  const check = (condition, message) => { if (!condition) throw Error(message); };
  const card = name => [...document.querySelectorAll('[data-scenario="'+name+'"] section.card')].find(e => e.querySelector("h2")?.textContent === "软件包与升级");
  const done = card("done");
  check(done, "completed node page lost its update card");
  for (const text of ["100%", "14/14", "v0.1.5", "升级完成时间", "已安装包组"]) check(done.textContent.includes(text), "completed node missing " + text);
  check(done.querySelector("progress").value === 14514061, "completed bytes lost");
  check(!done.textContent.includes("下载完成后仍需安装"), "completed upgrade still presented as installing");
  if (!screenshot) {
    check(card("downloading").textContent.includes("48%"), "downloading bytes not rendered");
    for (const text of ["TLS EOF", "2/4", "自动重试时间"]) check(card("retrying").textContent.includes(text), "retry missing " + text);
    check(card("verifying").textContent.includes("下载完成后仍需安装"), "100% download mistaken for activation completion");
    check(!card("verifying").textContent.includes("升级完成时间"), "verification shown as completed");
    check(card("failed").textContent.includes("registration failed"), "failure reason lost");
    check(card("missing").textContent.includes("尚无升级状态") && !card("missing").querySelector("progress"), "missing status hidden or fabricated");
    check(card("summary").textContent.includes("100%"), "top-level update compatibility lost");
    for (const text of ["已安装包组", "可用包组", "v0.1.6", "最近升级检查", "上次升级结果"]) check(card("metadata").textContent.includes(text), "upgrade metadata missing " + text);
    check(card("rolledback").textContent.includes("组件安装已回滚") && !card("rolledback").textContent.includes("升级完成时间"), "rollback mislabeled as success");
    check(card("live").textContent.includes("100%") && reads.live >= 2, "page failed to refresh from downloading to completed");
    for (const selector of ["[data-list]", "[data-overview]"]) {
      const text = document.querySelector(selector).textContent;
      for (const word of ["软件包与升级", "100%", "14/14", "v0.1.5", "等待自动重试下载"]) check(text.includes(word), selector + " missing " + word);
    }
  }
}
setTimeout(() => {
  const proof = document.createElement("output"); proof.id = "browser-proof";
  try { verify(); proof.dataset.result = "pass"; proof.textContent = "manager-update-browser: pass"; }
  catch (error) { proof.dataset.result = "fail"; proof.textContent = String(error); }
  document.body.prepend(proof);
}, screenshot ? 1000 : 11500);
`);
const preactRoot = resolve(repo, "packages/meduza-np-manager/node_modules/preact");
const server = await createServer({
  configFile: false, root, esbuild: { jsx: "automatic", jsxImportSource: "preact" },
  resolve: { alias: [
    { find: /^\/(web|packages)\//, replacement: `${repo.replaceAll("\\", "/")}$1/` },
    { find: "preact/jsx-dev-runtime", replacement: resolve(preactRoot, "jsx-runtime/dist/jsxRuntime.module.js") },
    { find: "preact/jsx-runtime", replacement: resolve(preactRoot, "jsx-runtime/dist/jsxRuntime.module.js") },
    { find: "preact/hooks", replacement: resolve(preactRoot, "hooks/dist/hooks.module.js") },
    { find: "preact", replacement: resolve(preactRoot, "dist/preact.module.js") },
  ] },
  server: { host: "127.0.0.1", port: 0, watch: null, fs: { allow: [repo] } },
});
try {
  await server.listen();
  const url = server.resolvedUrls.local[0];
  for (const screenshot of [false, true]) {
    const args = ["--headless=new", "--disable-gpu", "--disable-background-networking", "--no-first-run", "--no-default-browser-check",
      `--user-data-dir=${resolve(root, screenshot ? "screenshot-profile" : "test-profile")}`, "--window-size=1400,1300",
      `--virtual-time-budget=${screenshot ? 2000 : 13000}`, "--dump-dom"];
    if (screenshot) args.push(`--screenshot=${resolve(repo, "target/manager-update-progress.png")}`);
    const { stdout } = await promisify(execFile)(browser, [...args, url + (screenshot ? "?screenshot" : "")], { maxBuffer: 4 * 1024 * 1024, timeout: 45000 });
    writeFileSync(resolve(root, screenshot ? "screenshot.html" : "result.html"), stdout);
    assert.match(stdout, /id="browser-proof" data-result="pass"/, stdout.match(/<output[^>]*>.*?<\/output>/s)?.[0] ?? stdout.slice(-2000));
  }
  assert.ok(existsSync(resolve(repo, "target/manager-update-progress.png")));
  console.log("PASS: full node/list/overview pages, completed 14/14 report, live polling, retries, verification, failures and absent progress");
  console.log(`Screenshot: ${resolve(repo, "target/manager-update-progress.png")}`);
} finally { await server.close(); }
