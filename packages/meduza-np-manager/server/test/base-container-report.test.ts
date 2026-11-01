import { test } from "node:test";
import assert from "node:assert/strict";
import { adminHeaders, startManager } from "./helpers.js";

test("diagnostics package view accepts base runtime reports with independent or unknown image versions", async () => {
  const {app,s}=await startManager();
  try {
    const headers=await adminHeaders(app);
    await app.inject({method:"POST",url:"/api/v1/nodes",headers,payload:{id:"gw-image"}});
    const now=new Date().toISOString();
    const report:any={
      reportVersion:1,nodeId:"gw-image",bootId:"boot-image",sequence:1,observedAt:now,
      sync:{mode:"push",transport:"websocket",behavior:"static",state:"in-sync",backoff:{failureLevel:0,windowSeconds:0,probing:false}},
      system:{container:true,arch:"x86_64",os:"linux",openwrt:false},features:[],instances:[],
      packages:[
        {name:"meduza-np-base-container",version:"0.3.0",role:"container-base",manifestDigest:"",registration:"registered"},
        {name:"meduza-np-updater",version:"0.2.0",role:"updater",manifestDigest:"",registration:"registered"},
      ],
    };
    assert.deepEqual(s.catalog.validate("node-report",report),[],"independent image versions satisfy the actual sync report schema");
    s.db.run("INSERT INTO node_reports (node_id, boot_id, sequence, observed_at, received_at, report) VALUES (?, 'boot-image', 1, ?, ?, ?)","gw-image",now,now,JSON.stringify(report));
    const read=async()=>{
      const r=await app.inject({method:"GET",url:"/api/v1/nodes/gw-image",headers});
      assert.equal(r.statusCode,200,r.body);
      return r.json().packages.find((p:any)=>p.package==="meduza-np-base-container");
    };
    let image=await read();
    assert.equal(image.installed,true);
    assert.equal(image.installedVersion,"0.3.0");
    report.packages[0].version="";
    report.packages[0].reason="The image does not record its own version.";
    assert.deepEqual(s.catalog.validate("node-report",report),[],"unknown legacy image versions also satisfy the sync report schema");
    s.db.run("UPDATE node_reports SET report = ? WHERE node_id = ?",JSON.stringify(report),"gw-image");
    image=await read();
    assert.equal(image.installed,true);
    assert.equal(image.installedVersion,"", "unknown version stays unknown instead of adopting the updater/catalog version");
    report.packages.shift();
    s.db.run("UPDATE node_reports SET report = ? WHERE node_id = ?",JSON.stringify(report),"gw-image");
    assert.equal((await read()).installed,false,"being in an arbitrary container is not evidence of using this base image");
  } finally {await app.close();}
});
