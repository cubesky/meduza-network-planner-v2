// Push channel hub (AGENT.md §7.3): one outbound WebSocket per static node, small notifications only.
// The node always fetches the full snapshot over HTTPS after a notification.

import type { WebSocket } from "ws";
import type { ConfigService } from "./publish.js";

export const EVENT_VERSION = 1;
const PING_INTERVAL_MS = 30_000;
const MAX_QUEUED_BYTES = 256 * 1024;

export interface StreamHello {
  helloVersion: number;
  lastEventId?: string;
  lastSuccessfulRevision?: string;
}

interface Conn {
  ws: WebSocket;
  nodeId: string;
  alive: boolean;
  release: () => void;
}

export class StreamHub {
  private conns = new Map<string, Set<Conn>>();
  private timer: NodeJS.Timeout;

  constructor(private readonly config: ConfigService) {
    this.timer = setInterval(() => this.heartbeat(), PING_INTERVAL_MS);
    this.timer.unref();
  }

  get size(): number {
    let n = 0;
    for (const s of this.conns.values()) n += s.size;
    return n;
  }

  connected(nodeId: string): boolean {
    return (this.conns.get(nodeId)?.size ?? 0) > 0;
  }

  /** Registers an authenticated socket. Waits for the hello frame, sends welcome, missed events, then live ones. */
  attach(ws: WebSocket, nodeId: string, release: () => void): void {
    const conn: Conn = { ws, nodeId, alive: true, release };
    let greeted = false;
    const helloTimeout = setTimeout(() => ws.close(4400, "hello timeout"), 10_000);
    ws.on("pong", () => (conn.alive = true));
    ws.on("message", (data, isBinary) => {
      conn.alive = true;
      if (greeted || isBinary) return; // after hello the channel is server → node only
      let hello: StreamHello;
      try {
        hello = JSON.parse(String(data)) as StreamHello;
      } catch {
        ws.close(4400, "invalid hello");
        return;
      }
      if (hello.helloVersion !== 1) {
        ws.close(4409, "unsupported hello version");
        return;
      }
      greeted = true;
      clearTimeout(helloTimeout);
      // Keep only one stream per node: a reconnect replaces the old socket (§7.5.2).
      for (const old of this.conns.get(nodeId) ?? []) old.ws.close(4000, "replaced by newer connection");
      let set = this.conns.get(nodeId);
      if (!set) this.conns.set(nodeId, (set = new Set()));
      set.add(conn);
      const desired = this.config.desired(nodeId);
      const cursor = hello.lastEventId ? Number(hello.lastEventId) : NaN;
      const resume = Number.isInteger(cursor) && cursor >= 0 ? this.config.eventsAfter(nodeId, cursor) : { events: [], cursorValid: false };
      this.send(conn, { desiredRevision: desired?.revision ?? null, configDigest: desired?.digest, cursorValid: resume.cursorValid });
      for (const e of resume.events) this.send(conn, toEvent(nodeId, e));
    });
    ws.on("close", () => {
      clearTimeout(helloTimeout);
      this.conns.get(nodeId)?.delete(conn);
      if (this.conns.get(nodeId)?.size === 0) this.conns.delete(nodeId);
      conn.release();
    });
    ws.on("error", () => ws.terminate());
  }

  notify(nodeId: string, event: { seq: number; type: string; revision?: string | null; digest?: string | null; jobId?: string | null }): void {
    for (const conn of this.conns.get(nodeId) ?? []) {
      this.send(conn, toEvent(nodeId, { seq: event.seq, type: event.type, revision: event.revision ?? null, digest: event.digest ?? null, job_id: event.jobId ?? null }));
    }
  }

  /** Server is shedding load: ask connected nodes to stay quiet for a while (they add their own jitter). */
  broadcastCooldown(retryAfterMs: number): void {
    for (const [nodeId, set] of this.conns) {
      for (const conn of set) this.send(conn, { eventVersion: EVENT_VERSION, eventId: "cooldown", type: "server.cooldown", nodeId, retryAfterMs });
    }
  }

  disconnect(nodeId: string, reason: string): void {
    for (const conn of this.conns.get(nodeId) ?? []) conn.ws.close(4401, reason);
  }

  closeAll(reason: string): void {
    clearInterval(this.timer);
    for (const set of this.conns.values()) for (const conn of set) conn.ws.close(1001, reason);
  }

  private send(conn: Conn, payload: unknown): void {
    // A slow consumer must not grow memory without bound: drop it, it will resync over HTTPS.
    if (conn.ws.bufferedAmount > MAX_QUEUED_BYTES) {
      conn.ws.terminate();
      return;
    }
    conn.ws.send(JSON.stringify(payload));
  }

  private heartbeat(): void {
    for (const set of this.conns.values()) {
      for (const conn of set) {
        if (!conn.alive) {
          conn.ws.terminate();
          continue;
        }
        conn.alive = false;
        conn.ws.ping();
      }
    }
  }
}

function toEvent(nodeId: string, e: Record<string, unknown>) {
  return {
    eventVersion: EVENT_VERSION,
    eventId: String(e.seq),
    type: e.type,
    nodeId,
    ...(e.revision ? { revision: e.revision } : {}),
    ...(e.digest ? { configDigest: e.digest } : {}),
    ...(e.job_id ? { jobId: e.job_id } : {}),
  };
}
