// Complete-page check: external connection errors belong to their rows while the revision remains applied.
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { createServer } from "../packages/meduza-np-manager/node_modules/vite/dist/node/index.js";
const repo = fileURLToPath(new URL("../", import.meta.url));
const browser = process.env.CHROME_BIN ?? (process.platform === "win32" ? "C:/Program Files/Google/Chrome/Application/chrome.exe" : "chromium");
mkdirSync(resolve(repo,"target"),{recursive:true});
const root=mkdtempSync(resolve(repo,"target/connection-errors-browser-"));
writeFileSync(resolve(root,"index.html"),'<!doctype html><html><head><meta charset="utf-8"></head><body><main id="app"></main><script type="module" src="/preview.tsx"></script></body></html>');
writeFileSync(resolve(root,"preview.tsx"),`
import { render } from "preact";
import { setLocale } from "/web/ui-kit/src/index.tsx";
import { NodePage } from "/packages/meduza-np-manager/web/src/pages/Node.tsx";
import { InstancesPage } from "/packages/meduza-np-dashboard/web/src/pages.tsx";
import "/packages/meduza-np-manager/web/src/messages.ts";
import "/web/ui-kit/src/styles.css";
setLocale("zh-Hans");
const now=new Date().toISOString();
const instances=[
 {id:"wireguard/purrfectpaws/broken",kind:"wireguard-peer",state:"degraded",configuredEndpoint:"external.invalid:51820",reasonCode:"endpoint-resolution-failed",reason:"DNS_FAILURE_ONLY_AT_PEER: Name does not resolve",endpointResolution:{attempt:3,retryAtUnix:1234}},
 {id:"wireguard/purrfectpaws/healthy",kind:"wireguard-peer",state:"up",reasonCode:"connected",endpoint:"192.0.2.7:51820"},
 {id:"openvpn/lemon",kind:"openvpn-site2site",state:"starting",reason:"OPENVPN_RESOLVE_ONLY_AT_INSTANCE: session RESOLVE"},
];
const sync={state:"in-sync",mode:"push",transport:"websocket",behavior:"static",desiredRevision:"rev-000002",appliedRevision:"rev-000002",backoff:{failureLevel:0}};
const report={nodeId:"gw",observedAt:now,packages:[],features:[],instances,sync};
const detail={id:"gw",displayName:"gw",behavior:"static",freshness:"online",tags:[],lastSeenAt:now,streamConnected:true,
 desiredRevision:"rev-000002",appliedRevision:"rev-000002",inSync:true,enabledFeatures:[],problems:[],packages:[],jobs:[],sync,report};
window.fetch=async path=>{
 let data;const url=String(path);
 if(url==="/api/v1/config")data={revision:"rev-000002",document:{schemaVersion:1,nodes:{}}};
 else if(url==="/api/v1/drafts/mine")data=null;
 else if(url==="/api/v1/nodes/gw")data=detail;
 else if(url==="/api/status")data=report;
 else if(url==="/api/instances")data={processes:[],applied:{}};
 else throw Error("unexpected request "+url);
 return new Response(JSON.stringify(data),{headers:{"content-type":"application/json"}});
};
render(<div style={{maxWidth:"1250px",margin:"20px auto"}}>
 <section data-page="manager"><NodePage id="gw" tab="connections" role="viewer" /></section>
 <section data-page="dashboard"><InstancesPage session={{role:"viewer"}} /></section>
</div>,document.getElementById("app"));
setTimeout(()=>{
 const proof=document.createElement("output");proof.id="browser-proof";
 try {
  const check=(v,m)=>{if(!v)throw Error(m)};
  for(const name of ["manager","dashboard"]){
   const page=document.querySelector('[data-page="'+name+'"]');
   check(!page.querySelector(".callout.bad"),name+" rendered a global apply failure");
   for(const marker of ["DNS_FAILURE_ONLY_AT_PEER","OPENVPN_RESOLVE_ONLY_AT_INSTANCE"]){
    const rows=[...page.querySelectorAll("tbody > tr")].filter(r=>r.textContent.includes(marker));
    check(rows.length===1,name+" failed to show the external error on one row: "+marker);
    check(page.textContent.split(marker).length===2,name+" duplicated the connection error globally");
   }
   const healthy=[...page.querySelectorAll("tbody > tr")].find(r=>r.textContent.includes("healthy"));
   check(healthy && !healthy.textContent.includes("FAILURE"),name+" damaged an unrelated peer");
  }
  const manager=document.querySelector('[data-page="manager"]');
  check(manager.textContent.includes("external.invalid:51820"),"configured endpoint disappeared when DNS failed");
  check(manager.textContent.includes("端点 DNS 解析失败，自动重试"),"automatic retry status missing from peer row");
  proof.dataset.result="pass";proof.textContent="connection-errors-browser: pass";
 }catch(e){proof.dataset.result="fail";proof.textContent=String(e)}
 document.body.prepend(proof);
},1600);
`);
const preactRoot=resolve(repo,"packages/meduza-np-manager/node_modules/preact");
const server=await createServer({configFile:false,root,esbuild:{jsx:"automatic",jsxImportSource:"preact"},resolve:{alias:[
 {find:/^\/(web|packages)\//,replacement:`${repo.replaceAll("\\","/")}$1/`},
 {find:"preact/jsx-dev-runtime",replacement:resolve(preactRoot,"jsx-runtime/dist/jsxRuntime.module.js")},
 {find:"preact/jsx-runtime",replacement:resolve(preactRoot,"jsx-runtime/dist/jsxRuntime.module.js")},
 {find:"preact/hooks",replacement:resolve(preactRoot,"hooks/dist/hooks.module.js")},
 {find:"preact",replacement:resolve(preactRoot,"dist/preact.module.js")},
]},server:{host:"127.0.0.1",port:0,watch:null,fs:{allow:[repo]}}});
try{
 await server.listen();
 const {stdout}=await promisify(execFile)(browser,["--headless=new","--disable-gpu","--disable-background-networking","--no-first-run","--no-default-browser-check",
 `--user-data-dir=${resolve(root,"profile")}`,"--window-size=1400,1450","--virtual-time-budget=3500","--dump-dom",
 `--screenshot=${resolve(repo,"target/connection-errors.png")}`,server.resolvedUrls.local[0]],{maxBuffer:4*1024*1024,timeout:45000});
 writeFileSync(resolve(root,"result.html"),stdout);
 assert.ok(/id="browser-proof" data-result="pass"/.test(stdout),stdout.match(/<output[^>]*>.*?<\/output>/s)?.[0]??stdout.slice(-2000));
 console.log("PASS: Manager and Dashboard show WireGuard/OpenVPN external failures only on their rows; unrelated peer and applied revision remain healthy");
}finally{await server.close()}
