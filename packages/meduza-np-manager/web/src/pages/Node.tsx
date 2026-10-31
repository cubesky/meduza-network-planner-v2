// Node page: status (services with their restart buttons), routing (BGP/BFD/OSPF/Anycast), connections (Tinc,
// WireGuard, OpenVPN peers), settings (edited directly, applied through the apply bar) and diagnostics.

import { useState } from "preact/hooks";
import type { ComponentChildren } from "preact";
import { Badge, Button, Card, CopyBlock, Loadable, Modal, PageHead, StateBadge, StatusDot, Table, Tabs, ask, bytes, getLocale, relativeTime, stateTone, t, toast, useAsync } from "@meduza-np/ui-kit";
import { errorText, get, post } from "../api.ts";
import { isError, problemsFor, useConfig } from "../store.ts";
import { revisionBadge, syncCell, type NodeSummary } from "./Overview.tsx";
import { NodeSettings } from "./NodeSettings.tsx";

type Role = "admin" | "operator" | "viewer";

export interface NodeDetail extends NodeSummary {
  enrolledAt: string | null;
  revokedAt: string | null;
  hostname: string | null;
  plannerVersion: string | null;
  report: null | Record<string, any>;
  packages: Array<{ package: string; role: string; catalogVersion: string; installed: boolean; installedVersion: string | null; registration: string | null; reason: string | null; uiCompatible: boolean | null }>;
  jobs: Array<Record<string, unknown>>;
}

type Inst = { id: string; kind: string; state: string; reason?: string; observedAt?: string; [k: string]: unknown };

const ROUTING = new Set(["bgp-neighbor", "bfd-peer", "ospf-neighbor", "anycast"]);
const CONNECTIONS = new Set(["tinc-peer", "wireguard-peer", "openvpn-client", "openvpn-site2site", "openvpn-access"]);
const last = (id: string) => id.split("/").pop() ?? id;

/** "3h 26m" style duration for seconds. */
function duration(sec: unknown): string {
  const s = Number(sec);
  if (!Number.isFinite(s) || s < 0) return String(sec ?? "—");
  const d = Math.floor(s / 86400), h = Math.floor((s % 86400) / 3600), m = Math.floor((s % 3600) / 60);
  return d ? `${d}d ${h}h` : h ? `${h}h ${m}m` : m ? `${m}m` : `${Math.floor(s)}s`;
}

export function NodePage(props: { id: string; tab?: string; role: Role }) {
  const state = useAsync(() => get<NodeDetail>(`/api/v1/nodes/${encodeURIComponent(props.id)}`), [props.id], 10_000);
  const cfg = useConfig();
  const settingErrors = problemsFor(cfg.preview, props.id).filter(isError).length;
  const tab = props.tab || "status";
  const go = (t: string) => (location.hash = `/nodes/${encodeURIComponent(props.id)}${t === "status" ? "" : `/${t}`}`);

  const job = async (kind: string, extra: Record<string, unknown> = {}, done?: string) => {
    try {
      await post(`/api/v1/nodes/${encodeURIComponent(props.id)}/jobs`, { kind, ...extra });
      toast(done ?? t("Queued; the node runs it on its next sync."), "ok");
      state.reload();
    } catch (e) {
      toast(errorText(e), "bad");
    }
  };
  const canAct = props.role !== "viewer";

  return (
    <div>
      <Loadable state={state}>
        {(n) => {
          const instances = ((n.report?.instances ?? []) as Inst[]).filter((i) => i && i.id);
          const routing = instances.filter((i) => ROUTING.has(i.kind));
          const connections = instances.filter((i) => CONNECTIONS.has(i.kind));
          const services = instances.filter((i) => !ROUTING.has(i.kind) && !CONNECTIONS.has(i.kind));
          const restart = (instance: string, process?: string, label?: string) =>
            void job("restart-instance", { instance, ...(process ? { params: { process } } : {}) }, t("Restart of {name} queued", { name: label ?? instance }));
          return (
            <>
              <PageHead
                crumbs={<a href="#/nodes">{t("Nodes")}</a>}
                title={<span class="row" style={{ gap: "10px" }}><StatusDot state={n.freshness} />{n.displayName || n.id}</span>}
                sub={[
                  n.id,
                  t(n.behavior),
                  n.arch,
                  n.plannerVersion && `planner ${n.plannerVersion}`,
                  n.report?.system?.networkMode === "bridge" && t("container, bridge network"),
                  n.report?.system?.networkMode === "host" && t("container, host network"),
                ].filter(Boolean).join(" · ")}
                actions={canAct && <Button onClick={() => void job("sync-now", {}, t("Sync requested"))}>{t("Sync now")}</Button>}
              />
              <Tabs
                tabs={[
                  { id: "status", label: t("Status") },
                  { id: "routing", label: t("Routing"), count: routing.length || undefined },
                  { id: "connections", label: t("Connections"), count: connections.length || undefined },
                  { id: "settings", label: t("Settings"), count: settingErrors || undefined, tone: settingErrors ? "bad" : undefined },
                  { id: "diagnostics", label: t("Diagnostics") },
                ]}
                active={tab}
                onChange={go}
              />
              {tab === "status" && <StatusTab n={n} services={services} canAct={canAct} restart={restart} />}
              {tab === "routing" && <RoutingTab items={routing} canAct={canAct} restart={restart} />}
              {tab === "connections" && <ConnectionsTab items={connections} canAct={canAct} restart={restart} />}
              {tab === "settings" && <NodeSettings id={props.id} detail={n} role={props.role} />}
              {tab === "diagnostics" && <DiagnosticsTab n={n} role={props.role} job={job} reload={state.reload} />}
            </>
          );
        }}
      </Loadable>
    </div>
  );
}

// ---- status ----------------------------------------------------------------------------------------------

function StatusTab(props: { n: NodeDetail; services: Inst[]; canAct: boolean; restart: (instance: string, process?: string, label?: string) => void }) {
  const n = props.n;
  const features = ((n.report?.features ?? []) as Array<Record<string, any>>).filter((f) => f.enabled);
  return (
    <>
      <div class="stats">
        <div class="stat"><div class="value small"><StateBadge state={n.freshness} /></div><div class="label">{t("last seen {time}", { time: relativeTime(n.lastSeenAt) })}</div></div>
        <div class="stat"><div class="value small">{syncCell(n)}</div><div class="label">{t("sync")}</div></div>
        <div class="stat"><div class="value small">{revisionBadge(n)}</div><div class="label mono">{n.appliedRevision ?? "—"}</div></div>
        <div class="stat">
          <div class="value small">{n.resources?.memAvailableKib ? t("{size} free", { size: bytes(n.resources.memAvailableKib * 1024) }) : "—"}</div>
          <div class="label">{n.resources?.plannerRssKib ? t("planner RSS {size}", { size: bytes(n.resources.plannerRssKib * 1024) }) : t("memory")}</div>
        </div>
      </div>
      {n.failedRevision && n.failedRevision === n.desiredRevision && n.lastError && <div class="callout bad"><strong>{t("Last apply failed")}</strong> — {n.lastError}</div>}
      {n.maintenance && <div class="callout warn">{t("Maintenance: {reason} until {until}", { reason: n.maintenance.reason, until: n.maintenance.until })}</div>}
      {!n.report && <div class="callout info">{t("The node has not reported yet")}</div>}
      {features.length > 0 && (
        <Card title={t("Features")}>
          <div class="row">
            {features.map((f) => (
              <Badge
                tone={!f.available || !f.registered ? "bad" : f.limitationCode ? "warn" : f.healthy ? "ok" : f.running ? "info" : "warn"}
                title={f.reason ?? (f.limitationCode === "bridge-network" ? t("Acts only inside the container's bridge network: host/LAN traffic is affected only when routed into the container") : "")}
              >
                {f.id}
                {f.limitationCode === "bridge-network" && ` · ${t("container network only")}`}
              </Badge>
            ))}
          </div>
        </Card>
      )}
      <Card title={t("Services")}>
        <Table
          rows={props.services as unknown as Record<string, unknown>[]}
          rowKey={(r) => String(r.id)}
          empty={n.report ? t("No services reported") : t("The node has not reported yet")}
          columns={[
            {
              key: "id",
              label: t("Service"),
              render: (r) => {
                const i = r as Inst;
                const parts = i.id.split("/");
                const name = i.kind === "process" ? parts[parts.length - 2] : parts.slice(1).join("/") || i.id;
                return (
                  <span class="row" style={{ gap: "8px", flexWrap: "nowrap" }}>
                    <StatusDot state={i.state} />
                    <span><strong>{name}</strong> <span class="faint small">{parts[0]}{i.kind !== "process" ? ` · ${i.kind}` : ""}</span></span>
                  </span>
                );
              },
            },
            { key: "state", label: t("State"), render: (r) => <StateBadge state={String(r.state)} /> },
            {
              key: "restarts",
              label: t("Restarts"),
              render: (r) => (r.kind === "process" ? String(r.restarts ?? 0) : <span class="faint">—</span>),
            },
            {
              key: "actions",
              label: "",
              render: (r) => {
                const i = r as Inst;
                const parts = i.id.split("/");
                // a process restarts alone through its instance; a feature instance (pkg/inst) restarts as a whole
                const target = i.kind === "process" ? (i.instance as string | undefined) : parts.length >= 2 ? `${parts[0]}/${parts[1]}` : undefined;
                const proc = i.kind === "process" ? parts.slice(0, -1).join("/") : undefined;
                return (
                  <span class="row" style={{ justifyContent: "flex-end", flexWrap: "nowrap" }}>
                    <DetailsButton item={i} />
                    {props.canAct && target && (
                      <Button small onClick={async () => (await ask(t("Restart {name}?", { name: proc ?? target }), { ok: t("Restart") })) && props.restart(target, proc, proc ?? target)}>{t("Restart")}</Button>
                    )}
                  </span>
                );
              },
            },
          ]}
        />
      </Card>
    </>
  );
}

/** Reason and details of a status row, offered only when something is off (a reason, or not up). */
function DetailsButton(props: { item: Inst }) {
  const [open, setOpen] = useState(false);
  const { id, kind, state, reason, observedAt, ...detail } = props.item;
  const tone = stateTone(state);
  if (!reason && (tone === "ok" || tone === "info" || tone === "idle")) return null;
  return (
    <>
      <Button small variant={tone === "bad" ? "danger" : undefined} onClick={() => setOpen(true)}>{t("Details")}</Button>
      {open && (
        <Modal title={id} onClose={() => setOpen(false)}>
          <dl class="kv">
            <dt>{t("State")}</dt><dd><StateBadge state={state} /></dd>
            <dt>{t("Kind")}</dt><dd>{kind}</dd>
            {reason && <><dt>{t("Reason")}</dt><dd>{reason}</dd></>}
            {observedAt && <><dt>{t("Observed")}</dt><dd>{new Date(observedAt).toLocaleString(getLocale())}</dd></>}
            {Object.entries(detail).map(([k, v]) => <><dt>{k}</dt><dd class="mono small">{typeof v === "object" ? JSON.stringify(v) : String(v)}</dd></>)}
          </dl>
        </Modal>
      )}
    </>
  );
}

// ---- routing / connections -------------------------------------------------------------------------------

function Section(props: { title: string; actions?: ComponentChildren; children: ComponentChildren }) {
  return (
    <Card title={props.title} actions={props.actions}>
      {props.children}
    </Card>
  );
}

const stateCell = (r: Record<string, unknown>, label?: unknown) => (
  <span class="row" style={{ gap: "6px", flexWrap: "nowrap" }}>
    <StatusDot state={String(r.state)} />
    {label !== undefined ? String(label) : t(String(r.state))}
  </span>
);

function RoutingTab(props: { items: Inst[]; canAct: boolean; restart: (instance: string, process?: string, label?: string) => void }) {
  const of = (k: string) => props.items.filter((i) => i.kind === k) as unknown as Record<string, unknown>[];
  const bgp = of("bgp-neighbor"), bfd = of("bfd-peer"), ospf = of("ospf-neighbor"), anycast = of("anycast");
  if (!props.items.length) return <div class="empty">{t("This node reports no routing sessions.")}</div>;
  const restartFrr = props.canAct && (
    <Button small onClick={async () => (await ask(t("Restarting drops every BGP/BFD/OSPF session. Restart FRR?"), { ok: t("Restart FRR"), danger: true })) && props.restart("frr/main", undefined, "FRR")}>{t("Restart FRR")}</Button>
  );
  return (
    <>
      {bgp.length > 0 && (
        <Section title={t("BGP neighbors")} actions={restartFrr}>
          <Table
            rows={bgp}
            rowKey={(r) => String(r.id)}
            columns={[
              { key: "id", label: t("Neighbor"), render: (r) => <strong>{last(String(r.id)).replace(/^bgp-/, "")}</strong> },
              { key: "address", label: t("Address"), render: (r) => <span class="mono">{String(r.address ?? "—")}</span> },
              { key: "session", label: t("Session"), render: (r) => (r.session === "external" ? <Badge tone="info">eBGP</Badge> : <Badge tone="idle">iBGP</Badge>) },
              { key: "remoteAs", label: t("Remote AS") },
              { key: "bgpState", label: t("State"), render: (r) => stateCell(r, r.bgpState ?? r.state) },
              { key: "prefixes", label: t("Prefixes in / out"), render: (r) => `${r.prefixesReceived ?? "—"} / ${r.prefixesSent ?? "—"}` },
              { key: "uptime", label: t("Up for") },
              { key: "d", label: "", render: (r) => <DetailsButton item={r as Inst} /> },
            ]}
          />
        </Section>
      )}
      {bfd.length > 0 && (
        <Section title={t("BFD sessions")}>
          <Table
            rows={bfd}
            rowKey={(r) => String(r.id)}
            columns={[
              { key: "peer", label: t("Peer"), render: (r) => <span class="mono">{String(r.peer ?? last(String(r.id)))}</span> },
              { key: "local", label: t("Local"), render: (r) => <span class="mono">{String(r.local ?? "—")}</span> },
              { key: "status", label: t("State"), render: (r) => stateCell(r, r.status ?? r.state) },
              { key: "timers", label: t("Timers"), render: (r) => `${r.transmit_interval ?? "?"}/${r.receive_interval ?? "?"} ms × ${r.detect_multiplier ?? "?"}` },
              { key: "uptime", label: t("Up for"), render: (r) => duration(r.uptime) },
              { key: "d", label: "", render: (r) => <DetailsButton item={r as Inst} /> },
            ]}
          />
        </Section>
      )}
      {ospf.length > 0 && (
        <Section title={t("OSPF neighbors (Anycast)")}>
          <Table
            rows={ospf}
            rowKey={(r) => String(r.id)}
            columns={[
              { key: "routerId", label: t("Router ID"), render: (r) => <span class="mono">{String(r.routerId ?? "—")}</span> },
              { key: "interface", label: t("Interface"), render: (r) => <span class="mono">{String(r.interface ?? "—")}</span> },
              { key: "ospfState", label: t("State"), render: (r) => stateCell(r, r.ospfState ?? r.state) },
              { key: "d", label: "", render: (r) => <DetailsButton item={r as Inst} /> },
            ]}
          />
        </Section>
      )}
      {anycast.length > 0 && (
        <Section title={t("Anycast services")}>
          <Table
            rows={anycast}
            rowKey={(r) => String(r.id)}
            columns={[
              { key: "service", label: t("Service"), render: (r) => <strong>{String(r.service ?? last(String(r.id)))}</strong> },
              { key: "vips", label: t("Addresses"), render: (r) => <span class="mono">{String(r.vips ?? "—")}</span> },
              { key: "announced", label: t("Announced"), render: (r) => (r.announced ? <Badge tone="ok">{t("announced")}</Badge> : <Badge tone="warn">{t("withdrawn")}</Badge>) },
              { key: "guard", label: t("Health check"), render: (r) => <span class="small muted">{String(r.guard ?? "—")}</span> },
              { key: "d", label: "", render: (r) => <DetailsButton item={r as Inst} /> },
            ]}
          />
        </Section>
      )}
    </>
  );
}

function ConnectionsTab(props: { items: Inst[]; canAct: boolean; restart: (instance: string, process?: string, label?: string) => void }) {
  const of = (k: string) => props.items.filter((i) => i.kind === k) as unknown as Record<string, unknown>[];
  const tinc = of("tinc-peer"), wg = of("wireguard-peer");
  const ovpn = props.items.filter((i) => i.kind.startsWith("openvpn-")) as unknown as Record<string, unknown>[];
  if (!props.items.length) return <div class="empty">{t("This node reports no tunnel peers.")}</div>;
  const instanceOf = (id: string) => id.split("/").slice(0, 2).join("/");
  const restartBtn = (rows: Record<string, unknown>[], label: string) => {
    const insts = [...new Set(rows.map((r) => instanceOf(String(r.id))))];
    return props.canAct && insts.map((i) => (
      <Button small onClick={async () => (await ask(t("Restart {name}? Its connections drop briefly.", { name: i }), { ok: t("Restart") })) && props.restart(i, undefined, i)}>{t("Restart {name}", { name: insts.length > 1 ? i : label })}</Button>
    ));
  };
  return (
    <>
      {tinc.length > 0 && (
        <Section title={t("Tinc mesh peers")} actions={restartBtn(tinc, "Tinc")}>
          <Table
            rows={tinc}
            rowKey={(r) => String(r.id)}
            columns={[
              { key: "id", label: t("Peer"), render: (r) => <strong>{last(String(r.id))}</strong> },
              { key: "remoteAddress", label: t("Address"), render: (r) => <span class="mono">{String(r.remoteAddress ?? "—")}</span> },
              { key: "state", label: t("State"), render: (r) => stateCell(r, r.reasonCode ? t(String(r.reasonCode)) : undefined) },
              { key: "lastChange", label: t("Since"), render: (r) => relativeTime(r.lastChange as string | undefined) },
              { key: "d", label: "", render: (r) => <DetailsButton item={r as Inst} /> },
            ]}
          />
        </Section>
      )}
      {wg.length > 0 && (
        <Section title={t("WireGuard peers")} actions={restartBtn(wg, "WireGuard")}>
          <Table
            rows={wg}
            rowKey={(r) => String(r.id)}
            columns={[
              { key: "id", label: t("Peer"), render: (r) => <span><strong>{last(String(r.id))}</strong> <span class="faint small">{String(r.id).split("/")[1]}</span></span> },
              { key: "endpoint", label: t("Endpoint"), render: (r) => <span class="mono">{String(r.endpoint ?? "—")}</span> },
              { key: "state", label: t("State"), render: (r) => stateCell(r, r.reasonCode ? t(String(r.reasonCode)) : undefined) },
              { key: "latestHandshake", label: t("Last handshake"), render: (r) => relativeTime(r.latestHandshake as string | undefined) },
              { key: "traffic", label: t("Traffic in / out"), render: (r) => `${bytes(Number(r.rxBytes ?? 0))} / ${bytes(Number(r.txBytes ?? 0))}` },
              { key: "d", label: "", render: (r) => <DetailsButton item={r as Inst} /> },
            ]}
          />
        </Section>
      )}
      {ovpn.length > 0 && (
        <Section title={t("OpenVPN sessions")} actions={restartBtn(ovpn, "OpenVPN")}>
          <Table
            rows={ovpn}
            rowKey={(r) => String(r.id)}
            columns={[
              { key: "id", label: t("Session"), render: (r) => <span><strong>{last(String(r.id))}</strong> <span class="faint small">{String(r.kind).replace("openvpn-", "")}</span></span> },
              { key: "remote", label: t("Address"), render: (r) => <span class="mono">{String(r.realAddress ?? r.remote ?? r.remoteAddress ?? "—")}</span> },
              { key: "state", label: t("State"), render: (r) => stateCell(r) },
              { key: "since", label: t("Since"), render: (r) => relativeTime((r.connectedSince ?? r.since) as string | undefined) },
              { key: "d", label: "", render: (r) => <DetailsButton item={r as Inst} /> },
            ]}
          />
        </Section>
      )}
    </>
  );
}

// ---- diagnostics -----------------------------------------------------------------------------------------

/** Saves a `collect-diagnostics` result (the node's redacted support bundle) as a JSON file. */
function downloadBundle(nodeId: string, createdAt: string, result: string) {
  const stamp = createdAt.replace(/[^0-9]/g, "").slice(0, 14);
  const url = URL.createObjectURL(new Blob([JSON.stringify(JSON.parse(result), null, 2) + "\n"], { type: "application/json" }));
  const a = document.createElement("a");
  a.href = url;
  a.download = `meduza-np-support-${nodeId}-${stamp}.json`;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

function DiagnosticsTab(props: { n: NodeDetail; role: Role; job: (kind: string, extra?: Record<string, unknown>, done?: string) => Promise<void>; reload: () => void }) {
  const n = props.n;
  const [token, setToken] = useState<{ token: string; expiresAt: string } | null>(null);
  return (
    <>
      <Card title={t("Node")}>
        <dl class="kv">
          <dt>{t("Hostname")}</dt><dd>{n.hostname ?? "—"}</dd>
          <dt>{t("Enrolled")}</dt><dd>{n.enrolledAt ? new Date(n.enrolledAt).toLocaleString(getLocale()) : t("not enrolled")}</dd>
          <dt>{t("Revisions")}</dt><dd class="mono small">{t("desired {desired} · applied {applied}", { desired: n.desiredRevision ?? "—", applied: n.appliedRevision ?? "—" })}</dd>
        </dl>
        <div class="row" style={{ marginTop: "12px" }}>
          {props.role !== "viewer" && <Button onClick={() => void props.job("collect-diagnostics", {}, t("Diagnostics requested; the redacted support bundle appears under Jobs."))}>{t("Collect diagnostics")}</Button>}
          {props.role === "admin" && (
            <Button onClick={async () => {
              try {
                setToken(await post(`/api/v1/nodes/${encodeURIComponent(n.id)}/enrollment-tokens`, { ttlSeconds: 86400 }));
              } catch (e) {
                toast(errorText(e), "bad");
              }
            }}>{t("Enrollment token")}</Button>
          )}
          <span class="spacer" />
          {props.role === "admin" && (
            <Button variant="danger" onClick={async () => {
              if (!(await ask(t("Revoke {id}? Its identity stops working immediately; running services on the node keep their last configuration.", { id: n.id }), { ok: t("Revoke"), danger: true }))) return;
              try {
                await post(`/api/v1/nodes/${encodeURIComponent(n.id)}/revoke`);
                props.reload();
              } catch (e) {
                toast(errorText(e), "bad");
              }
            }}>{t("Revoke")}</Button>
          )}
        </div>
      </Card>
      <Card title={t("Packages & providers")}>
        <Table
          rows={n.packages as unknown as Record<string, unknown>[]}
          rowKey={(r) => String(r.package)}
          columns={[
            { key: "package", label: t("Package"), render: (r) => <span class="mono">{String(r.package)}</span> },
            { key: "installed", label: t("Installed"), render: (r) => (r.installed ? <Badge tone="ok">{String(r.installedVersion)}</Badge> : <Badge tone="idle">{t("not installed")}</Badge>) },
            { key: "registration", label: t("Registration"), render: (r) => (r.registration ? <StateBadge state={String(r.registration)} /> : <span class="faint">—</span>) },
            { key: "reason", label: t("Reason"), render: (r) => <span class="small muted">{String(r.reason ?? "")}</span> },
          ]}
        />
      </Card>
      <Card title={t("Jobs")}>
        <Table
          rows={n.jobs}
          rowKey={(r) => String(r.id)}
          empty={t("No jobs")}
          columns={[
            { key: "kind", label: t("Kind") },
            { key: "instance", label: t("Instance"), render: (r) => <span class="mono small">{String(r.instance ?? "—")}</span> },
            { key: "status", label: t("Status"), render: (r) => <StateBadge state={String(r.status)} /> },
            { key: "created_at", label: t("Created"), render: (r) => relativeTime(String(r.created_at)) },
            { key: "message", label: t("Message"), render: (r) => <span class="small">{String(r.message ?? "")}</span> },
            {
              key: "result",
              label: "",
              render: (r) =>
                r.result ? (
                  <div class="row">
                    {r.kind === "collect-diagnostics" && r.status === "succeeded" && (
                      <Button small onClick={() => downloadBundle(n.id, String(r.created_at), String(r.result))}>{t("Download")}</Button>
                    )}
                    <details><summary class="small">{t("show")}</summary><pre>{JSON.stringify(JSON.parse(String(r.result)), null, 2)}</pre></details>
                  </div>
                ) : null,
            },
          ]}
        />
      </Card>
      <details class="card">
        <summary><strong>{t("Raw report")}</strong></summary>
        <pre>{JSON.stringify(n.report, null, 2)}</pre>
      </details>
      {token && (
        <Modal title={t("Enrollment token")} onClose={() => setToken(null)}>
          <p>{t("Single use, expires {time}. On the node run:", { time: token.expiresAt })}</p>
          <CopyBlock text={`meduza-np enroll --url ${location.origin} --token ${token.token}`} />
          <p class="small muted">{t("The node generates its own identity key; the token cannot be shown again.")}</p>
        </Modal>
      )}
    </>
  );
}
