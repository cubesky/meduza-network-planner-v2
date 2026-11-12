// Pure BGP path types/helpers shared by the topology API and its browser path table.
export interface BgpPath extends Record<string, unknown> {
  node: string;
  address: string;
  neighbor: string;
  prefix: string;
  family: "ipv4" | "ipv6";
  asPath: string | null;
  valid: boolean | null;
  best: boolean;
  nextHops: string[];
}

export interface BgpPathReport {
  node: string;
  complete: boolean;
  available: boolean;
  addressFamilies: string[];
  errors: string[];
}

export function asnNumber(token: string): number | undefined {
  if (!/^\d+(\.\d+)?$/.test(token)) return undefined;
  const dotted = token.split(".").map(Number);
  if (dotted.length === 2 && dotted.some((n) => n > 65535)) return undefined;
  const n = dotted.length === 2 ? dotted[0]! * 65536 + dotted[1]! : dotted[0]!;
  return Number.isInteger(n) && n > 0 && n < 4294967295 ? n : undefined;
}

/** Sets do not imply an order. Preserve known members, break inferred adjacency at unordered/unknown
 * segments, and continue afterwards instead of dropping the rest of the AS_PATH. */
export function asPathSegments(text: string): Array<{ ordered: boolean; asns: number[] }> {
  return (text.match(/\{[^}]*\}|\([^)]*\)|\[[^\]]*\]|\S+/g) ?? []).map((token) => {
    const grouped = /^[{([]/.test(token);
    const asns = (grouped ? token.slice(1, -1).split(/[,\s]+/) : [token])
      .map(asnNumber).filter((n): n is number => n !== undefined);
    return { ordered: (!grouped || token.startsWith("(")) && asns.length > 0, asns };
  });
}

export function pathPage(paths: BgpPath[], query: string, page: number, names: Record<string, string> = {}, size = 50) {
  const words = query.trim().toLowerCase().split(/\s+/).filter(Boolean);
  const filtered = words.length ? paths.filter((p) => {
    const ases = asPathSegments(p.asPath ?? "").flatMap((s) => s.asns).map((asn) => `AS${asn} ${names[asn] ?? ""}`).join(" ");
    const text = `${p.node} ${p.neighbor} ${p.address} ${p.prefix} ${p.family} ${p.asPath ?? ""} ${p.nextHops.join(" ")} ${ases}`.toLowerCase();
    return words.every((word) => text.includes(word));
  }) : paths;
  const pageCount = Math.max(1, Math.ceil(filtered.length / size));
  const current = Math.max(0, Math.min(pageCount - 1, page));
  return { rows: filtered.slice(current * size, (current + 1) * size), total: filtered.length, page: current, pageCount };
}
