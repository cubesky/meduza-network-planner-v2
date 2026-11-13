import { useState } from "preact/hooks";
import { accessFirst, InstanceTree } from "@meduza-np/ui-kit/instance-tree";
import { BinaryProgress, PackageProgress, installPhaseLabel, progressFromStatus } from "@meduza-np/ui-kit/package-progress";
import { PackageResourceCard, type PackageResources } from "@meduza-np/ui-kit/package-resources";
import { Badge, Button, Card, Field, Loadable, StateBadge, Table, TextInput, NumberInput, Checkbox, bytes, getLocale, relativeTime, t, toast, useAsync, type AsyncState } from "@meduza-np/ui-kit";
import { errorText, get, post, roleAtLeast, type Feature, type PackageStatus, type Report, type Session } from "./api.ts";
import { mihomoPanelUrl, type MihomoPanel } from "./mihomo-panel.ts";

const kib = (v: number | undefined) => (v == null ? "—" : bytes(v * 1024));

async function act(path: string, body: unknown, done: string, after?: () => void) {
  try {
    await post(path, body);
    toast(done, "ok");
    after?.();
  } catch (e) {
    toast(errorText(e), "bad");
  }
}

// ---- overview -------------------------------------------------------------------------------------------------

export function OverviewPage(props: { session: Session; status: AsyncState<Report> }) {
  return (
    <Loadable state={props.status}>
      {(r) => (
        <div>
          <Card
            title={<>{t("Sync")} <StateBadge state={r.sync.state} /></>}
            actions={roleAtLeast(props.session.role, "operator") && <Button small onClick={() => act("/api/sync", {}, t("Sync requested"))}>{t("Sync now")}</Button>}
          >
            <dl class="kv">
              <dt>{t("Node")}</dt><dd>{r.nodeId || t("Not enrolled")}</dd>
              <dt>{t("Channel")}</dt><dd>{r.sync.transport} · {r.sync.mode} · {r.sync.behavior}</dd>
              <dt>{t("Desired / received / applied")}</dt>
              <dd>{r.sync.desiredRevision ?? "—"} / {r.sync.receivedRevision ?? "—"} / {r.sync.appliedRevision ?? "—"}</dd>
              <dt>{t("Last successful revision")}</dt><dd>{r.sync.lastSuccessfulRevision ?? "—"}</dd>
              {r.sync.applyStage && (<><dt>{t("Apply stage")}</dt><dd>{r.sync.applyStage}</dd></>)}
              {r.sync.degradedReason && (<><dt>{t("Degraded reason")}</dt><dd><Badge tone="warn">{r.sync.degradedReason}</Badge></dd></>)}
              {r.sync.lastError && (<><dt>{t("Last error")}</dt><dd class="bad">{r.sync.lastError}</dd></>)}
              {r.sync.recoveryError && (<><dt>{t("Last-good recovery")}</dt><dd class="bad">{r.sync.recoveryError}</dd></>)}
            </dl>
            {r.sync.backoff.failureLevel > 0 && (
              <p class="small">
                <Badge tone="warn">{t("Backoff L{level}", { level: r.sync.backoff.failureLevel })}</Badge> {t("Window {seconds}s", { seconds: r.sync.backoff.windowSeconds })} ·
                {r.sync.backoff.nextAttemptInMs != null && <> {t("Next attempt in {seconds}s", { seconds: Math.ceil(r.sync.backoff.nextAttemptInMs / 1000) })} ·</>}
                {r.sync.backoff.serverCooldownMs ? <> {t("Server cooldown {seconds}s", { seconds: Math.ceil(r.sync.backoff.serverCooldownMs / 1000) })} ·</> : null}
                {r.sync.backoff.probing ? t("Probing recovery") : ""} {t("Reason")}: {r.sync.backoff.lastFailure ?? "—"}
                <br />{t("The last successful configuration continues running during backoff.")}
              </p>
            )}
          </Card>
          <MaintenanceCard report={r} session={props.session} />
          <MihomoPanelCard />
          <Card title={t("Resources")}>
            <dl class="kv">
              <dt>{t("Planner RSS")}</dt><dd>{kib(r.resources?.plannerRssKib)}</dd>
              <dt>{t("Available memory")}</dt><dd>{kib(r.resources?.memAvailableKib)} / {kib(r.resources?.memTotalKib)}</dd>
              <dt>{t("Free state disk")}</dt><dd>{kib(r.resources?.stateDiskFreeKib)}</dd>
              <dt>{t("Load")}</dt><dd>{r.resources?.load1Milli != null ? (r.resources.load1Milli / 1000).toFixed(2) : "—"}</dd>
              <dt>{t("Supervised processes")}</dt><dd>{r.resources?.supervisedProcesses ?? "—"}</dd>
              <dt>{t("System")}</dt><dd>{String(r.system?.os ?? "")} {String(r.system?.osRelease ?? "")} · {String(r.system?.arch ?? "")}{r.system?.container ? ` · ${r.system?.networkMode === "bridge" ? t("container, bridge network") : r.system?.networkMode === "host" ? t("container, host network") : t("container")}` : ""}{r.system?.openwrt ? " · OpenWrt" : ""}</dd>
              <dt>{t("Reported at")}</dt><dd>{relativeTime(r.observedAt)}</dd>
            </dl>
          </Card>
          <PackageResourceCard packages={r.resources?.packages} reportedAt={r.observedAt} />
        </div>
      )}
    </Loadable>
  );
}

export function MihomoPanelCard() {
  const [busy, setBusy] = useState(false);
  const status = useAsync(() => get<{enabled:boolean;ready:boolean}>("/api/mihomo/panel/status"), [], 10000);
  if (!status.data?.enabled) return null;
  const open = async () => {
    const tab = window.open("about:blank", "_blank");
    if (!tab) { toast(t("Please allow pop-ups to open the Mihomo panel."), "bad"); return; }
    tab.opener = null;
    setBusy(true);
    try {
      const info = await post<MihomoPanel>("/api/mihomo/panel", {});
      tab.location.replace(mihomoPanelUrl(info, window.location.hostname));
    } catch (e) {
      tab.close();
      toast(errorText(e), "bad");
    } finally { setBusy(false); }
  };
  return <Card title="Mihomo" actions={<Button small disabled={busy} onClick={() => void open()}>{t("Open Mihomo panel")}</Button>}>
    <p class="small muted">{t("Connect automatically using this node's address and API secret.")}</p>
  </Card>;
}

function MaintenanceCard(props: { report: Report; session: Session }) {
  const m = props.report.maintenance;
  const [reason, setReason] = useState("");
  const [minutes, setMinutes] = useState<number | undefined>(30);
  const [pause, setPause] = useState(true);
  const canAct = roleAtLeast(props.session.role, "operator");
  return (
    <Card title={<>{t("Maintenance mode")} {m?.active ? <Badge tone="warn">{t("In progress")}</Badge> : <Badge tone="idle">{t("Disabled")}</Badge>}</>}>
      {m?.active ? (
        <div>
          <p>{m.reason} · {t("Started by {name}", { name: m.by })} · {t("Until {time}", { time: new Date(m.until).toLocaleString(getLocale()) })} {m.applyPaused && <Badge tone="warn">{t("Configuration application paused")}</Badge>}</p>
          {canAct && <Button onClick={() => act("/api/maintenance/stop", {}, t("Maintenance ended"))}>{t("End maintenance")}</Button>}
        </div>
      ) : canAct ? (
        <div class="form-row">
          <Field label={t("Reason")}><TextInput value={reason} onInput={setReason} placeholder={t("For example: replace the uplink")} /></Field>
          <Field label={t("Duration (minutes)")}><NumberInput value={minutes} onInput={setMinutes} min={1} max={1440} /></Field>
          <Checkbox checked={pause} onChange={setPause} label={t("Pause automatic configuration application")} />
          <Button
            disabled={!reason.trim() || !minutes}
            onClick={() => act("/api/maintenance/start", { reason, durationSeconds: (minutes ?? 30) * 60, pauseApply: pause, stopInstances: [] }, t("Maintenance started"))}
          >
            {t("Start maintenance")}
          </Button>
        </div>
      ) : (
        <p class="muted">{t("Operator permission is required.")}</p>
      )}
    </Card>
  );
}

// ---- features / packages ----------------------------------------------------------------------------------------

export function FeaturesPage() {
  const st = useAsync(() => get<Feature[]>("/api/features"), [], 10000);
  return (
    <Card title={t("Features of installed packages")}>
      <Loadable state={st} empty={(f) => f.length === 0}>
        {(f) => (
          <Table
            rowKey={(x) => String(x.id)}
            rows={f as never}
            columns={[
              { key: "id", label: t("Features") },
              { key: "package", label: t("Package") },
              { key: "state", label: t("Status"), render: (x: Feature) => <StateBadge state={x.available ? (x.healthy === false ? "degraded" : "ok") : x.registered ? "idle" : "failed"} label={featureLabel(x)} /> },
              { key: "provider", label: t("Provider"), render: (x: Feature) => x.provider ?? "—" },
              {
                key: "reason",
                label: t("Description"),
                render: (x: Feature) =>
                  x.reason ?? (x.limitationCode === "bridge-network" ? t("Acts only inside the container's bridge network: host/LAN traffic is affected only when routed into the container") : x.limitation ?? ""),
              },
            ] as never}
          />
        )}
      </Loadable>
    </Card>
  );
}

function featureLabel(f: Feature): string {
  if (!f.registered) return t("Registration failed");
  if (!f.available) return t("Missing dependencies");
  if (!f.enabled) return t("Disabled");
  if (f.running === false) return t("Not running");
  if (f.healthy === false) return t("Unhealthy");
  return f.limitationCode ? t("Healthy (limited)") : t("Healthy");
}

export function PackagesPage() {
  const st = useAsync(() => get<{ packages: PackageStatus[]; resources?: PackageResources[]; providers?: Record<string, { path: string; source: string; version?: string }>; providerErrors?: Record<string, string> }>("/api/packages"), [], 15000);
  return (
    <Loadable state={st}>
      {(p) => (
        <div>
          <Card title={t("Installed packages")}>
            <Table
              rowKey={(x) => String(x.name)}
              rows={p.packages as never}
              columns={[
                { key: "name", label: t("Package") },
                { key: "version", label: t("Version") },
                { key: "role", label: t("Role") },
                { key: "registration", label: t("Registration"), render: (x: PackageStatus) => <StateBadge state={x.registration === "registered" ? "ok" : "failed"} label={t(x.registration)} /> },
                { key: "reason", label: t("Description"), render: (x: PackageStatus) => x.reason ?? "" },
              ] as never}
            />
          </Card>
          <Card title={t("Binary providers")}>
            <dl class="kv">
              {Object.entries(p.providers ?? {}).map(([slot, v]) => (
                <><dt>{slot}</dt><dd>{v.source} · <code>{v.path}</code> {v.version ?? ""}</dd></>
              ))}
              {Object.entries(p.providerErrors ?? {}).map(([slot, e]) => (
                <><dt>{slot}</dt><dd class="bad">{e}</dd></>
              ))}
            </dl>
          </Card>
          <PackageResourceCard packages={p.resources} />
        </div>
      )}
    </Loadable>
  );
}

// ---- instances --------------------------------------------------------------------------------------------------

interface Proc { id: string; state: string; pid?: number; restarts: number; lastExit?: string }

export function InstancesPage(props: { session: Session }) {
  const report = useAsync(() => get<Report>("/api/status"), [], 5000);
  const procs = useAsync(() => get<{ processes: Proc[]; applied?: Record<string, { package: string; processes: string[] }> }>("/api/instances"), [], 5000);
  const canAct = roleAtLeast(props.session.role, "operator");
  const maintenance = !!report.data?.maintenance?.active;
  return (
    <div>
      <Card title={t("Instances")}>
        <Loadable state={procs} empty={(p) => Object.keys(p.applied ?? {}).length === 0}>
          {(p) => (
            <Table
              rowKey={(x) => String(x.id)}
              rows={accessFirst(Object.entries(p.applied ?? {}).map(([id, a]) => ({ id, package: a.package, processes: a.processes.join(", ") })),
                (x) => report.data?.instances.find((i) => i.id === x.id)?.kind) as never}
              columns={[
                { key: "id", label: t("Instances") },
                { key: "package", label: t("Package") },
                { key: "processes", label: t("Processes") },
                {
                  key: "actions",
                  label: "",
                  render: (x: { id: string }) =>
                    canAct && (
                      <span style={{ display: "flex", gap: "4px" }}>
                        <Button small onClick={() => act("/api/instances/restart", { instance: x.id }, t("{name} restarted", { name: x.id }), procs.reload)}>{t("Restart")}</Button>
                        <Button small disabled={!maintenance} title={maintenance ? "" : t("Start maintenance first; otherwise automatic reconciliation will restart the instance")} onClick={() => act("/api/instances/stop", { instance: x.id }, t("{name} stopped", { name: x.id }), procs.reload)}>{t("Stop")}</Button>
                        <Button small onClick={() => act("/api/instances/start", { instance: x.id }, t("{name} started", { name: x.id }), procs.reload)}>{t("Start")}</Button>
                      </span>
                    ),
                },
              ] as never}
            />
          )}
        </Loadable>
      </Card>
      <Card title={t("Processes")}>
        <Loadable state={procs}>
          {(p) => (
            <Table
              rowKey={(x) => String(x.id)}
              rows={accessFirst(p.processes, (x) => report.data?.instances.find((i) => i.id === x.id.split("/").slice(0, 2).join("/"))?.kind) as never}
              columns={[
                { key: "id", label: t("Processes") },
                { key: "state", label: t("Status"), render: (x: Proc) => <StateBadge state={x.state} /> },
                { key: "pid", label: "PID" },
                { key: "restarts", label: t("Restart count") },
                { key: "lastExit", label: t("Last exit") },
              ] as never}
            />
          )}
        </Loadable>
      </Card>
      <Card title={t("Status and neighbors")}>
        <Loadable state={report} empty={(r) => r.instances.length === 0}>
          {(r) => (
            <InstanceTree items={r.instances} />
          )}
        </Loadable>
      </Card>
    </div>
  );
}

// ---- config / updates / diagnostics -----------------------------------------------------------------------------

export function ConfigPage() {
  const st = useAsync(() => get<{ digest: string; config: unknown } | null>("/api/config"), []);
  return (
    <Card title={t("Last successful configuration (read-only, secrets redacted)")}>
      <p class="small muted">{t("Edit and publish configuration drafts in Manager; this dashboard does not keep a separate master configuration.")}</p>
      <Loadable state={st} empty={(c) => !c}>
        {(c) => (
          <div>
            <p class="small">digest <code>{c!.digest}</code></p>
            <pre class="code">{JSON.stringify(c!.config, null, 2)}</pre>
          </div>
        )}
      </Loadable>
    </Card>
  );
}

export function UpdatesPage(props: { session: Session; status: AsyncState<Report> }) {
  const st = useAsync(() => get<Record<string, any>>("/api/update/status"), [], 3000);
  const report = props.status;
  const op = roleAtLeast(props.session.role, "operator");
  const admin = roleAtLeast(props.session.role, "admin");
  return (
    <div>
      {st.data?.available !== false && <PackageProgress progress={st.data ? progressFromStatus(st.data) : report.data?.update} reportedAt={report.data?.observedAt} />}
      <BinaryDownloads report={report.data} />
      <Loadable state={st}>
        {(u) =>
          u.available === false ? (
            <Card title={t("Updates")}><p>{t("Updater is not installed on this node; automatic updates are unavailable.")}</p></Card>
          ) : (
            <div>
              <Card
                title={<>{t("Updates")} {u.busy ? <Badge tone="info">{installPhaseLabel(u.busy === "checking" ? "checking" : "installing")}</Badge> : null} {u.control?.paused ? <Badge tone="warn">{t("Paused")}</Badge> : null}</>}
                actions={
                  <span style={{ display: "flex", gap: "6px" }}>
                    {op && <Button small disabled={!!u.busy} onClick={() => act("/api/update/check", {}, t("Update check started"), st.reload)}>{t("Check")}</Button>}
                    {admin && <Button small variant="primary" disabled={!!u.busy} onClick={() => act("/api/update/apply", {}, t("Update started"), st.reload)}>{u.progress?.phase === "failed" || u.state?.lastResult?.outcome === "failed" ? t("Retry installation") : t("Upgrade now")}</Button>}
                    {op && (u.control?.paused
                      ? <Button small onClick={() => act("/api/update/resume", {}, t("Resumed"), st.reload)}>{t("Resume")}</Button>
                      : <Button small onClick={() => act("/api/update/pause", {}, t("Paused"), st.reload)}>{t("Pause")}</Button>)}
                    {op && <Button small onClick={() => act("/api/update/defer", { seconds: 86400 }, t("Deferred for 24 hours"), st.reload)}>{t("Defer for one day")}</Button>}
                  </span>
                }
              >
                <dl class="kv">
                  <dt>{t("Channel")}</dt><dd>{u.config?.channel}{u.config?.pinnedGroupVersion ? ` · ${t("Pinned {version}", { version: u.config.pinnedGroupVersion })}` : ""}</dd>
                  <dt>{t("Installed group")}</dt><dd>{u.installed?.groupVersion ?? "—"} ({t("generation {generation}", { generation: u.installed?.generation ?? 0 })})</dd>
                  <dt>{t("Available group")}</dt><dd>{u.state?.available?.groupVersion ?? "—"}</dd>
                  <dt>{t("Last check")}</dt><dd>{relativeTime(u.state?.lastCheck)} {u.state?.lastCheckError && <span class="bad">{u.state.lastCheckError}</span>}</dd>
                  <dt>{t("Last result")}</dt><dd>{u.state?.lastResult ? `${u.state.lastResult.groupVersion} ${t(u.state.lastResult.outcome)} ${u.state.lastResult.detail ?? ""}` : "—"}</dd>
                  {u.journal && (<><dt>{t("In progress")}</dt><dd><Badge tone="info">{u.journal.groupVersion} · {installPhaseLabel(u.journal.phase)}</Badge></dd></>)}
                </dl>
              </Card>
              <Card title={t("Installed package versions")}>
                <Table
                  rowKey={(x) => String(x.name)}
                  rows={Object.entries(u.installed?.packages ?? {}).map(([name, p]: [string, any]) => ({ name, version: p.version, source: p.source })) as never}
                  columns={[{ key: "name", label: t("Package") }, { key: "version", label: t("Version") }, { key: "source", label: t("Source") }] as never}
                />
              </Card>
              {u.lastPlan && (
                <Card title={t("Plan: {version}", { version: u.lastPlan.groupVersion })}>
                  {u.lastPlan.blocked?.length > 0 && <p class="bad">{t("Blocked")}: {u.lastPlan.blocked.join("; ")}</p>}
                  <Table
                    rowKey={(x) => String(x.package)}
                    rows={u.lastPlan.actions as never}
                    columns={[{ key: "package", label: t("Package") }, { key: "kind", label: t("Action") }, { key: "from", label: t("Current") }, { key: "to", label: t("Target") }] as never}
                  />
                </Card>
              )}
            </div>
          )
        }
      </Loadable>
    </div>
  );
}

function BinaryDownloads({ report }: { report?: Report | null }) {
  return <>{(report?.instances ?? []).filter((i) => i.kind === "mihomo-provider" && (i.mode === "auto-download" || i.downloadProgress)).map((i) => <BinaryProgress
    name="Mihomo" progress={i.downloadProgress} providerState={i.providerState} reason={i.reason}
    configurationError={report?.sync.state === "apply-failed" ? report.sync.lastError : undefined}
  />)}</>;
}

/** Downloads the planner's redacted support bundle (status, checks, configuration, logs; credentials removed). */
async function exportBundle() {
  try {
    const b = await get<Record<string, unknown>>("/api/support-bundle");
    const url = URL.createObjectURL(new Blob([JSON.stringify(b, null, 2) + "\n"], { type: "application/json" }));
    const a = document.createElement("a");
    a.href = url;
    a.download = `meduza-np-support-${new Date().toISOString().replace(/[^0-9]/g, "").slice(0, 14)}.json`;
    a.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
    const cut = Array.isArray(b.truncated) ? t(" (truncated: {items})", { items: (b.truncated as string[]).join(", ") }) : "";
    toast(t("Support bundle exported with credentials redacted{detail}", { detail: cut }), "ok");
  } catch (e) {
    toast(t("Export failed: {error}", { error: errorText(e) }), "bad");
  }
}

export function DiagnosticsPage() {
  const doctor = useAsync(() => get<Array<{ check: string; ok: boolean; detail?: string }>>("/api/doctor"), []);
  const logs = useAsync(() => get<string[]>("/api/logs?lines=300"), [], 5000);
  return (
    <div>
      <Card
        title={t("Diagnostics")}
        actions={
          <div class="row">
            <Button small onClick={() => void exportBundle()}>{t("Export support bundle")}</Button>
            <Button small onClick={doctor.reload}>{t("Check again")}</Button>
          </div>
        }
      >
        <Loadable state={doctor}>
          {(d) => (
            <ul class="plain">
              {(Array.isArray(d) ? d : []).map((c: any) => (
                <li><StateBadge state={c.ok ? "ok" : "failed"} /> {c.check ?? c.name} <span class="small muted">{c.detail ?? c.message ?? ""}</span></li>
              ))}
            </ul>
          )}
        </Loadable>
      </Card>
      <Card title={t("Planner logs")}>
        <Loadable state={logs}>{(l) => <pre class="code">{(Array.isArray(l) ? l : []).join("\n")}</pre>}</Loadable>
      </Card>
    </div>
  );
}
