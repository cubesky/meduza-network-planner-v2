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
const completed = { remoteControl:true, active:false, phase:"done", groupVersion:"v0.1.5", currentPackage:"meduza-np-updater",
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
  controlcheck:completed,
  controlapply:completed,
  controlerror:completed,
  controlpending:completed,
  controlunsupported:{...completed,remoteControl:false},
  recovery:completed,
};
const requests = [];
const requestJobs = {controlpending:[{id:"job-pending",kind:"update-apply",status:"delivered",created_at:new Date().toISOString(),expires_at:new Date(Date.now()+600000).toISOString()}]};
const reads = {};
const binary = { phase:"downloading", active:true, targetVersion:"v1.19.33", platform:"linux/amd64", downloadedBytes:1048576, bytesTotal:4194304, attempt:3, updatedAt:"2026-11-03T09:02:47Z" };
const binaryFixtures = {
  binary,
  binaryretry:{...binary,phase:"retrying",active:false,error:"TLS EOF",retryAtUnix:1793711427},
  binaryinterrupted:{...binary,phase:"interrupted",active:false,error:"binary preparation stopped before completion"},
  binarywaiting:null,
  binaryverifying:{...binary,phase:"validating",downloadedBytes:4194304},
  binarydone:{...binary,phase:"done",active:false,downloadedBytes:4194304},
};
for (const name of Object.keys(binaryFixtures)) fixtures[name] = completed;
const detail = (name) => {
  reads[name] = (reads[name] ?? 0) + 1;
  const update = name === "live" && reads[name] > 1 ? completed : fixtures[name];
  return {id:"gw-"+name,displayName:name,behavior:"static",freshness:"online",arch:"x86_64",tags:[],
    lastSeenAt:"2026-11-03T09:02:47Z",streamConnected:true,desiredRevision:"rev-000008",appliedRevision:"rev-000003",
    failedRevision:"rev-000008",lastError:name === "recovery" ? "openvpn/lemon: compression unsupported" : "process-failed: openvpn/lemon",inSync:false,enabledFeatures:[],problems:[],
    resources:null,maintenance:null,packages:[],jobs:requestJobs[name] ?? [],update,
    sync:{state:"apply-failed",mode:"push",transport:"websocket"},
    report:{nodeId:"gw-"+name,observedAt:"2026-11-03T09:02:47Z",packages:[],sync:{recoveryError:name === "recovery" ? "rev-000003: dnsmasq: at least one listen address" : undefined},instances:name in binaryFixtures ? [{id:"mihomo-pkg/provider",kind:"mihomo-provider",state:"pending",mode:"auto-download",providerState:name === "binarydone" ? "registered" : "waiting-binary",downloadProgress:binaryFixtures[name],reason:"binary not found"}] : [],features:[],
      update:name === "summary" ? undefined : update}};
};
window.fetch = async (path, options) => {
  const url = String(path);
  let data;
  if (options?.method === "POST" && url.endsWith("/jobs")) {
    const name = url.split("/")[4].slice(3);
    const body = JSON.parse(options.body);
    requests.push({name,...body});
    if (name === "controlerror") return new Response(JSON.stringify({error:"unavailable",message:"test manager unavailable"}), {status:503,headers:{"content-type":"application/json"}});
    requestJobs[name] = [{id:"job-"+name,kind:body.kind,status:"succeeded",created_at:new Date().toISOString(),expires_at:new Date(Date.now()+600000).toISOString()}];
    if (name === "controlapply") fixtures[name] = {...completed,phase:"self-restart",active:true};
    data = {jobId:"job-"+name};
  }
  else if (url === "/api/v1/config") data = {revision:"rev-000008",document:{schemaVersion:1,nodes:{}}};
  else if (url === "/api/v1/drafts/mine") data = null;
  else if (url === "/api/v1/nodes") data = [detail("done"),detail("retrying"),detail("missing")];
  else if (url === "/api/v1/overview") data = {revision:"rev-000008",nodes:[detail("done"),detail("retrying")],counts:{total:2,online:2,offline:0,outOfSync:2},server:{connectedStreams:2,inFlight:0}};
  else if (url.startsWith("/api/v1/nodes/gw-")) data = detail(url.slice("/api/v1/nodes/gw-".length));
  else throw Error("Unexpected request: " + url);
  return new Response(JSON.stringify(data), {headers:{"content-type":"application/json"}});
};
const binaryScreenshot = new URL(location.href).searchParams.has("binaryscreenshot");
const screenshot = binaryScreenshot || new URL(location.href).searchParams.has("screenshot");
render(<div style={{maxWidth:"1150px",margin:"20px auto"}}>
  {(screenshot ? [binaryScreenshot ? "binary" : "done"] : Object.keys(fixtures)).map(name => <section data-scenario={name}><NodePage id={"gw-"+name} role={name.startsWith("control") || screenshot && !binaryScreenshot ? "operator" : "viewer"} /></section>)}
  {!binaryScreenshot && <section data-list><NodesPage role="viewer" /></section>}
  {!screenshot && <section data-overview><OverviewPage /></section>}
</div>,document.getElementById("app"));
let controlsProof;
if (!screenshot) setTimeout(async () => {
  try {
    for (const [name, label] of [["controlcheck","检查更新"],["controlapply","立即升级"],["controlerror","立即升级"]]) {
      const section = document.querySelector('[data-scenario="'+name+'"]');
      const button = [...section.querySelectorAll("button")].find(b => b.textContent === label);
      if (!button || button.disabled) throw Error("upgrade control unavailable: " + name);
      button.click();
    }
    controlsProof = true;
  } catch (e) { controlsProof = String(e); }
}, 600);
function verify() {
  const check = (condition, message) => { if (!condition) throw Error(message); };
  const card = name => [...document.querySelectorAll('[data-scenario="'+name+'"] section.card')].find(e => e.querySelector("h2")?.textContent === "软件包与升级");
  if (binaryScreenshot) {
    check(document.querySelector('[data-scenario="binary"]').textContent.includes("Mihomo 本体下载"), "binary screenshot lost its card");
    check(document.querySelector('[aria-label="本体下载进度"]').value === 1048576, "binary screenshot lost real bytes");
    return;
  }
  const done = card("done");
  check(done, "completed node page lost its update card");
  for (const text of ["100%", "14/14", "v0.1.5", "升级完成时间", "已安装包组"]) check(done.textContent.includes(text), "completed node missing " + text);
  check(done.querySelector("progress").value === 14514061, "completed bytes lost");
  check(!done.textContent.includes("下载完成后仍需安装"), "completed upgrade still presented as installing");
  if (!screenshot) {
    check(controlsProof === true, "controls did not run: " + controlsProof);
    check(requests.some(r => r.name === "controlcheck" && r.kind === "update-check"), "check button did not queue update-check");
    check(requests.some(r => r.name === "controlapply" && r.kind === "update-apply"), "upgrade button did not queue update-apply");
    check(requests.length === 3 && requests.every(r => Object.keys(r).length === 2), "upgrade sent arbitrary parameters or repeated requests");
    check(card("controlcheck").textContent.includes("updater 已接受请求"), "accepted check request is hidden");
    check(card("controlapply").textContent.includes("updater 已接受请求") && card("controlapply").textContent.includes("重启更新器"), "updater takeover or request acknowledgement is hidden");
    check(card("controlapply").textContent.includes("接受请求表示已安排执行") && !card("controlapply").textContent.includes("升级完成时间"), "accepted request falsely reported as completed upgrade");
    check([...card("controlapply").querySelectorAll("button")].every(b => b.disabled), "active upgrade still allows repeat requests");
    check(card("controlerror").querySelector('[role="alert"]').textContent.includes("test manager unavailable"), "request failure was hidden");
    check([...card("controlpending").querySelectorAll("button")].every(b => b.disabled) && card("controlpending").textContent.includes("已下发，等待 updater 确认"), "pending command allows duplicate upgrade");
    check([...card("controlunsupported").querySelectorAll("button")].every(b => b.disabled) && card("controlunsupported").textContent.includes("节点尚未提供远程升级控制"), "previous planner support is misrepresented");
    check(!card("done").querySelector("button"), "viewer has upgrade controls");
    const recovery = document.querySelector('[data-scenario="recovery"]');
    check(recovery.querySelector(".callout.bad").textContent.includes("openvpn/lemon: compression unsupported"), "primary apply failure was hidden");
    check(recovery.querySelector(".callout.warn").textContent.includes("恢复历史成功配置时降级") && recovery.querySelector(".callout.warn").textContent.includes("rev-000003: dnsmasq"), "historical recovery failure is not labelled separately");
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
    const binaryCard = name => [...document.querySelectorAll('[data-scenario="'+name+'"] section.card')].find(e => e.querySelector("h2")?.textContent === "Mihomo 本体下载");
    check(binaryCard("binary").textContent.includes("25%") && binaryCard("binary").querySelector("progress").value === 1048576, "real binary bytes are not rendered");
    for (const text of ["TLS EOF", "第 3 次下载尝试", "自动重试时间"]) check(binaryCard("binaryretry").textContent.includes(text), "binary retry missing " + text);
    check(binaryCard("binaryinterrupted").textContent.includes("本体准备已中断"), "stopped preparation still looks active");
    check(binaryCard("binarywaiting").textContent.includes("等待本体准备") && !binaryCard("binarywaiting").querySelector("progress"), "waiting binary fabricated progress");
    check(binaryCard("binarywaiting").textContent.includes("配置应用失败影响了本体准备"), "configuration blocker is hidden");
    check(binaryCard("binaryverifying").textContent.includes("100%") && !binaryCard("binaryverifying").textContent.includes("本体已就绪"), "100% binary download is falsely considered ready");
    check(binaryCard("binarydone").textContent.includes("本体已就绪"), "activated binary readiness missing");
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
  for (const mode of ["test", "screenshot", "binaryscreenshot"]) {
    const screenshot = mode !== "test";
    const args = ["--headless=new", "--disable-gpu", "--disable-background-networking", "--no-first-run", "--no-default-browser-check",
      `--user-data-dir=${resolve(root, `${mode}-profile`)}`, "--window-size=1400,1300",
      `--virtual-time-budget=${screenshot ? 2000 : 13000}`, "--dump-dom"];
    if (screenshot) args.push(`--screenshot=${resolve(repo, mode === "binaryscreenshot" ? "target/manager-binary-progress.png" : "target/manager-update-progress.png")}`);
    const { stdout } = await promisify(execFile)(browser, [...args, url + (screenshot ? `?${mode}` : "")], { maxBuffer: 4 * 1024 * 1024, timeout: 45000 });
    writeFileSync(resolve(root, `${mode}.html`), stdout);
    assert.match(stdout, /id="browser-proof" data-result="pass"/, stdout.match(/<output[^>]*>.*?<\/output>/s)?.[0] ?? stdout.slice(-2000));
  }
  assert.ok(existsSync(resolve(repo, "target/manager-update-progress.png")));
  console.log("PASS: full node/list/overview pages; package updates and independent binary bytes, retries, interruptions and verification");
  console.log(`Screenshot: ${resolve(repo, "target/manager-update-progress.png")}`);
} finally { await server.close(); }
