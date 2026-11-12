import { useState } from "preact/hooks";
import { Badge, Button, Table, t } from "@meduza-np/ui-kit";
import { pathPage, type BgpPath, type BgpPathReport } from "../../server/src/bgp-paths.ts";
import { asPathSegments } from "../../server/src/bgp-paths.ts";

export type { BgpPath, BgpPathReport } from "../../server/src/bgp-paths.ts";

export function BgpPaths(props: { paths: BgpPath[]; reports?: BgpPathReport[]; names?: Record<string, string> }) {
  const [query, setQuery] = useState("");
  const [page, setPage] = useState(0);
  const [state, setState] = useState("all");
  const filtered = props.paths.filter((p) => state === "all" || (state === "best" ? p.best : state === "valid" ? p.valid === true && !p.best : p.valid === false));
  const result = pathPage(filtered, query, page, props.names);
  const incomplete = props.reports?.filter((r) => !r.complete) ?? [];
  return <div class="bgp-paths">
    {incomplete.map((r) => <div class="callout warn">
      <strong>{r.node}: </strong>{r.available || r.errors.length ? t("BGP path collection is incomplete") : t("Update this node to report all retained BGP paths")}
      {r.errors.length > 0 && <span> · {r.errors.join("; ")}</span>}
    </div>)}
    <p class="muted small">{t("All received paths currently retained by FRR, including non-best and invalid paths. Routes rejected before storage are not included.")}</p>
    <div class="row" style={{ gap: "8px", marginBottom: "8px" }}>
      <input type="text" style={{ flex: "1 1 260px", width: "auto", minWidth: "160px", maxWidth: "440px" }} aria-label={t("Search BGP paths")} placeholder={t("Search node, neighbor, prefix or AS")}
        value={query} onInput={(e) => { setQuery(e.currentTarget.value); setPage(0); }} />
      <select style={{ flex: "0 1 180px", width: "auto" }} aria-label={t("Path state")} value={state} onChange={(e) => { setState(e.currentTarget.value); setPage(0); }}>
        <option value="all">{t("All paths")}</option><option value="best">{t("Best path")}</option>
        <option value="valid">{t("Non-best path")}</option><option value="invalid">{t("Invalid path")}</option>
      </select>
      <span class="muted">{t("{count} paths", { count: result.total })}</span>
      <span class="spacer" />
      <Button small disabled={result.page === 0} onClick={() => setPage(result.page - 1)}>{t("Previous")}</Button>
      <span>{t("Page {page} of {total}", { page: result.page + 1, total: result.pageCount })}</span>
      <Button small disabled={result.page + 1 === result.pageCount} onClick={() => setPage(result.page + 1)}>{t("Next")}</Button>
    </div>
    <Table rows={result.rows.map((p, i) => ({ ...p, rowId: result.page * 50 + i }))} rowKey={(p) => String(p.rowId)} empty={t("No retained BGP paths reported")}
      columns={[
        { key: "node", label: t("Node") },
        { key: "address", label: t("Neighbor"), render: (p) => <span title={p.neighbor}>{p.address}</span> },
        { key: "family", label: t("Address family"), render: (p) => p.family === "ipv6" ? "IPv6" : "IPv4" },
        { key: "prefix", label: t("Prefix"), render: (p) => <span class="mono">{p.prefix}</span> },
        { key: "asPath", label: t("AS path"), render: (p) => {
          const named = [...new Set(asPathSegments(p.asPath ?? "").flatMap((s) => s.asns))].filter((asn) => props.names?.[asn]);
          return <span style={{ whiteSpace: "normal", overflowWrap: "anywhere" }}><span class="mono">{p.asPath === "" ? t("Empty AS path") : p.asPath ?? "—"}</span>
            {named.length > 0 && <span class="small muted" style={{ display: "block" }}>{named.map((asn) => `AS${asn} ${props.names![asn]}`).join(" · ")}</span>}</span>;
        } },
        { key: "valid", label: t("Path state"), render: (p) => <Badge tone={p.best ? "ok" : p.valid === false ? "bad" : "idle"}>{p.best ? t("Best path") : p.valid === true ? t("Non-best path") : p.valid === false ? t("Invalid path") : t("Unknown")}</Badge> },
        { key: "nextHops", label: t("Next hop"), render: (p) => <span class="mono small">{p.nextHops.join(", ") || "—"}</span> },
      ]} />
  </div>;
}
