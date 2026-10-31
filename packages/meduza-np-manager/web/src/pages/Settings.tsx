// Global settings (cluster-wide, D21): internal ASN, BFD timers and ECMP paths (D24), the Anycast catalog, defaults
// for every node and the OpenVPN
// certificate authority. Edits go into the same working copy as node settings and are applied through the apply bar;
// CA operations take effect immediately (they create keys, not configuration).

import { useState } from "preact/hooks";
import { Badge, Button, Card, Checkbox, Field, ListInput, Loadable, Modal, PageHead, Select, Table, Tabs, TextInput, ToggleField, ask, askText, msg, t, toast, useAsync } from "@meduza-np/ui-kit";
import { errorText, get, post } from "../api.ts";
import { isError, problemsFor, update, useConfig, type ClusterDoc as Doc, type MihomoCatalog, type MihomoSubscription, type Problem } from "../store.ts";
import { ProblemList } from "../ApplyBar.tsx";

const SYNC_FIELDS = [
  ["heartbeatSeconds", msg("Heartbeat")],
  ["pushKeepaliveSeconds", msg("Push keepalive")],
  ["roamingPollSeconds", msg("Roaming poll")],
  ["calibrationSeconds", msg("Clock calibration")],
] as const;

type Role = "admin" | "operator" | "viewer";

// The page's sections, each its own tab (address #/settings/<id>); `owns` claims the problems shown on the tab.
const TABS = [
  { id: "routing", label: msg("Routing"), owns: (p: Problem) => p.path.startsWith("/routing") && !p.path.startsWith("/routing/anycast") },
  { id: "anycast", label: msg("Anycast"), owns: (p: Problem) => p.path.startsWith("/routing/anycast") },
  { id: "dns", label: msg("DNS"), owns: (p: Problem) => p.path.startsWith("/dns") || p.path.startsWith("/mosdns") || p.path.includes("/services/mosdns") },
  { id: "tinc", label: msg("Tinc mesh"), owns: (p: Problem) => p.path.startsWith("/tinc") },
  { id: "mihomo", label: msg("Mihomo subscriptions"), owns: (p: Problem) => p.path.startsWith("/mihomo") },
  { id: "defaults", label: msg("Node defaults"), owns: (p: Problem) => p.path.startsWith("/defaults") },
  { id: "pki", label: msg("Certificates"), owns: (_: Problem) => false },
] as const;

export function SettingsPage(props: { role: Role; tab?: string }) {
  const s = useConfig();
  const tab = TABS.some((x) => x.id === props.tab) ? props.tab! : "routing";
  const editable = props.role !== "viewer";
  if (s.error && !s.loaded) return <div class="callout bad">{t("Could not load the configuration: {error}", { error: s.error })}</div>;
  if (!s.loaded) return <div class="state-box">{t("Loading…")}</div>;
  const doc = s.doc;
  const problems = problemsFor(s.preview, null);
  // MosDNS problems are reported on the nodes that run it
  const all = [...problems, ...(s.preview?.problems ?? []).filter((p) => p.path.includes("/services/mosdns") && !problems.includes(p))];
  const errorsOf = (id: string) => all.filter((p) => isError(p) && TABS.find((x) => x.id === id)!.owns(p));
  const unowned = problems.filter((p) => !TABS.some((x) => x.owns(p)));
  const mine = all.filter((p) => TABS.find((x) => x.id === tab)!.owns(p));
  const errorAt = (path: string) => problems.find((p) => p.path === path)?.message;
  const change = (next: Doc) => update(() => next);
  const defaults = doc.defaults ?? {};
  const setDefaults = (k: string, v: unknown) => change({ ...doc, defaults: { ...defaults, [k]: v } });
  const up = defaults.updatePolicy ?? {};
  // Sets one routing setting; an emptied routing object disappears.
  const setRouting = (k: string, v: unknown) => {
    const { [k]: _old, ...rest } = (doc.routing ?? {}) as Record<string, unknown>;
    const next = v === undefined ? rest : { ...rest, [k]: v };
    change({ ...doc, routing: Object.keys(next).length ? next : undefined });
  };
  const bfd = (doc.routing?.bfd ?? {}) as Record<string, number | boolean | undefined>;
  const setBfd = (k: string, v: number | boolean | undefined) => {
    const { [k]: _old, ...rest } = bfd;
    const next = v === undefined ? rest : { ...rest, [k]: v };
    setRouting("bfd", Object.keys(next).length ? next : undefined);
  };
  const num = (e: Event) => {
    const v = (e.target as HTMLInputElement).value;
    return v ? Number(v) : undefined;
  };
  return (
    <div>
      <PageHead title={t("Global settings")} sub={t("Settings shared by every node. Save your edits, then apply them from Changes.")} />
      <ProblemList problems={unowned.filter(isError)} tone="bad" />
      <ProblemList problems={unowned.filter((p) => !isError(p))} tone="warn" />
      <Tabs
        tabs={TABS.map((x) => {
          const n = errorsOf(x.id).length;
          return { id: x.id, label: t(x.label), count: n || undefined, tone: n ? "bad" as const : undefined };
        })}
        active={tab}
        onChange={(id) => (location.hash = `/settings/${id}`)}
      />
      {tab !== "anycast" && tab !== "mihomo" && tab !== "dns" && <ProblemList problems={mine.filter(isError)} tone="bad" />}
      {tab !== "anycast" && tab !== "mihomo" && tab !== "dns" && <ProblemList problems={mine.filter((p) => !isError(p))} tone="warn" />}
      {tab === "routing" && <Card title={t("Routing")}>
        <div class="form-grid">
          <Field label={t("Internal ASN")} error={errorAt("/routing/internalAsn")} help={t("The AS number of the internal network: iBGP between mesh members uses it.")}>
            <input type="number" value={doc.routing?.internalAsn ?? ""} disabled={!editable} onInput={(e) => setRouting("internalAsn", num(e))} />
          </Field>
          <Field label={t("ECMP paths")} error={errorAt("/routing/maximumPaths")} help={t("Equal-cost paths installed per prefix (FRR maximum-paths).")}>
            <input type="number" min={1} max={64} value={doc.routing?.maximumPaths ?? ""} placeholder={t("default")} disabled={!editable} onInput={(e) => setRouting("maximumPaths", num(e))} />
          </Field>
        </div>
        <p class="small muted">{t("Every Tinc mesh member exchanges the internal routes over iBGP; external connections carry their BGP settings (remote ASN, prefixes) in their WireGuard peer or OpenVPN instance.")}</p>
        <h3>{t("BGP prefix filters")}</h3>
        <p class="small muted">{t("Applied to every eBGP session that has no filter of its own, first match wins: permit|deny <IPv4 network> [ge N] [le N], one rule per line. A prefix no rule matches is accepted.")}</p>
        <fieldset disabled={!editable} style={{ border: 0, padding: 0, margin: 0, minWidth: 0 }}>
          <div class="form-grid">
            {(["in", "out"] as const).map((dir) => (
              <Field label={dir === "in" ? t("Received routes") : t("Advertised routes")} error={problems.find((p) => p.path.startsWith(`/routing/bgpFilter/${dir}/`))?.message}>
                <ListInput value={doc.routing?.bgpFilter?.[dir]} placeholder="deny 0.0.0.0/0" onChange={(v) => {
                  const { [dir]: _old, ...rest } = doc.routing?.bgpFilter ?? {};
                  const next = v.length ? { ...rest, [dir]: v } : rest;
                  setRouting("bgpFilter", Object.keys(next).length ? next : undefined);
                }} />
              </Field>
            ))}
          </div>
        </fieldset>
        <h3>{t("BFD")}</h3>
        <fieldset disabled={!editable} style={{ border: 0, padding: 0, margin: 0, minWidth: 0 }}>
          <div class="toggle-grid">
            <ToggleField label={t("BFD on the internal mesh")} help={t("Detects a failed mesh member within a second. eBGP sessions use these timers when they enable BFD; roaming nodes never use BFD.")}
              checked={bfd.enabled !== false} onChange={(v) => setBfd("enabled", v ? undefined : false)} />
          </div>
          <div class="form-grid">
            <Field label={t("Transmit interval (ms)")} error={errorAt("/routing/bfd/transmitInterval")}><input type="number" value={(bfd.transmitInterval as number) ?? ""} placeholder="300" onInput={(e) => setBfd("transmitInterval", num(e))} /></Field>
            <Field label={t("Receive interval (ms)")} error={errorAt("/routing/bfd/receiveInterval")}><input type="number" value={(bfd.receiveInterval as number) ?? ""} placeholder="300" onInput={(e) => setBfd("receiveInterval", num(e))} /></Field>
            <Field label={t("Detect multiplier")} error={errorAt("/routing/bfd/detectMultiplier")}><input type="number" value={(bfd.detectMultiplier as number) ?? ""} placeholder="3" onInput={(e) => setBfd("detectMultiplier", num(e))} /></Field>
          </div>
        </fieldset>
      </Card>}
      {tab === "anycast" && <Card title={t("Anycast")}>
        <AnycastEditor doc={doc} onChange={change} editable={editable} problems={mine} />
      </Card>}
      {tab === "defaults" && <Card title={t("Defaults for all nodes")}>
        <p class="small muted">{t("Used by every node that does not set its own value.")}</p>
        <fieldset disabled={!editable} style={{ border: 0, padding: 0, margin: 0, minWidth: 0 }}>
          <h3>{t("Update policy")}</h3>
          <div class="form-grid">
            <Field label={t("Channel")}><TextInput value={up.channel ?? ""} placeholder="stable" onInput={(v) => setDefaults("updatePolicy", { automatic: false, ...up, channel: v || "stable" })} /></Field>
            <Field label={t("Maintenance window")} help={t("HH:MM-HH:MM")}><TextInput value={up.maintenanceWindow} onInput={(v) => setDefaults("updatePolicy", { channel: "stable", automatic: false, ...up, maintenanceWindow: v || undefined })} /></Field>
            <Field label={t("Timezone")}><TextInput value={up.timezone} onInput={(v) => setDefaults("updatePolicy", { channel: "stable", automatic: false, ...up, timezone: v || undefined })} /></Field>
          </div>
          <div class="toggle-grid">
            <ToggleField label={t("Automatic")} checked={!!up.automatic} onChange={(v) => setDefaults("updatePolicy", { channel: "stable", ...up, automatic: v })} />
          </div>
          <h3>{t("Sync intervals (seconds, jittered)")}</h3>
          <div class="form-grid">
            {SYNC_FIELDS.map(([k, label]) => (
              <Field label={t(label)}>
                <input type="number" value={defaults.sync?.[k] ?? ""} placeholder={t("default")} onInput={(e) => {
                  const v = (e.target as HTMLInputElement).value;
                  setDefaults("sync", { ...defaults.sync, [k]: v ? Number(v) : undefined });
                }} />
              </Field>
            ))}
          </div>
        </fieldset>
      </Card>}
      {tab === "dns" && (
        <>
          <ProblemList problems={mine.filter((p) => isError(p) && !p.path.includes("mosdns"))} tone="bad" />
          <Card title={t("DNS hosts")}>
            <DnsHosts doc={doc} onChange={change} editable={editable} />
          </Card>
          <Card title={t("MosDNS")}>
            <MosdnsShared doc={doc} onChange={change} editable={editable} problems={mine.filter((p) => p.path.includes("mosdns"))} />
          </Card>
        </>
      )}
      {tab === "tinc" && <Card title={t("Tinc mesh")}>
        <TincMesh doc={doc} onChange={change} editable={editable} />
      </Card>}
      {tab === "mihomo" && <Card title={t("Mihomo subscriptions")}>
        <MihomoSubscriptions doc={doc} onChange={change} editable={editable} problems={mine} />
      </Card>}
      {tab === "pki" && <CertificateAuthority role={props.role} />}
    </div>
  );
}

// ---- OpenVPN certificate authority -----------------------------------------------------------------------

interface Ca {
  id: string;
  name: string;
  subject: string;
  notAfter: string;
  certRef: string;
  crlRef: string;
}

interface Cert {
  serial: string;
  caId: string;
  subject: string;
  usage: string;
  notAfter: string;
  revokedAt: string | null;
  node?: string | null;
  expiresInDays: number;
}

function CertificateAuthority(props: { role: Role }) {
  const cas = useAsync(() => get<Ca[]>("/api/v1/pki/cas"), []);
  const certs = useAsync(() => get<Cert[]>("/api/v1/pki/certs"), []);
  const [creating, setCreating] = useState(false);
  const admin = props.role === "admin";
  return (
    <Card title={t("OpenVPN certificate authority")} actions={admin && <Button small onClick={() => setCreating(true)}>{t("New CA")}</Button>}>
      <p class="small muted">{t("OpenVPN instances issue their certificates from here in their node settings. The CA key stays encrypted on the manager and is never delivered to nodes.")}</p>
      <Loadable state={cas} empty={(c) => !c.length} emptyText={t("No CA yet. Create one to issue OpenVPN certificates.")}>
        {(rows) => (
          <Table
            rows={rows as unknown as Record<string, unknown>[]}
            rowKey={(r) => String(r.id)}
            columns={[
              { key: "name", label: t("Name"), render: (r) => <strong>{String(r.name)}</strong> },
              { key: "subject", label: t("Subject"), render: (r) => <span class="small mono">{String(r.subject)}</span> },
              { key: "notAfter", label: t("Valid until"), render: (r) => String(r.notAfter).slice(0, 10) },
            ]}
          />
        )}
      </Loadable>
      <h3>{t("Issued certificates")}</h3>
      <Loadable state={certs} empty={(c) => !c.length} emptyText={t("No certificates issued")}>
        {(rows) => (
          <Table
            rows={rows as unknown as Record<string, unknown>[]}
            rowKey={(r) => String(r.serial)}
            columns={[
              { key: "subject", label: t("Subject"), render: (r) => <span class="small mono">{String(r.subject)}</span> },
              { key: "usage", label: t("Usage"), render: (r) => t(String(r.usage)) },
              { key: "node", label: t("Node"), render: (r) => String(r.node ?? "—") },
              {
                key: "notAfter",
                label: t("Expires"),
                render: (r) => {
                  const days = Number(r.expiresInDays);
                  return <Badge tone={r.revokedAt ? "idle" : days < 0 ? "bad" : days < 30 ? "warn" : "ok"}>{r.revokedAt ? t("revoked") : days < 0 ? t("expired") : t("{days} d", { days })}</Badge>;
                },
              },
              {
                key: "actions",
                label: "",
                render: (r) =>
                  admin && !r.revokedAt && (
                    <Button small variant="danger" onClick={async () => {
                      if (!(await ask(t("Revoke {subject}? Connections using it are refused once nodes receive the new CRL (next publish).", { subject: String(r.subject) }), { ok: t("Revoke"), danger: true }))) return;
                      try {
                        await post(`/api/v1/pki/certs/${r.serial}/revoke`, { reason: "unspecified" });
                        certs.reload();
                      } catch (e) {
                        toast(errorText(e), "bad");
                      }
                    }}>{t("Revoke")}</Button>
                  ),
              },
            ]}
          />
        )}
      </Loadable>
      {creating && <NewCa onClose={() => setCreating(false)} onDone={() => cas.reload()} />}
    </Card>
  );
}

function NewCa(props: { onClose: () => void; onDone: () => void }) {
  const [id, setId] = useState("");
  const [name, setName] = useState("");
  const [alg, setAlg] = useState("ecdsa-p256");
  return (
    <Modal title={t("New certificate authority")} onClose={props.onClose} actions={
      <Button variant="primary" disabled={!id || !name} onClick={async () => {
        try {
          await post("/api/v1/pki/cas", { id, name, algorithm: alg });
          props.onDone();
          props.onClose();
        } catch (e) {
          toast(errorText(e), "bad");
        }
      }}>{t("Create")}</Button>
    }>
      <Field label="ID"><TextInput mono value={id} onInput={setId} placeholder="vpn" /></Field>
      <Field label={t("Name (CN)")}><TextInput value={name} onInput={setName} placeholder="Meduza VPN CA" /></Field>
      <Field label={t("Key algorithm")}><Select value={alg} options={["ecdsa-p256", "rsa-2048", "rsa-3072"]} onChange={setAlg} /></Field>
    </Modal>
  );
}

// ---- Anycast catalog -------------------------------------------------------------------------------------

interface AnycastService {
  id: string;
  description?: string;
  prefix: string;
  health: { kind: "tcp" | "dns" | "http"; port: number; host?: string; name?: string; path?: string };
  metric?: number;
}
interface AnycastCatalog {
  pool: string;
  area?: string;
  services?: AnycastService[];
}

/** First /30 of `pool` no service uses (the manager requires one /30 per service inside the pool). */
function nextFree30(pool: string, used: string[]): string | undefined {
  const m = /^(\d+)\.(\d+)\.(\d+)\.(\d+)\/(\d+)$/.exec(pool.trim());
  if (!m || Number(m[5]) > 30 || m.slice(1, 5).some((o) => Number(o) > 255)) return undefined;
  const size = 2 ** (32 - Number(m[5]));
  const addr = m.slice(1, 5).reduce((a, o) => a * 256 + Number(o), 0);
  const start = addr - (addr % size);
  const taken = new Set(used.map((p) => p.split("/")[0]));
  for (let a = start; a < start + size; a += 4) {
    const dotted = [24, 16, 8, 0].map((sh) => Math.floor(a / 2 ** sh) % 256).join(".");
    if (!taken.has(dotted)) return `${dotted}/30`;
  }
  return undefined;
}

/**
 * Cluster-wide Anycast catalog (`routing.anycast`, D18): one pool, one /30 per service, a node-side health probe.
 * Which nodes run a service is each node's `routing.anycast`; this card edits both together.
 */
function AnycastEditor(props: { doc: Doc; onChange: (d: Doc) => void; editable: boolean; problems: Problem[] }) {
  const doc = props.doc;
  const cat = (doc.routing?.anycast as AnycastCatalog | undefined) ?? { pool: "" };
  const services = cat.services ?? [];
  // (documents written before D24 kept the selection in services.frr.anycastServices)
  const selection = (n: string): string[] => doc.nodes[n]!.routing?.anycast ?? doc.nodes[n]!.services?.frr?.anycastServices ?? [];
  // Anycast runs over the Tinc mesh (OSPF): roaming nodes and nodes without Tinc cannot host a service. A node that
  // selects one anyway stays listed so the selection can be removed (the manager reports it as a problem).
  const eligible = (n: string) => doc.nodes[n]!.behavior !== "roaming" && !!doc.nodes[n]!.services?.tinc;
  const frrNodes = Object.keys(doc.nodes).sort().filter((n) => eligible(n) || selection(n).length);
  const hosts = (id: string) => frrNodes.filter((n) => eligible(n) || selection(n).includes(id));

  // Writes the catalog and (when given) the nodes' selections in one change; an empty catalog disappears.
  const commit = (next: AnycastCatalog, nodes: Doc["nodes"] = doc.nodes) => {
    const { anycast: _old, ...routing } = doc.routing ?? {};
    const empty = !next.pool && !next.area && !(next.services ?? []).length;
    const r = empty ? routing : { ...routing, anycast: next };
    props.onChange({ ...doc, routing: Object.keys(r).length ? r : undefined, nodes });
  };
  const select = (nodes: Doc["nodes"], n: string, ids: string[]): Doc["nodes"] => {
    const spec = { ...nodes[n]! };
    const { anycast: _old, ...routing } = spec.routing ?? {};
    spec.routing = ids.length ? { ...routing, anycast: ids } : routing;
    if (!Object.keys(spec.routing).length) delete spec.routing;
    if (spec.services?.frr?.anycastServices) {
      const { anycastServices: _legacy, ...frr } = spec.services.frr;
      spec.services = { ...spec.services, frr };
    }
    return { ...nodes, [n]: spec };
  };
  const setService = (i: number, s: AnycastService) => {
    const old = services[i]!.id;
    let nodes = doc.nodes;
    if (s.id !== old) for (const n of frrNodes) if (selection(n).includes(old)) nodes = select(nodes, n, selection(n).map((x) => (x === old ? s.id : x)));
    commit({ ...cat, services: services.map((x, j) => (j === i ? s : x)) }, nodes);
  };
  const remove = async (i: number) => {
    const id = services[i]!.id;
    const users = frrNodes.filter((n) => selection(n).includes(id));
    if (users.length && !(await ask(t("Remove {id}? It is also removed from {nodes}; their instances stop announcing it after publishing.", { id, nodes: users.join(", ") }), { ok: t("Remove"), danger: true }))) return;
    let nodes = doc.nodes;
    for (const n of users) nodes = select(nodes, n, selection(n).filter((x) => x !== id));
    commit({ ...cat, services: services.filter((_, j) => j !== i) }, nodes);
  };
  const add = () => {
    let k = services.length + 1;
    while (services.some((s) => s.id === `service-${k}`)) k++;
    const prefix = nextFree30(cat.pool, services.map((s) => s.prefix));
    if (cat.pool && !prefix) toast(t("No free /30 left in {pool}", { pool: cat.pool }), "bad");
    commit({ ...cat, services: [...services, { id: `service-${k}`, prefix: prefix ?? "", health: { kind: "tcp", port: 80 } }] });
  };
  const rel = (p: Problem) => p.path.replace("/routing/anycast", "") || "/";

  return (
    // minWidth 0: a fieldset defaults to min-content width, which would widen the page instead of scrolling the table
    <fieldset disabled={!props.editable} style={{ border: 0, padding: 0, minWidth: 0 }}>
      <p class="small muted">
        {t("OSPF over the Tinc mesh carries only these prefixes. Every FRR node on the mesh routes the pool to the nearest healthy instance; a service's two /30 addresses are on its node only while the health probe passes, so the service must listen on a wildcard address.")}
      </p>
      <div class="form-grid">
        <Field label={t("Pool")} help={t("The cluster's Anycast range, e.g. 10.255.0.0/24; one /30 per service.")}>
          <TextInput mono value={cat.pool} placeholder="10.255.0.0/24" onInput={(v) => commit({ ...cat, pool: v.trim() })} />
        </Field>
        <Field label={t("OSPF area")} help={t("Default 0.0.0.0.")}>
          <TextInput mono value={cat.area} placeholder="0.0.0.0" onInput={(v) => commit({ ...cat, area: v.trim() || undefined })} />
        </Field>
      </div>
      {services.length > 0 && (
        <div class="table-wrap">
          <table class="table">
            <thead>
              <tr>
                <th>ID</th><th>{t("Description")}</th><th>{t("Prefix (/30)")}</th><th>{t("Health probe")}</th><th>{t("Metric")}</th><th>{t("Runs on")}</th><th />
              </tr>
            </thead>
            <tbody>
              {services.map((s, i) => {
                const h = s.health ?? { kind: "tcp", port: 80 };
                const setHealth = (patch: Partial<AnycastService["health"]>) => setService(i, { ...s, health: { ...h, ...patch } });
                const rowProblems = props.problems.filter((p) => p.path.startsWith(`/routing/anycast/services/${i}/`) || p.path === `/routing/anycast/services/${i}`);
                return (
                  <tr>
                    <td style={{ minWidth: "110px" }}><TextInput mono value={s.id} invalid={rowProblems.some((p) => p.path.endsWith("/id"))} onInput={(v) => setService(i, { ...s, id: v.trim() })} /></td>
                    <td style={{ minWidth: "100px" }}><TextInput value={s.description} placeholder={s.id} onInput={(v) => setService(i, { ...s, description: v || undefined })} /></td>
                    <td style={{ minWidth: "150px" }}>
                      <TextInput mono value={s.prefix} invalid={rowProblems.some((p) => p.path.endsWith("/prefix"))} placeholder="10.255.0.4/30" onInput={(v) => setService(i, { ...s, prefix: v.trim() })} />
                      {rowProblems.map((p) => <div class="small" style={{ color: "var(--bad)" }}>{p.message}</div>)}
                    </td>
                    <td style={{ minWidth: "180px" }}>
                      <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: "4px" }}>
                        <Select value={h.kind} options={["tcp", "dns", "http"]} onChange={(v) => setHealth({ kind: v as AnycastService["health"]["kind"] })} />
                        <input type="number" placeholder={t("port")} title={t("port")} value={h.port ?? ""} onInput={(e) => setHealth({ port: Number((e.target as HTMLInputElement).value) })} />
                        <TextInput mono value={h.host} placeholder="127.0.0.1" onInput={(v) => setHealth({ host: v.trim() || undefined })} />
                        {h.kind === "dns" && <TextInput mono value={h.name} placeholder={t("name to resolve")} onInput={(v) => setHealth({ name: v.trim() || undefined })} />}
                        {h.kind === "http" && <TextInput mono value={h.path} placeholder="/" onInput={(v) => setHealth({ path: v.trim() || undefined })} />}
                      </div>
                    </td>
                    <td>
                      <input type="number" style={{ width: "70px" }} title={t("OSPF cost from each node (lower attracts more traffic)")} value={s.metric ?? ""}
                        onInput={(e) => {
                          const v = (e.target as HTMLInputElement).value;
                          setService(i, { ...s, metric: v ? Number(v) : undefined });
                        }} />
                    </td>
                    <td>
                      {hosts(s.id).length ? (
                        <div style={{ whiteSpace: "nowrap" }}>
                          {hosts(s.id).map((n) => (
                            <div>
                              <Checkbox label={n} checked={selection(n).includes(s.id)} onChange={(on) =>
                                commit(cat, select(doc.nodes, n, on ? [...selection(n).filter((x) => x !== s.id), s.id] : selection(n).filter((x) => x !== s.id)))} />
                            </div>
                          ))}
                        </div>
                      ) : (
                        <span class="small muted">{t("no FRR node on the Tinc mesh")}</span>
                      )}
                    </td>
                    <td>{props.editable && <Button small variant="danger" onClick={() => void remove(i)}>{t("Remove")}</Button>}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      {props.problems.filter((p) => !/^\/routing\/anycast\/services\/\d+/.test(p.path)).map((p) => <div class="small" style={{ color: "var(--bad)" }}>{rel(p)}: {p.message}</div>)}
      {props.editable && <Button small onClick={add}>{t("Add service")}</Button>}
    </fieldset>
  );
}

/**
 * Cluster-wide Mihomo subscriptions (D29): one list, one default, one refresh interval. Every node running Mihomo uses
 * the default unless it chooses another one in its settings. The URL is stored as a secret shared with the nodes that
 * use it (only the chosen subscription reaches a node).
 */
function MihomoSubscriptions(props: { doc: Doc; onChange: (d: Doc) => void; editable: boolean; problems: Problem[] }) {
  const cat: MihomoCatalog = props.doc.mihomo ?? {};
  const list = cat.subscriptions ?? [];
  const [adding, setAdding] = useState<{ id: string; name: string; url: string } | null>(null);
  const [replacing, setReplacing] = useState<MihomoSubscription | null>(null);
  const [url, setUrl] = useState("");
  const [busy, setBusy] = useState(false);
  const users = (id: string) =>
    Object.entries(props.doc.nodes)
      .filter(([, n]) => ["mihomo", "mihomo-openwrt"].some((k) => n.services?.[k] && n.services[k].enabled !== false && (n.services[k].subscription ?? cat.default) === id))
      .map(([n]) => n);
  const commit = (next: MihomoCatalog) => {
    const empty = !(next.subscriptions ?? []).length && next.refreshSeconds === undefined;
    const { mihomo: _old, ...rest } = props.doc;
    props.onChange(empty ? rest : { ...props.doc, mihomo: next });
  };
  const setSub = (i: number, s: MihomoSubscription) => commit({ ...cat, subscriptions: list.map((x, j) => (j === i ? s : x)) });
  const storeUrl = (id: string, value: string) => post("/api/v1/secrets", { id, kind: "text", value, nodeScope: ["*"], description: "Mihomo subscription URL" });
  const refresh = async (subscription?: string) => {
    try {
      const r = await post<{ jobs: Array<{ node: string }> }>("/api/v1/mihomo/refresh", subscription ? { subscription } : {});
      toast(r.jobs.length ? t("Refresh queued on {nodes}", { nodes: r.jobs.map((j) => j.node).join(", ") }) : t("No node uses it (in the applied configuration)"), "ok");
    } catch (e) {
      toast(errorText(e), "bad");
    }
  };
  const rowProblem = (i: number) => props.problems.find((p) => p.path.startsWith(`/mihomo/subscriptions/${i}`))?.message;
  return (
    <fieldset disabled={!props.editable} style={{ border: 0, padding: 0, margin: 0, minWidth: 0 }}>
      <p class="small muted">{t("Every node running Mihomo uses the default subscription unless its own settings choose another one from this list.")}</p>
      <div class="form-grid">
        <Field label={t("Automatic refresh (seconds)")} help={t("How often nodes download their subscription again; default 43200 (12 h).")} error={props.problems.find((p) => p.path === "/mihomo/refreshSeconds")?.message}>
          <input type="number" min={60} value={cat.refreshSeconds ?? ""} placeholder="43200" onInput={(e) => {
            const v = (e.target as HTMLInputElement).value;
            commit({ ...cat, refreshSeconds: v ? Number(v) : undefined });
          }} />
        </Field>
      </div>
      {list.length > 0 ? (
        <div class="table-wrap">
          <table class="table">
            <thead>
              <tr><th>{t("Default")}</th><th>{t("ID")}</th><th>{t("Name")}</th><th>{t("Enabled")}</th><th>{t("Used by")}</th><th /></tr>
            </thead>
            <tbody>
              {list.map((s, i) => (
                <tr>
                  <td><input type="radio" name="mihomo-default" checked={cat.default === s.id} onChange={() => commit({ ...cat, default: s.id })} /></td>
                  <td class="mono">{s.id}{rowProblem(i) && <div class="small" style={{ color: "var(--bad)" }}>{rowProblem(i)}</div>}</td>
                  <td><TextInput value={s.name} onInput={(v) => setSub(i, { ...s, name: v || undefined })} /></td>
                  <td><Checkbox checked={s.enabled !== false} onChange={(v) => setSub(i, { ...s, enabled: v ? undefined : false })} /></td>
                  <td class="small">{users(s.id).join(", ") || "—"}</td>
                  <td class="actions">
                    <Button small onClick={() => void refresh(s.id)}>{t("Refresh now")}</Button>{" "}
                    <Button small onClick={() => { setReplacing(s); setUrl(""); }}>{t("Replace URL")}</Button>{" "}
                    <Button small variant="danger" onClick={async () => {
                      const u = users(s.id);
                      if (u.length && !(await ask(t("{id} is used by {nodes}. Remove it anyway? They fall back to the default.", { id: s.id, nodes: u.join(", ") }), { ok: t("Delete"), danger: true }))) return;
                      const rest = list.filter((_, j) => j !== i);
                      commit({ ...cat, subscriptions: rest, default: cat.default === s.id ? rest[0]?.id : cat.default });
                    }}>{t("Delete")}</Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : (
        <div class="empty">{t("No subscriptions yet.")}</div>
      )}
      {props.problems.filter((p) => p.path === "/mihomo/default").map((p) => <div class="small" style={{ color: "var(--bad)" }}>{p.message}</div>)}
      <div class="row" style={{ marginTop: "10px" }}>
        <Button small variant="primary" onClick={() => setAdding({ id: `sub-${list.length + 1}`, name: "", url: "" })}>{t("Add subscription")}</Button>
        {list.length > 0 && <Button small onClick={() => void refresh()}>{t("Refresh all nodes now")}</Button>}
      </div>
      {adding && (
        <Modal title={t("Add subscription")} onClose={() => setAdding(null)} actions={
          <Button variant="primary" busy={busy} disabled={!adding.id || !adding.url} onClick={async () => {
            setBusy(true);
            try {
              const urlRef = `mihomo-sub-${adding.id}`;
              await storeUrl(urlRef, adding.url);
              const next = [...list, { id: adding.id, ...(adding.name ? { name: adding.name } : {}), urlRef }];
              commit({ ...cat, subscriptions: next, default: cat.default ?? adding.id });
              setAdding(null);
            } catch (e) {
              toast(errorText(e), "bad");
            } finally {
              setBusy(false);
            }
          }}>{t("Add")}</Button>
        }>
          <Field label={t("ID")}><TextInput mono value={adding.id} onInput={(v) => setAdding({ ...adding, id: v.toLowerCase().replace(/[^a-z0-9_-]/g, "-") })} /></Field>
          <Field label={t("Name")}><TextInput value={adding.name} onInput={(v) => setAdding({ ...adding, name: v })} /></Field>
          <Field label={t("Subscription URL")} help={t("Stored encrypted; delivered only to the nodes that use this subscription.")}>
            <input type="password" value={adding.url} autoComplete="off" onInput={(e) => setAdding({ ...adding, url: (e.target as HTMLInputElement).value })} />
          </Field>
        </Modal>
      )}
      {replacing && (
        <Modal title={t("Replace URL of {id}", { id: replacing.id })} onClose={() => setReplacing(null)} actions={
          <Button variant="primary" busy={busy} disabled={!url} onClick={async () => {
            setBusy(true);
            try {
              await storeUrl(replacing.urlRef, url);
              toast(t("Stored. Nodes receive the new URL with the next applied revision."), "ok");
              setReplacing(null);
            } catch (e) {
              toast(errorText(e), "bad");
            } finally {
              setBusy(false);
            }
          }}>{t("Save")}</Button>
        }>
          <Field label={t("Subscription URL")}>
            <input type="password" value={url} autoComplete="off" onInput={(e) => setUrl((e.target as HTMLInputElement).value)} />
          </Field>
        </Modal>
      )}
    </fieldset>
  );
}

// ---- DNS hosts, Tinc mesh, MosDNS (D58) -------------------------------------------------------------------

type Host = { hostname: string; ips: string[] };

/** Static host entries every node's resolver answers; a node's own entry for the same name wins. */
function DnsHosts(props: { doc: Doc; onChange: (d: Doc) => void; editable: boolean }) {
  const hosts: Host[] = props.doc.dns?.hosts ?? [];
  const commit = (next: Host[]) => {
    const { dns: _old, ...rest } = props.doc;
    props.onChange(next.length ? { ...props.doc, dns: { ...props.doc.dns, hosts: next } } : rest);
  };
  const set = (i: number, h: Host) => commit(hosts.map((x, j) => (j === i ? h : x)));
  return (
    <fieldset disabled={!props.editable} style={{ border: 0, padding: 0, margin: 0, minWidth: 0 }}>
      <p class="small muted">{t("Every node answers these names; a node's own entry for the same name replaces the cluster's.")}</p>
      {hosts.length > 0 ? (
        <div class="table-wrap">
          <table class="table">
            <thead>
              <tr><th>{t("Hostname")}</th><th>{t("Addresses (one per line)")}</th><th /></tr>
            </thead>
            <tbody>
              {hosts.map((h, i) => (
                <tr>
                  <td><TextInput mono value={h.hostname} onInput={(v) => set(i, { ...h, hostname: v.trim() })} /></td>
                  <td><ListInput value={h.ips} onChange={(ips) => set(i, { ...h, ips })} /></td>
                  <td class="actions"><Button small variant="danger" onClick={() => commit(hosts.filter((_, j) => j !== i))}>{t("Delete")}</Button></td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : (
        <div class="empty">{t("No host entries.")}</div>
      )}
      <div class="row" style={{ marginTop: "10px" }}>
        <Button small variant="primary" onClick={() => commit([...hosts, { hostname: "", ips: [] }])}>{t("Add host")}</Button>
      </div>
    </fieldset>
  );
}

const TINC_CIPHERS = ["aes-256-cbc", "aes-128-cbc", "aes-256-gcm", "chacha20-poly1305", "blowfish", "none"];
const TINC_DIGESTS = ["sha256", "sha512", "sha1", "none"];

/** Mesh-wide Tinc cipher and digest: every member must speak the same ones. */
function TincMesh(props: { doc: Doc; onChange: (d: Doc) => void; editable: boolean }) {
  const tinc = props.doc.tinc ?? {};
  const set = (k: "cipher" | "digest", v: string) => {
    const { [k]: _old, ...rest } = tinc;
    const next = v ? { ...rest, [k]: v } : rest;
    const { tinc: _t, ...doc } = props.doc;
    props.onChange(Object.keys(next).length ? { ...props.doc, tinc: next } : doc);
  };
  return (
    <fieldset disabled={!props.editable} style={{ border: 0, padding: 0, margin: 0, minWidth: 0 }}>
      <p class="small muted">{t("Used by every mesh member that does not set its own; members with different settings cannot talk to each other.")}</p>
      <div class="form-grid">
        <Field label={t("Cipher")}><Select value={tinc.cipher ?? ""} allowEmpty options={TINC_CIPHERS} onChange={(v) => set("cipher", v)} /></Field>
        <Field label={t("Digest")}><Select value={tinc.digest ?? ""} allowEmpty options={TINC_DIGESTS} onChange={(v) => set("digest", v)} /></Field>
      </div>
    </fieldset>
  );
}

interface RuleFile {
  id: string;
  fileName?: string;
  url: string;
  refreshHours?: number;
  enabled?: boolean;
  category?: string;
}

const DOMAIN_SETS = [
  ["local", msg("Resolved by the local upstreams")],
  ["block", msg("Blocked")],
  ["ddns", msg("Dynamic DNS (never cached)")],
  ["global", msg("Resolved by the remote upstreams")],
] as const;

/** The MosDNS configuration every node that enables MosDNS shares; a node's own fields win. */
function MosdnsShared(props: { doc: Doc; onChange: (d: Doc) => void; editable: boolean; problems: Problem[] }) {
  const m = (props.doc.mosdns ?? {}) as Record<string, any>;
  const set = (k: string, v: unknown) => {
    const { [k]: _old, ...rest } = m;
    const empty = v === undefined || v === "" || (Array.isArray(v) && !v.length);
    const next = empty ? rest : { ...rest, [k]: v };
    const { mosdns: _m, ...doc } = props.doc;
    props.onChange(Object.keys(next).length ? { ...props.doc, mosdns: next } : doc);
  };
  const domains = (m.domains ?? {}) as Record<string, string[]>;
  const setDomains = (k: string, v: string[]) => {
    const { [k]: _old, ...rest } = domains;
    const next = v.length ? { ...rest, [k]: v } : rest;
    set("domains", Object.keys(next).length ? next : undefined);
  };
  const rules = (m.ruleFiles ?? []) as RuleFile[];
  const setRule = (i: number, r: RuleFile) => set("ruleFiles", rules.map((x, j) => (j === i ? r : x)));
  const users = Object.entries(props.doc.nodes).filter(([, n]) => n.services?.mosdns && n.services.mosdns.enabled !== false).map(([id]) => id);
  const num = (e: Event) => {
    const v = (e.target as HTMLInputElement).value;
    return v ? Number(v) : undefined;
  };
  return (
    <fieldset disabled={!props.editable} style={{ border: 0, padding: 0, margin: 0, minWidth: 0 }}>
      <p class="small muted">{t("Shared by every node that enables MosDNS ({nodes}); a node's own MosDNS fields replace these.", { nodes: users.join(", ") || "—" })}</p>
      <div class="form-grid">
        <Field label={t("Listen address")}><TextInput mono value={m.listen ?? ""} placeholder="0.0.0.0" onInput={(v) => set("listen", v.trim() || undefined)} /></Field>
        <Field label={t("Port")}><input type="number" value={m.port ?? ""} placeholder="53" onInput={(e) => set("port", num(e))} /></Field>
        <Field label={t("Local upstreams")} help={t("Not used with a custom plugin chain.")}><ListInput value={m.localUpstreams} onChange={(v) => set("localUpstreams", v)} /></Field>
        <Field label={t("Remote upstreams")} help={t("Not used with a custom plugin chain.")}><ListInput value={m.remoteUpstreams} onChange={(v) => set("remoteUpstreams", v)} /></Field>
      </div>
      <h3>{t("Domain lists")}</h3>
      <p class="small muted">{t("One rule per line (domain:, full:, keyword:, regexp:). A custom plugin chain reads them as etcd_local.txt, etcd_block.txt, etcd_ddns.txt and etcd_global.txt.")}</p>
      <div class="form-grid">
        {DOMAIN_SETS.map(([k, label]) => (
          <Field label={t(label)}><ListInput value={domains[k]} onChange={(v) => setDomains(k, v)} /></Field>
        ))}
      </div>
      <h3>{t("Rule files")}</h3>
      {rules.length > 0 ? (
        <div class="table-wrap">
          <table class="table">
            <thead>
              <tr><th>{t("ID")}</th><th>{t("File name")}</th><th>{t("URL")}</th><th>{t("Refresh (hours)")}</th><th>{t("Enabled")}</th><th /></tr>
            </thead>
            <tbody>
              {rules.map((r, i) => (
                <tr>
                  <td class="mono">{r.id}</td>
                  <td><TextInput mono value={r.fileName} placeholder={`${r.id}.txt`} onInput={(v) => setRule(i, { ...r, fileName: v.trim() || undefined })} /></td>
                  <td><TextInput mono value={r.url} onInput={(v) => setRule(i, { ...r, url: v.trim() })} /></td>
                  <td><input type="number" min={1} value={r.refreshHours ?? ""} placeholder="24" onInput={(e) => setRule(i, { ...r, refreshHours: num(e) })} /></td>
                  <td><Checkbox checked={r.enabled !== false} onChange={(v) => setRule(i, { ...r, enabled: v ? undefined : false })} /></td>
                  <td class="actions"><Button small variant="danger" onClick={() => set("ruleFiles", rules.filter((_, j) => j !== i))}>{t("Delete")}</Button></td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : (
        <div class="empty">{t("No rule files.")}</div>
      )}
      <div class="row" style={{ marginTop: "10px" }}>
        <Button small variant="primary" onClick={async () => {
          const id = (await askText(t("Rule file ID (lowercase letters, digits, - and _)"), { title: t("Add rule file"), ok: t("Add"), mono: true }))?.toLowerCase();
          if (id) set("ruleFiles", [...rules, { id, url: "" }]);
        }}>{t("Add rule file")}</Button>
      </div>
      <h3>{t("Custom plugin chain")}</h3>
      <Field wide label={t("plugins (YAML)")} help={t("Optional. Replaces the generated plugin chain; references to /etc/mosdns/<file> are resolved to the domain lists and rule files above.")}>
        <textarea class="mono" rows={10} spellcheck={false} value={m.pluginsYaml ?? ""} onInput={(e) => set("pluginsYaml", (e.target as HTMLTextAreaElement).value || undefined)} />
      </Field>
      <ProblemList problems={props.problems} tone="bad" />
    </fieldset>
  );
}
