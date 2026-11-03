// DNS resolution loops on one node (AGENT §9.7, §9.9, §9.10): dnsmasq is the client-facing front end, MosDNS and
// Mihomo DNS are its backends. A fallback, backend, upstream, nameserver or bootstrap resolver that points back at
// the front end makes every query circle until it times out. The plugins refuse such a configuration on the node
// (the revision fails there and the previous one keeps running, found in the lab); checking the same rules here shows
// the problem at the field before publishing.

import type { Problem } from "../cluster.js";

type Rec = Record<string, unknown>;

interface Addr {
  ip: string;
  port: number;
}

const enabled = (s: unknown): s is Rec => !!s && typeof s === "object" && (s as Rec).enabled !== false;
const strings = (v: unknown): string[] => (Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : []);

/** `ip`, `ip#port` (dnsmasq), `ip:port`, `[v6]:port` or `scheme://host[:port][/path]`; plain DNS defaults to 53. */
export function parseResolver(s: string): Addr | null {
  let rest = s.trim();
  const scheme = /^([a-z]+):\/\//.exec(rest);
  if (scheme) {
    // only plain DNS can reach the front end's listener; DoT/DoH/QUIC use other ports and names
    if (!["udp", "tcp"].includes(scheme[1]!)) return null;
    rest = rest.slice(scheme[0].length).split("/")[0]!;
  }
  let ip = rest;
  let port = 53;
  rest = rest.split("?")[0]!;
  const fragment = rest.split("#");
  const hash = fragment.length === 2 && /^\d+$/.test(fragment[1]!) ? fragment : [fragment[0]!];
  rest = hash[0]!;
  ip = rest;
  const v6 = /^\[([^\]]+)\](?::(\d+))?$/.exec(rest);
  if (hash.length === 2) [ip, port] = [hash[0]!, Number(hash[1])];
  else if (v6) [ip, port] = [v6[1]!, v6[2] ? Number(v6[2]) : 53];
  else if ((rest.match(/:/g) ?? []).length === 1) [ip, port] = [rest.split(":")[0]!, Number(rest.split(":")[1])];
  if (!Number.isInteger(port) || port < 1 || port > 65535 || !ip) return null;
  return { ip: ip.toLowerCase() === "localhost" ? "127.0.0.1" : ip.toLowerCase(), port };
}

const loopback = (ip: string) => ip.startsWith("127.") || ip === "::1";
const wildcard = (ip: string) => ip === "0.0.0.0" || ip === "::";

/** Does a query sent to `a` arrive at the listener `l`? */
function reaches(a: Addr, l: Addr): boolean {
  return a.port === l.port && (a.ip === l.ip || (wildcard(l.ip) && (loopback(a.ip) || wildcard(a.ip))));
}

export function dnsLoopProblems(nodeId: string, services: Rec): Problem[] {
  const front = enabled(services.dnsmasq) ? services.dnsmasq : undefined;
  const port = typeof front?.port === "number" ? front.port : 53;
  const configured = strings(front?.listenAddresses);
  const listeners = (front && !configured.length ? ["0.0.0.0", "::"] : configured).map((ip) => ({ ip: ip.toLowerCase(), port }));
  const problems: Problem[] = [];
  const base = `/nodes/${nodeId}/services`;
  const check = (path: string, target: string | Addr | null, message: string) => {
    const a = typeof target === "string" ? parseResolver(target) : target;
    if (a && listeners.some((l) => reaches(a, l))) problems.push({ path: `${base}/${path}`, node: nodeId, message });
  };
  const LOOP = "points back at the dnsmasq front end (resolution loop)";

  for (const [i, s] of strings(front?.fallbackServers).entries()) check(`dnsmasq/fallbackServers/${i}`, s, `fallback server ${LOOP}`);
  const mosdns = services.mosdns;
  if (enabled(mosdns)) {
    const listen = { ip: String(mosdns.listen ?? "127.0.0.1").toLowerCase(), port: typeof mosdns.port === "number" ? mosdns.port : 1153 };
    check("mosdns/port", listen, "MosDNS must use its own listener, not the front end's");
    for (const field of ["localUpstreams", "remoteUpstreams"]) {
      for (const [i, u] of strings(mosdns[field]).entries()) check(`mosdns/${field}/${i}`, u, `upstream ${LOOP}`);
    }
    if (typeof mosdns.bootstrap === "string") check("mosdns/bootstrap", mosdns.bootstrap, `bootstrap resolver ${LOOP}`);
  }

  const dns = enabled(services.mihomo) ? (services.mihomo as Rec).dns : undefined;
  if (dns && typeof dns === "object" && (dns as Rec).enabled === true) {
    const d = dns as Rec;
    const listen = parseResolver(String(d.listen ?? "127.0.0.1:1053"));
    if (listen && listeners.some((l) => l.port === listen.port && (l.ip === listen.ip || wildcard(l.ip) || wildcard(listen.ip)))) {
      problems.push({ path: `${base}/mihomo/dns/listen`, node: nodeId, message: "Mihomo DNS must use its own listener, not the front end's" });
    }
    const resolver = (field: string, value: string) => {
      check(`mihomo/dns/${field}`, value, `nameserver ${LOOP}`);
      const a = parseResolver(value);
      if (listen && a && reaches(a, listen)) problems.push({ path: `${base}/mihomo/dns/${field}`, node: nodeId, message: "nameserver points back at Mihomo DNS itself (resolution loop)" });
    };
    for (const field of ["nameservers", "defaultNameservers", "fallback", "proxyServerNameservers", "directNameservers"]) {
      for (const [i, n] of strings(d[field]).entries()) resolver(`${field}/${i}`, n);
    }
    for (const field of ["nameserverPolicy", "proxyServerNameserverPolicy"]) {
      if (!d[field] || typeof d[field] !== "object") continue;
      for (const [key, value] of Object.entries(d[field] as Rec)) {
        const path = `${field}/${key.replace(/~/g, "~0").replace(/\//g, "~1")}`;
        if (typeof value === "string") resolver(path, value);
        else for (const n of strings(value)) resolver(path, n);
      }
    }
    if (d.respectRules === true && !strings(d.proxyServerNameservers).length) problems.push({
      path: `${base}/mihomo/dns/proxyServerNameservers`, node: nodeId, message: "respectRules requires independent proxy-server nameservers",
    });
  }
  return problems;
}
