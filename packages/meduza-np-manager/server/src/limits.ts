// Server-side capacity protection for node traffic (AGENT.md §7.5.3): bounded concurrency, per-node rate,
// handshake rate and stream count. Overload answers early with 429/503 + a jittered Retry-After so recovering
// nodes spread out instead of retrying in lockstep.

import type { Limits } from "./config.js";

export class Overloaded extends Error {
  constructor(readonly status: 429 | 503, readonly retryAfterSeconds: number, readonly reason: string) {
    super(reason);
  }
}

class TokenBucket {
  private tokens: number;
  private last: number;
  constructor(private readonly capacity: number, private readonly refillPerSecond: number, now = Date.now()) {
    this.tokens = capacity;
    this.last = now;
  }
  take(now = Date.now()): number {
    this.tokens = Math.min(this.capacity, this.tokens + ((now - this.last) / 1000) * this.refillPerSecond);
    this.last = now;
    if (this.tokens >= 1) {
      this.tokens -= 1;
      return 0;
    }
    return Math.ceil((1 - this.tokens) / this.refillPerSecond);
  }
}

/** `base` seconds plus uniform jitter of up to `spread` seconds. */
export function jitteredRetryAfter(base: number, spread: number, rand: () => number = Math.random): number {
  return Math.max(1, Math.ceil(base + rand() * spread));
}

export class NodeTrafficLimiter {
  private inFlight = 0;
  private perNode = new Map<string, TokenBucket>();
  private handshakes: TokenBucket;
  private streams = 0;

  constructor(private readonly limits: Limits, private readonly rand: () => number = Math.random) {
    this.handshakes = new TokenBucket(limits.maxStreamHandshakesPerSecond, limits.maxStreamHandshakesPerSecond);
  }

  /** Admits a short node request; call the returned release when done. */
  admit(nodeId: string | null): () => void {
    if (this.inFlight >= this.limits.maxConcurrentNodeRequests) {
      throw new Overloaded(503, jitteredRetryAfter(5, 25, this.rand), "manager busy");
    }
    if (nodeId) {
      let bucket = this.perNode.get(nodeId);
      if (!bucket) {
        // Burst of a quarter minute worth of requests, refilled continuously.
        bucket = new TokenBucket(Math.max(4, Math.ceil(this.limits.perNodeRequestsPerMinute / 4)), this.limits.perNodeRequestsPerMinute / 60);
        this.perNode.set(nodeId, bucket);
        if (this.perNode.size > 100_000) this.perNode.clear();
      }
      const wait = bucket.take();
      if (wait > 0) throw new Overloaded(429, jitteredRetryAfter(wait, Math.min(30, wait + 5), this.rand), "per-node rate exceeded");
    }
    this.inFlight++;
    let released = false;
    return () => {
      if (!released) {
        released = true;
        this.inFlight--;
      }
    };
  }

  /** Admits a push-channel handshake; the stream slot is held until the returned release is called. */
  admitStream(): () => void {
    if (this.streams >= this.limits.maxConcurrentStreams) throw new Overloaded(503, jitteredRetryAfter(30, 60, this.rand), "too many streams");
    const wait = this.handshakes.take();
    if (wait > 0) throw new Overloaded(503, jitteredRetryAfter(wait, 30, this.rand), "handshake rate exceeded");
    this.streams++;
    let released = false;
    return () => {
      if (!released) {
        released = true;
        this.streams--;
      }
    };
  }

  stats() {
    return { inFlight: this.inFlight, streams: this.streams, trackedNodes: this.perNode.size };
  }
}
