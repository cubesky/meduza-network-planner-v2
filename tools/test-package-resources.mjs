// Exercise complete Manager and Dashboard pages in Chrome against the same node measurements.
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { createServer } from "../packages/meduza-np-manager/node_modules/vite/dist/node/index.js";

const repo = fileURLToPath(new URL("../", import.meta.url));
const browser = process.env.CHROME_BIN ?? (process.platform === "win32" ? "C:/Program Files/Google/Chrome/Application/chrome.exe" : "chromium");
mkdirSync(resolve(repo, "target"), { recursive: true });
const root = mkdtempSync(resolve(repo, "target/package-resources-browser-"));
writeFileSync(resolve(root, "index.html"), '<!doctype html><html><head><meta charset="utf-8"></head><body><main id="app"></main><script type="module" src="/preview.tsx"></script></body></html>');
writeFileSync(resolve(root, "preview.tsx"), `
import { render } from "preact";
import { setLocale } from "/web/ui-kit/src/index.tsx";
import { NodePage } from "/packages/meduza-np-manager/web/src/pages/Node.tsx";
import { OverviewPage, PackagesPage } from "/packages/meduza-np-dashboard/web/src/pages.tsx";
import "/packages/meduza-np-manager/web/src/messages.ts";
import "/web/ui-kit/src/styles.css";
setLocale("zh-Hans");
const now = new Date().toISOString();
const proc = (id, rss, pid) => ({id, instance:id, memoryRssBytes:rss, pid, scope:"planner"});
const packages = [
  {package:"meduza-np-openvpn",diskBytes:1048576,dataBytes:4096,memoryRssBytes:50331648,processes:[proc("openvpn/lemon",33554432,10),proc("openvpn/second",16777216,11)]},
  {package:"meduza-np-openvpn-pkg",diskBytes:16777216,dataBytes:0,memoryRssBytes:0,processes:[]},
  {package:"meduza-np-updater",diskBytes:2097152,dataBytes:0,memoryRssBytes:8388608,processes:[{id:"updater",scope:"updater",pid:1,memoryRssBytes:8388608}]},
  {package:"meduza-np-mosdns",diskBytes:4096,dataBytes:1048576,processes:[],reason:"measurement-unavailable"},
  {package:"meduza-np-base-container",processes:[],reason:"container-image-shared"},
].map(p=>({...p,diskObservedAt:now,memoryObservedAt:now}));
const report = {nodeId:"gw",observedAt:now,packages:[],features:[],instances:[],resources:{packages},
 sync:{state:"in-sync",transport:"websocket",mode:"push",behavior:"static",backoff:{failureLevel:0}}};
const detail = (id) => ({id,displayName:id,behavior:"static",freshness:"online",tags:[],lastSeenAt:now,streamConnected:true,
 enabledFeatures:[],problems:[],packages:[],jobs:[],resources:report.resources,
 report:id === "legacy" ? {...report,resources:{}} : report});
window.fetch = async (path) => {
 let data;
 const url = String(path);
 if (url === "/api/v1/config") data = {revision:"rev-000001",document:{schemaVersion:1,nodes:{}}};
 else if (url === "/api/v1/drafts/mine") data = null;
 else if (url.startsWith("/api/v1/nodes/")) data = detail(url.split("/").pop());
 else if (url === "/api/status") data = report;
 else if (url === "/api/packages") data = {packages:[],resources:packages};
 else throw Error("unexpected request " + url);
 return new Response(JSON.stringify(data),{headers:{"content-type":"application/json"}});
};
const mode = new URL(location.href).searchParams.get("mode");
render(<div style={{maxWidth:"1350px",margin:"20px auto"}}>
 <section data-page="manager"><NodePage id="gw" role="viewer" /></section>
 {mode !== "screenshot" && <>
   <section data-page="dashboard"><OverviewPage session={{role:"viewer"}} /></section>
   <section data-page="packages"><PackagesPage /></section>
   <section data-page="legacy"><NodePage id="legacy" role="viewer" /></section>
 </>}
</div>, document.getElementById("app"));
setTimeout(() => {
 const proof=document.createElement("output");proof.id="browser-proof";
 try {
  const check=(v,m)=>{if(!v)throw Error(m)};
  for (const page of mode === "screenshot" ? ["manager"] : ["manager","dashboard","packages"]) {
   const section=document.querySelector('[data-page="'+page+'"]');
   const card=[...section.querySelectorAll("section.card")].find(c=>c.querySelector("h2")?.textContent === "各包资源占用");
   check(card, page + " has no resource card");
   const text=card.textContent;
   for(const expected of ["openvpn/lemon","openvpn/second","openvpn-pkg","48.0 MiB","32.0 MiB","16.0 MiB","updater PID 1","无运行进程","未测得","共享镜像层"]) {
    check(text.includes(expected),page+" missing "+expected);
   }
   check(card.querySelectorAll("details").length === 2, page+" instance groups missing");
   const openvpn=[...card.querySelectorAll("tbody > tr")].find(r=>r.querySelector("strong")?.textContent === "openvpn");
   check(openvpn.querySelector("details").open,page+" instances not visible");
   openvpn.querySelector("summary").click();
   check(!openvpn.querySelector("details").open,page+" instance details do not collapse");
   openvpn.querySelector("summary").click();
  }
  if(mode!=="screenshot")check(document.querySelector('[data-page="legacy"]').textContent.includes("节点尚未提供各包资源统计"),"older node silently shows zeros");
  proof.dataset.result="pass";proof.textContent="package-resources-browser: pass";
 } catch(e) {proof.dataset.result="fail";proof.textContent=String(e)}
 document.body.prepend(proof);
},1500);
`);
const preactRoot = resolve(repo, "packages/meduza-np-manager/node_modules/preact");
const server = await createServer({
  configFile:false,root,esbuild:{jsx:"automatic",jsxImportSource:"preact"},
  resolve:{alias:[
    {find:/^\/(web|packages)\//,replacement:`${repo.replaceAll("\\", "/")}$1/`},
    {find:"preact/jsx-dev-runtime",replacement:resolve(preactRoot,"jsx-runtime/dist/jsxRuntime.module.js")},
    {find:"preact/jsx-runtime",replacement:resolve(preactRoot,"jsx-runtime/dist/jsxRuntime.module.js")},
    {find:"preact/hooks",replacement:resolve(preactRoot,"hooks/dist/hooks.module.js")},
    {find:"preact",replacement:resolve(preactRoot,"dist/preact.module.js")},
  ]},server:{host:"127.0.0.1",port:0,watch:null,fs:{allow:[repo]}},
});
try {
 await server.listen();
 for(const mode of ["desktop","mobile","screenshot"]) {
  const args=["--headless=new","--disable-gpu","--disable-background-networking","--no-first-run","--no-default-browser-check",
    `--user-data-dir=${resolve(root,mode+"-profile")}`,`--window-size=${mode==="mobile"?"420,1100":"1500,1600"}`,"--virtual-time-budget=3500","--dump-dom"];
  if(mode==="screenshot")args.push(`--screenshot=${resolve(repo,"target/package-resources.png")}`);
  const {stdout}=await promisify(execFile)(browser,[...args,server.resolvedUrls.local[0]+"?mode="+mode],{maxBuffer:4*1024*1024,timeout:45000});
  writeFileSync(resolve(root,mode+".html"),stdout);
  assert.ok(/id="browser-proof" data-result="pass"/.test(stdout),stdout.match(/<output[^>]*>.*?<\/output>/s)?.[0]??stdout.slice(-2000));
 }
 console.log("PASS: Manager and Dashboard pages; per-instance RSS, package disk/data, independent updater, zero/unknown/legacy states and collapsible details (desktop/mobile)");
} finally {await server.close()}
