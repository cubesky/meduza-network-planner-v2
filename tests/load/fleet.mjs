#!/usr/bin/env node
// Fleet load test for the manager (AGENT.md §7.5, §14.4.4): starts a real manager process on a fresh data
// directory, enrolls N simulated planners and drives them the way the real sync client does (one WebSocket per
// static node, HTTPS config pull + ACKs after every notification, periodic reports, ETag polling for roaming
// nodes, Retry-After and jittered exponential backoff as in crates/runtime/src/backoff.rs). It measures
// enrollment and connection time, publish fan-out latency, request latency, overload answers, manager RSS / CPU
// and database size, optionally across a manager restart. Results go to stdout and to --out as JSON.
//
//   pnpm --filter @meduza-np/manager build && node tests/load/fleet.mjs --nodes 500 --restart
//
// See tests/load/README.md for the options and how to record sizing results.

import { spawn, execFile } from "node:child_process";
import { createRequire } from "node:module";
import { mkdtempSync, writeFileSync, readFileSync, readdirSync, statSync, rmSync, openSync, closeSync, existsSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir, cpus, totalmem } from "node:os";
import { join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { randomBytes, randomUUID } from "node:crypto";
import { parseArgs, promisify } from "node:util";

const ROOT = fileURLToPath(new URL("../../", import.meta.url));
const MANAGER = join(ROOT, "packages/meduza-np-manager");
const { WebSocket } = createRequire(join(MANAGER, "package.json"))("ws");
const { SoftAuthenticator, NodeIdentity } = await import(pathToFileURL(join(MANAGER, "dist/server/test/helpers.js")).href);
const { sessionSigningInput } = await import(pathToFileURL(join(MANAGER, "dist/server/src/nodes.js")).href);

const { values: opt } = parseArgs({
  options: {
    nodes: { type: "string", default: "200" },
    ramp: { type: "string", default: "30" },
    duration: { type: "string", default: "60" },
    "report-interval": { type: "string", default: "30" },
    roaming: { type: "string", default: "0.1" },
    "poll-interval": { type: "string", default: "60" },
    "startup-spread": { type: "string", default: "30" },
    restart: { type: "boolean", default: false },
    env: { type: "string", multiple: true, default: [] },
    out: { type: "string" },
    keep: { type: "boolean", default: false },
    container: { type: "string" },
    memory: { type: "string" },
    "container-cpus": { type: "string" },
  },
});
if ((opt.memory || opt["container-cpus"]) && !opt.container) throw new Error("--memory / --container-cpus need --container IMAGE");
if (opt.container && process.platform !== "linux") throw new Error("--container needs Linux (Podman, cgroup v2)");

const N = int(opt.nodes, "nodes");
const RAMP_S = num(opt.ramp, "ramp");
const DURATION_S = num(opt.duration, "duration");
const REPORT_S = num(opt["report-interval"], "report-interval");
const POLL_S = num(opt["poll-interval"], "poll-interval");
const SPREAD_S = num(opt["startup-spread"], "startup-spread");
const ROAMING = Math.max(0, Math.min(1, Number(opt.roaming)));
const BACKOFF_BASE_MS = 2000;
const BACKOFF_CAP_MS = 300_000;
const BOOTSTRAP = randomBytes(24).toString("base64url");

function int(v, name) {
  const n = Number(v);
  if (!Number.isInteger(n) || n <= 0) throw new Error(`--${name} must be a positive integer`);
  return n;
}
function num(v, name) {
  const n = Number(v);
  if (!Number.isFinite(n) || n < 0) throw new Error(`--${name} must be a non-negative number`);
  return n;
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const rand = (ms) => Math.random() * ms;
const log = (msg) => console.log(`[${((Date.now() - T0) / 1000).toFixed(1).padStart(6)}s] ${msg}`);
const T0 = Date.now();

// ---- statistics ---------------------------------------------------------------------------------------------

let phase = "setup";
const stats = { status: {}, overloaded: { 429: 0, 503: 0 }, wsRejected: 0, wsReconnects: 0, errors: 0, latency: {} };
const samples = [];

function countStatus(code) {
  stats.status[code] = (stats.status[code] ?? 0) + 1;
  if (code === 429 || code === 503) stats.overloaded[code]++;
}
function recordLatency(route, ms) {
  (stats.latency[route] ??= []).push(ms);
}
function pct(list, p) {
  if (!list.length) return null;
  const s = [...list].sort((a, b) => a - b);
  return Math.round(s[Math.min(s.length - 1, Math.floor((p / 100) * s.length))] * 10) / 10;
}
function summary(list) {
  return { n: list.length, p50: pct(list, 50), p95: pct(list, 95), p99: pct(list, 99), max: pct(list, 100) };
}

// ---- manager process ----------------------------------------------------------------------------------------

const work = mkdtempSync(join(tmpdir(), "mnp-load-"));
const dataDir = join(work, "data");
writeFileSync(join(work, "session.key"), randomBytes(32).toString("hex"));
writeFileSync(join(work, "secret.key"), randomBytes(32).toString("hex"));
writeFileSync(join(work, "bootstrap"), BOOTSTRAP);
const port = await freePort();
const ORIGIN = `http://localhost:${port}`;
const BASE = `http://127.0.0.1:${port}`;
let manager = null;

function freePort() {
  return new Promise((resolve, reject) => {
    const srv = createServer().listen(0, "127.0.0.1", () => {
      const p = srv.address().port;
      srv.close(() => resolve(p));
    });
    srv.on("error", reject);
  });
}

async function startManager() {
  if (opt.container) return startContainer();
  const env = {
    ...process.env,
    NODE_ENV: "production",
    LOG_LEVEL: "warn",
    PUBLIC_ORIGIN: ORIGIN,
    RP_ID: "localhost",
    ALLOW_INSECURE_ORIGIN: "1",
    LISTEN_HOST: "127.0.0.1",
    PORT: String(port),
    DATA_DIR: dataDir,
    SESSION_SECRET_FILE: join(work, "session.key"),
    SECRET_ENCRYPTION_KEY_FILE: join(work, "secret.key"),
    BOOTSTRAP_TOKEN_FILE: join(work, "bootstrap"),
    PACKAGES_DIR: join(ROOT, "packages"),
    SCHEMAS_DIR: join(ROOT, "schemas"),
    WEB_DIR: join(MANAGER, "dist/web"),
  };
  for (const kv of opt.env) {
    const i = kv.indexOf("=");
    if (i <= 0) throw new Error(`--env expects KEY=VALUE, got ${kv}`);
    env[kv.slice(0, i)] = kv.slice(i + 1);
  }
  const logFd = openSync(join(work, "manager.log"), "a");
  const child = spawn(process.execPath, ["--import", pathToFileURL(join(ROOT, "tests/load/probe.mjs")).href, join(MANAGER, "dist/server/src/main.js")], {
    env,
    stdio: ["ignore", logFd, logFd, "ipc"],
  });
  closeSync(logFd);
  let lastCpu = null;
  child.on("message", (m) => {
    if (!m?.probe) return;
    const cpuPct = lastCpu ? ((m.cpuUs - lastCpu.cpuUs) / 1000 / (m.at - lastCpu.at)) * 100 : 0;
    lastCpu = m;
    samples.push({ phase, at: m.at, rss: m.rss, heapUsed: m.heapUsed, cpuPct });
  });
  const exited = new Promise((r) => child.once("exit", (code, signal) => r({ code, signal })));
  const m = { child, exited };
  manager = m;
  void exited.then((x) => {
    if (manager === m) log(`manager exited unexpectedly: ${JSON.stringify(x)}, see ${join(work, "manager.log")}`);
  });
  return waitReady(exited, join(work, "manager.log"));
}

async function waitReady(exited, where) {
  const started = Date.now();
  for (;;) {
    const exit = await Promise.race([exited, sleep(200).then(() => null)]);
    if (exit) throw new Error(`manager exited during start (${JSON.stringify(exit)}), see ${where}`);
    if (Date.now() - started > 300_000) throw new Error(`manager not ready after 300 s, see ${where}`);
    try {
      const r = await fetch(`${BASE}/readyz`);
      if (r.ok) return Date.now() - started;
    } catch {
      // not listening yet
    }
  }
}

// ---- manager container (--container IMAGE) ------------------------------------------------------------------
// Runs the production image the way deploy/podman/meduza-np-manager.container does (read-only root, tmpfs /tmp,
// no capabilities, no-new-privileges, Podman secrets for uid 10001, named data volume, loopback port), optionally
// with --memory / --cpus. Memory and CPU come from the container's cgroup (v2) and the node process' VmRSS; a
// restart is `podman stop` (SIGTERM, then the run checks exit code and OOM kill) + `podman start`.

const tag = randomBytes(4).toString("hex");
const ctr = { name: `mnp-load-${tag}`, volume: `mnp-load-${tag}-data`, secrets: [`mnp-load-${tag}-session`, `mnp-load-${tag}-secret`, `mnp-load-${tag}-bootstrap`], created: false, stops: [] };
// Always asynchronous: a blocking call (e.g. a 20 s `podman stop`) would freeze every simulated planner with it.
const podman = async (...args) => (await promisify(execFile)("podman", args, { encoding: "utf8" })).stdout.trim();

async function startContainer() {
  if (!ctr.created) {
    await podman("volume", "create", ctr.volume);
    for (const [s, f] of [[ctr.secrets[0], "session.key"], [ctr.secrets[1], "secret.key"], [ctr.secrets[2], "bootstrap"]]) await podman("secret", "create", s, join(work, f));
    ctr.created = true;
    const secret = (s, target) => ["--secret", `${s},type=mount,target=${target},mode=0400,uid=10001,gid=10001`];
    const env = ["NODE_ENV=production", "LOG_LEVEL=warn", `PUBLIC_ORIGIN=${ORIGIN}`, "RP_ID=localhost", "ALLOW_INSECURE_ORIGIN=1", "BOOTSTRAP_TOKEN_FILE=/run/secrets/bootstrap", ...opt.env];
    await podman(
      "run", "-d", "--name", ctr.name, "--read-only", "--tmpfs", "/tmp:rw,size=64m,mode=1777", "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
      "--ulimit", "nofile=65536:65536", "-p", `127.0.0.1:${port}:8787`, "-v", `${ctr.volume}:/var/lib/meduza-np-manager:Z`,
      ...secret(ctr.secrets[0], "/run/secrets/session.key"), ...secret(ctr.secrets[1], "/run/secrets/secret.key"), ...secret(ctr.secrets[2], "/run/secrets/bootstrap"),
      ...env.flatMap((e) => ["-e", e]), ...(opt.memory ? ["--memory", opt.memory, "--memory-swap", opt.memory] : []), ...(opt["container-cpus"] ? ["--cpus", opt["container-cpus"]] : []),
      opt.container,
    );
  } else {
    await podman("start", ctr.name);
  }
  const exited = (async () => {
    for (;;) {
      await sleep(1000);
      if (manager !== m) return null;
      const st = await inspect();
      if (st && !st.Running) return { code: st.ExitCode, oomKilled: st.OOMKilled };
    }
  })();
  const m = { exited, sampler: null };
  manager = m;
  void exited.then((x) => {
    if (x && manager === m) log(`manager container exited unexpectedly: ${JSON.stringify(x)}`);
  });
  const ms = await waitReady(exited, `podman logs ${ctr.name}`);
  m.sampler = startSampler(m, (await inspect())?.Pid);
  return ms;
}

async function inspect() {
  try {
    return JSON.parse(await podman("inspect", "--format", "{{json .State}}", ctr.name));
  } catch {
    return null;
  }
}

/** The container's cgroup: the libpod-<id>.scope ancestor of the node process (crun puts the process one level below). */
function cgroupDir(pid) {
  const rel = readFileSync(`/proc/${pid}/cgroup`, "utf8").split("\n").find((l) => l.startsWith("0::"))?.slice(3) ?? "/";
  const parts = rel.split("/");
  const i = parts.findLastIndex((p) => /^libpod-[0-9a-f]+\.scope$/.test(p));
  return join("/sys/fs/cgroup", ...(i >= 0 ? parts.slice(0, i + 1) : parts));
}

function startSampler(m, pid) {
  const dir = cgroupDir(pid);
  if (!existsSync(join(dir, "memory.current"))) throw new Error(`no cgroup v2 memory controller at ${dir}`);
  ctr.cgroup = dir;
  ctr.memoryMax = readFileSync(join(dir, "memory.max"), "utf8").trim();
  let last = null;
  const t = setInterval(() => {
    try {
      const at = Date.now();
      const cpuUs = Number(/usage_usec (\d+)/.exec(readFileSync(join(dir, "cpu.stat"), "utf8"))[1]);
      const rss = Number(/VmRSS:\s+(\d+) kB/.exec(readFileSync(`/proc/${pid}/status`, "utf8"))[1]) * 1024;
      const anon = Number(/^anon (\d+)/m.exec(readFileSync(join(dir, "memory.stat"), "utf8"))[1]);
      const cgroup = Number(readFileSync(join(dir, "memory.current"), "utf8"));
      ctr.oomKills = Number(/oom_kill (\d+)/.exec(readFileSync(join(dir, "memory.events"), "utf8"))?.[1] ?? 0);
      const cpuPct = last ? ((cpuUs - last.cpuUs) / 1000 / (at - last.at)) * 100 : 0;
      last = { at, cpuUs };
      samples.push({ phase, at, rss, heapUsed: null, cgroup, anon, cpuPct });
    } catch {
      // process or cgroup gone (stopping)
    }
  }, 1000);
  t.unref();
  m.sampler = t;
  return t;
}

async function stopContainer(m) {
  clearInterval(m.sampler);
  const t = Date.now();
  try {
    await podman("stop", "-t", "25", ctr.name);
  } catch (e) {
    log(`podman stop: ${e.message}`);
  }
  const st = await inspect();
  ctr.stops.push({ stopMs: Date.now() - t, exitCode: st?.ExitCode ?? null, oomKilled: st?.OOMKilled ?? null, oomKillsInCgroup: ctr.oomKills ?? 0 });
}

async function removeContainer() {
  if (!ctr.created || opt.keep) return;
  for (const args of [["rm", "-f", ctr.name], ["volume", "rm", "-f", ctr.volume], ...ctr.secrets.map((s) => ["secret", "rm", s])]) {
    try {
      await podman(...args);
    } catch {
      // already gone
    }
  }
}

async function containerDataBytes() {
  const mount = await podman("volume", "inspect", "--format", "{{.Mountpoint}}", ctr.volume);
  try {
    return dirBytes(mount);
  } catch {
    // rootless: the volume belongs to a sub-uid
    return Number((await podman("unshare", "du", "-sb", mount)).split(/\s+/)[0]);
  }
}

async function stopManager() {
  const m = manager;
  if (!m) return;
  if (opt.container) {
    manager = null;
    return stopContainer(m);
  }
  manager = null;
  // SIGTERM drains streams and closes the database (on Windows this is a hard kill; SQLite WAL survives it).
  const t = Date.now();
  m.child.kill("SIGTERM");
  await Promise.race([m.exited, sleep(25_000).then(() => m.child.kill("SIGKILL"))]);
  const x = await m.exited;
  ctr.stops.push({ stopMs: Date.now() - t, exitCode: x.code, signal: x.signal });
}

// ---- operator (admin passkey session) -----------------------------------------------------------------------

const admin = { cookie: "", csrf: "" };

async function adminCall(method, path, body) {
  const r = await fetch(`${BASE}${path}`, {
    method,
    headers: { origin: ORIGIN, "content-type": "application/json", cookie: admin.cookie, ...(admin.csrf ? { "x-csrf-token": admin.csrf } : {}) },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await r.text();
  if (!r.ok) throw new Error(`${method} ${path}: ${r.status} ${text}`);
  const setCookie = r.headers.get("set-cookie");
  if (setCookie) admin.cookie = setCookie.split(";")[0];
  return text ? JSON.parse(text) : null;
}

async function bootstrapAdmin() {
  const authn = new SoftAuthenticator("localhost", ORIGIN);
  const { challengeId, options } = await adminCall("POST", "/api/v1/auth/passkeys/register/options", { bootstrapToken: BOOTSTRAP, username: "load-admin" });
  await adminCall("POST", "/api/v1/auth/passkeys/register/verify", { challengeId, response: authn.register(options), name: "load" });
  admin.csrf = (await adminCall("GET", "/api/v1/auth/session")).csrfToken;
  if (!admin.csrf) throw new Error("bootstrap did not sign in");
}

let generation = 0;
/** Publishes a document; `touch` decides which nodes get a changed LAN prefix this round. */
async function publish(nodes, touch) {
  generation++;
  const doc = { schemaVersion: 1, nodes: {} };
  for (const n of nodes) {
    n.lanGen = touch(n) ? generation : (n.lanGen ?? 0);
    doc.nodes[n.id] = { networks: { lan: [`10.${(n.index >> 8) & 255}.${n.index & 255}.0/24`, `172.16.${n.lanGen % 256}.0/24`] } };
  }
  const draft = await adminCall("POST", "/api/v1/drafts", { document: doc });
  const at = Date.now();
  const out = await adminCall("POST", `/api/v1/drafts/${draft.id}/publish`, { message: `load ${generation}` });
  const changed = new Map(out.nodes.filter((x) => x.changed).map((x) => [x.nodeId, x.nodeRevision]));
  return { at, publishMs: Date.now() - at, changed };
}

// ---- simulated planner --------------------------------------------------------------------------------------

class SimNode {
  constructor(index, roaming) {
    this.index = index;
    this.id = `load-${String(index).padStart(5, "0")}`;
    this.roaming = roaming;
    this.identity = new NodeIdentity();
    this.bootId = randomUUID();
    this.sequence = 0;
    this.token = null;
    this.applied = null;
    this.appliedAt = new Map();
    this.lastEventId = null;
    this.etag = null;
    this.ws = null;
    this.connected = false;
    this.level = 0;
    this.stopped = false;
    this.syncing = null;
    this.timers = new Set();
  }

  later(ms, fn) {
    const t = setTimeout(() => {
      this.timers.delete(t);
      if (!this.stopped) fn();
    }, ms);
    this.timers.add(t);
  }

  backoffMs() {
    // Full jitter on an exponential window (crates/runtime backoff: base 2 s, cap 300 s). Like the shared gate,
    // the failure level rises at most once per window however many requests fail inside it.
    const now = Date.now();
    const windowMs = () => Math.min(BACKOFF_CAP_MS, BACKOFF_BASE_MS * 2 ** Math.max(0, this.level - 1));
    if (now >= (this.windowEnd ?? 0)) {
      this.level = Math.min(this.level + 1, 12);
      this.windowEnd = now + windowMs();
    }
    return BACKOFF_BASE_MS / 2 + rand(windowMs());
  }

  /** Waits out a failure window; like the shared gate, it ends early once the push channel is healthy again. */
  backoffSleep() {
    const ms = this.backoffMs();
    return new Promise((resolve) => {
      const t = setTimeout(done, ms);
      function done() {
        clearTimeout(t);
        resolve();
      }
      (this.waiters ??= []).push(done);
    });
  }

  healthy() {
    this.level = 0;
    this.windowEnd = 0;
    for (const w of this.waiters?.splice(0) ?? []) w();
  }

  /** Short control request with the planner's retry rules: Retry-After for 429/503, re-auth on 401. */
  async request(method, path, body, extraHeaders = {}) {
    for (let attempt = 0; !this.stopped; attempt++) {
      if (!this.token && !path.startsWith("/api/v1/nodes/self/session") && path !== "/api/v1/nodes/enroll") await this.openSession();
      const started = performance.now();
      let r, text;
      try {
        r = await fetch(`${BASE}${path}`, {
          method,
          headers: { "content-type": "application/json", ...(this.token ? { authorization: `Bearer ${this.token}` } : {}), ...extraHeaders },
          body: body === undefined ? undefined : JSON.stringify(body),
          signal: AbortSignal.timeout(20_000), // planner request timeout (transport.rs)
        });
        text = await r.text();
      } catch {
        stats.errors++;
        await this.backoffSleep();
        continue;
      }
      countStatus(r.status);
      if (r.status === 429 || r.status === 503) {
        const ra = Number(r.headers.get("retry-after") ?? "5");
        await sleep(Math.min(ra, 600) * 1000 + rand(1000));
        continue;
      }
      if (r.status === 401 && this.token && attempt < 3) {
        this.token = null;
        continue;
      }
      if (r.status >= 500) {
        await this.backoffSleep();
        continue;
      }
      this.healthy();
      recordLatency(`${method} ${path.replace(/\?.*$/, "")}`, performance.now() - started);
      if (r.status >= 400) throw new Error(`${this.id} ${method} ${path}: ${r.status} ${text}`);
      return { status: r.status, headers: r.headers, body: text ? JSON.parse(text) : null };
    }
    return null;
  }

  async enroll(enrollmentToken) {
    await this.request("POST", "/api/v1/nodes/enroll", { enrollmentToken, publicKey: this.identity.publicKeyB64(), hostname: this.id, arch: "x86_64", plannerApi: 1, plannerVersion: "load" });
  }

  async openSession() {
    const ch = await this.request("POST", "/api/v1/nodes/self/session/challenge", { nodeId: this.id });
    if (!ch) return;
    const s = await this.request("POST", "/api/v1/nodes/self/session", { nodeId: this.id, nonce: ch.body.nonce, signature: this.identity.sign(sessionSigningInput(this.id, ch.body.nonce)) });
    if (s) this.token = s.body.token;
  }

  start(delayMs) {
    this.later(delayMs, async () => {
      await this.openSession();
      if (this.roaming) this.pollLoop();
      else this.connect();
      this.later(rand(REPORT_S * 1000), () => this.reportLoop());
    });
  }

  stop() {
    this.stopped = true;
    for (const t of this.timers) clearTimeout(t);
    this.timers.clear();
    this.ws?.terminate();
    this.healthy();
  }

  connect() {
    if (this.stopped) return;
    const ws = new WebSocket(`${BASE.replace("http", "ws")}/api/v1/nodes/self/stream`, {
      headers: { authorization: `Bearer ${this.token}` },
      // Like the planner (transport.rs: connect timeout 10 s): a connection accepted by a socket that never answers
      // (e.g. the port conmon holds while the manager container is stopped) must not hang forever.
      handshakeTimeout: 10_000,
    });
    this.ws = ws;
    this.note = "ws-connecting";
    let opened = false;
    ws.on("unexpected-response", (_req, res) => {
      countStatus(res.statusCode);
      stats.wsRejected++;
      this.lastFail = `http ${res.statusCode}`;
      const ra = Number(res.headers["retry-after"] ?? "0");
      res.resume();
      ws.terminate();
      if (res.statusCode === 401) this.token = null;
      this.reconnect(ra > 0 ? Math.min(ra, 600) * 1000 + rand(1000) : this.backoffMs(), res.statusCode === 401);
    });
    ws.on("open", () => {
      opened = true;
      this.note = "ws-hello";
      ws.send(JSON.stringify({ helloVersion: 1, ...(this.lastEventId ? { lastEventId: this.lastEventId } : {}), ...(this.applied ? { lastSuccessfulRevision: this.applied } : {}) }));
    });
    ws.on("message", (data) => {
      const m = JSON.parse(String(data));
      if (m.eventVersion === undefined) {
        // welcome
        this.connected = true;
        this.healthy();
        if (m.desiredRevision && m.desiredRevision !== this.applied) void this.sync();
        return;
      }
      if (m.eventId && /^\d+$/.test(m.eventId)) this.lastEventId = m.eventId;
      if (m.type === "config.available" && m.revision !== this.applied) void this.sync();
      if (m.type === "server.cooldown") this.cooldownUntil = Date.now() + (m.retryAfterMs ?? 30_000);
    });
    ws.on("error", (e) => (this.lastFail = e.message));
    ws.on("close", (code) => {
      if (this.ws !== ws) return;
      this.lastFail = `${this.lastFail ?? ""} close ${code}`.trim();
      const was = this.connected;
      this.connected = false;
      if (this.stopped) return;
      if (!opened || !was) {
        // refused / rejected handshake (a rejection already scheduled its Retry-After reconnect)
        this.reconnect(this.backoffMs());
        return;
      }
      stats.wsReconnects++;
      // A full channel loss (manager restart) adds the startup stagger so the fleet does not return in lockstep.
      this.reconnect(this.backoffMs() + (code === 1001 || code === 1006 ? rand(SPREAD_S * 1000) : 0));
    });
  }

  reconnect(ms, reauth = false) {
    if (this.reconnecting) return;
    this.reconnecting = true;
    this.note = `reconnect in ${Math.round(ms)} ms`;
    this.later(ms, async () => {
      this.reconnecting = false;
      try {
        if (reauth || !this.token) {
          this.note = "session";
          await this.openSession();
        }
      } catch (e) {
        log(`${this.id}: session failed: ${e.message}`);
        this.reconnect(this.backoffMs(), true);
        return;
      }
      this.connect();
    });
  }

  async sync() {
    if (this.syncing) {
      this.resync = true;
      return this.syncing;
    }
    this.syncing = (async () => {
      do {
        this.resync = false;
        const r = await this.request("GET", "/api/v1/nodes/self/config", undefined, this.etag ? { "if-none-match": this.etag } : {});
        if (!r || r.status !== 200) continue;
        this.etag = r.headers.get("etag");
        const { revision } = r.body.config;
        const ack = { revision, configDigest: r.body.meta.configDigest, bootId: this.bootId };
        for (const stage of ["received", "validated", "applied"]) await this.request("POST", "/api/v1/nodes/self/acks", { ...ack, stage });
        this.applied = revision;
        this.appliedAt.set(revision, Date.now());
      } while (this.resync && !this.stopped);
    })().finally(() => (this.syncing = null));
    return this.syncing;
  }

  pollLoop() {
    void this.sync().finally(() => this.later(POLL_S * 1000 * (0.8 + rand(0.4)), () => this.pollLoop()));
  }

  reportLoop() {
    const report = {
      reportVersion: 1,
      nodeId: this.id,
      bootId: this.bootId,
      sequence: ++this.sequence,
      observedAt: new Date().toISOString(),
      sync: {
        mode: this.roaming ? "poll" : "push",
        transport: this.roaming ? "https" : "websocket",
        behavior: this.roaming ? "roaming" : "static",
        state: "in-sync",
        backoff: { failureLevel: this.level, windowSeconds: 0, probing: false },
      },
      packages: ["meduza-np-wireguard", "meduza-np-tinc", "meduza-np-dnsmasq"].map((name) => ({ name, version: "0.1.0", role: "service-adapter", manifestDigest: "load", registration: "registered", provides: [] })),
      features: [],
      instances: [],
    };
    void this.request("POST", "/api/v1/nodes/self/reports", report).catch(() => stats.errors++).finally(() => this.later(REPORT_S * 1000 * (0.9 + rand(0.2)), () => this.reportLoop()));
  }
}

async function pool(items, limit, fn) {
  let i = 0;
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, async () => {
    while (i < items.length) await fn(items[i++]);
  }));
}

async function waitFor(label, pred, timeoutMs, pending) {
  const start = Date.now();
  let nextDump = start + 60_000;
  while (!pred()) {
    if (pending && Date.now() > nextDump) {
      nextDump += 60_000;
      for (const n of pending().slice(0, 5)) log(`  still waiting for ${n.id}: ${n.note} last=${n.lastFail} connected=${n.connected} level=${n.level} token=${!!n.token}`);
    }
    if (Date.now() - start > timeoutMs) throw new Error(`timeout waiting for ${label}`);
    await sleep(100);
  }
  return Date.now() - start;
}

/** Publish and wait until every changed node ACKed `applied` for its new revision. */
async function measureFanOut(nodes, label, touch) {
  phase = label;
  const pub = await publish(nodes, touch);
  const targets = nodes.filter((n) => pub.changed.has(n.id));
  // Roaming nodes only see it at their next poll, so the bound is the poll interval plus slack.
  const done = (n) => n.appliedAt.has(pub.changed.get(n.id));
  await waitFor(`${label} applied`, () => targets.every(done), (POLL_S * 1.3 + 120) * 1000).catch((e) => {
    const missing = targets.filter((n) => !done(n)).slice(0, 10);
    for (const n of missing) log(`  missing ${n.id}: roaming=${n.roaming} connected=${n.connected} applied=${n.applied} want=${pub.changed.get(n.id)} syncing=${!!n.syncing} level=${n.level}`);
    throw e;
  });
  const lat = (filter) => targets.filter(filter).map((n) => n.appliedAt.get(pub.changed.get(n.id)) - pub.at);
  const result = { nodes: targets.length, publishMs: pub.publishMs, pushedAppliedMs: summary(lat((n) => !n.roaming)), polledAppliedMs: summary(lat((n) => n.roaming)) };
  log(`${label}: ${targets.length} nodes, publish ${pub.publishMs} ms, push→applied p95 ${result.pushedAppliedMs.p95} ms, max ${result.pushedAppliedMs.max} ms`);
  return result;
}

function dirBytes(dir) {
  let total = 0;
  for (const f of readdirSync(dir, { withFileTypes: true })) total += f.isDirectory() ? dirBytes(join(dir, f.name)) : statSync(join(dir, f.name)).size;
  return total;
}

function memory(filter) {
  const s = samples.filter(filter);
  if (!s.length) return null;
  const mib = (b) => Math.round((b / 1048576) * 10) / 10;
  const max = (k) => mib(Math.max(...s.map((x) => x[k])));
  return {
    rssMiBMax: max("rss"),
    rssMiBLast: mib(s.at(-1).rss),
    ...(opt.container
      ? { cgroupMiBMax: max("cgroup"), cgroupMiBLast: mib(s.at(-1).cgroup), anonMiBMax: max("anon") }
      : { heapMiBMax: max("heapUsed") }),
    cpuPctAvg: Math.round((s.reduce((a, x) => a + x.cpuPct, 0) / s.length) * 10) / 10,
    cpuPctMax: Math.round(Math.max(...s.map((x) => x.cpuPct)) * 10) / 10,
  };
}

// ---- scenario -----------------------------------------------------------------------------------------------

const result = {
  params: { nodes: N, roamingRatio: ROAMING, rampSeconds: RAMP_S, steadySeconds: DURATION_S, reportIntervalSeconds: REPORT_S, pollIntervalSeconds: POLL_S, startupSpreadSeconds: SPREAD_S, restart: opt.restart, env: opt.env },
  host: { platform: process.platform, arch: process.arch, node: process.version, cpus: cpus().length, cpuModel: cpus()[0]?.model, memMiB: Math.round(totalmem() / 1048576) },
};
const nodes = Array.from({ length: N }, (_, i) => new SimNode(i, i < Math.round(N * ROAMING)));
const statics = nodes.filter((n) => !n.roaming);
const streams = async () => (await (await fetch(`${BASE}/readyz`)).json()).streams ?? 0;

try {
  result.managerStartMs = await startManager();
  log(`manager ready on ${BASE} (${opt.container ? `container ${ctr.name}, cgroup ${ctr.cgroup}, memory.max ${ctr.memoryMax}` : `data ${dataDir}`})`);
  await bootstrapAdmin();

  phase = "enroll";
  let t = Date.now();
  const tokens = new Map();
  await pool(nodes, 8, async (n) => {
    await adminCall("POST", "/api/v1/nodes", { id: n.id, behavior: n.roaming ? "roaming" : "static" });
    tokens.set(n.id, (await adminCall("POST", `/api/v1/nodes/${n.id}/enrollment-tokens`, {})).token);
  });
  result.createNodesMs = Date.now() - t;
  t = Date.now();
  await pool(nodes, 32, (n) => n.enroll(tokens.get(n.id)));
  result.enrollMs = Date.now() - t;
  log(`created ${N} nodes in ${result.createNodesMs} ms, enrolled in ${result.enrollMs} ms`);

  phase = "connect";
  t = Date.now();
  for (const n of nodes) n.start(rand(RAMP_S * 1000));
  result.connectAllMs = await waitFor("all streams", () => statics.every((n) => n.connected), (RAMP_S + 600) * 1000, () => statics.filter((n) => !n.connected));
  log(`${statics.length} static nodes connected in ${result.connectAllMs} ms (manager streams: ${await streams()})`);

  result.fanOutAll = await measureFanOut(nodes, "publish-all", () => true);

  phase = "steady";
  const steadyStart = Date.now();
  await sleep(DURATION_S * 1000);
  result.steady = memory((s) => s.phase === "steady" && s.at >= steadyStart + 5000);

  result.fanOutOne = await measureFanOut(nodes, "publish-one", (n) => n.index === N - 1);

  if (opt.restart) {
    phase = "restart";
    const before = { ...stats.overloaded, rejected: stats.wsRejected };
    t = Date.now();
    await stopManager();
    const down = Date.now() - t;
    await sleep(2000);
    t = Date.now();
    result.restart = { stopMs: down, readyMs: await startManager() };
    result.restart.reconnectAllMs = await waitFor("reconnect", () => statics.every((n) => n.connected), (SPREAD_S + 900) * 1000, () => statics.filter((n) => !n.connected));
    result.restart.overloadAnswers = { 429: stats.overloaded[429] - before[429], 503: stats.overloaded[503] - before[503], wsRejected: stats.wsRejected - before.rejected };
    log(`restart: ready in ${result.restart.readyMs} ms, all ${statics.length} static nodes back in ${result.restart.reconnectAllMs} ms`);
    result.fanOutAfterRestart = await measureFanOut(nodes, "publish-after-restart", () => true);
  }

  phase = "done";
  result.memoryOverall = memory(() => true);
  result.requests = { byStatus: stats.status, overloaded: stats.overloaded, wsRejected: stats.wsRejected, wsReconnects: stats.wsReconnects, transportErrors: stats.errors };
  result.latencyMs = Object.fromEntries(Object.entries(stats.latency).map(([k, v]) => [k, summary(v)]));
} finally {
  for (const n of nodes) n.stop();
  await stopManager();
  if (opt.container) {
    result.container = { image: opt.container, memory: opt.memory ?? null, cpus: opt["container-cpus"] ?? null, cgroup: ctr.cgroup ?? null, memoryMax: ctr.memoryMax ?? null, stops: ctr.stops };
    try {
      result.dataDirBytes = await containerDataBytes();
    } catch (e) {
      log(`data volume size: ${e.message}`);
    }
    await removeContainer();
  }
}
if (!opt.container) {
  result.dataDirBytes = dirBytes(dataDir);
  result.stops = ctr.stops;
}
result.dataDirMiB = Math.round((result.dataDirBytes / 1048576) * 100) / 100;

const json = JSON.stringify(result, null, 2);
console.log(json);
if (opt.out) writeFileSync(opt.out, json + "\n");
if (!opt.keep) rmSync(work, { recursive: true, force: true });
else log(`kept ${work}`);
process.exit(0);
