// Shared report-only presentation: zero, unavailable and stale measurements have distinct meanings.
import { Card, bytes, relativeTime, t } from "./index.tsx";

export interface ProcessResources {
  id: string;
  instance?: string;
  pid: number;
  scope: string;
  memoryRssBytes?: number | null;
}

export interface PackageResources {
  package: string;
  diskBytes?: number | null;
  dataBytes?: number | null;
  memoryRssBytes?: number | null;
  diskObservedAt: string;
  memoryObservedAt: string;
  reason?: string;
  processes: ProcessResources[];
}

export const resourceSize = (size: number | null | undefined): string => size == null ? t("Not measured") : bytes(size);

function Instances(props: { processes: ProcessResources[] }) {
  const groups = new Map<string, ProcessResources[]>();
  for (const process of props.processes) {
    const key = process.instance ?? process.id;
    groups.set(key, [...(groups.get(key) ?? []), process]);
  }
  return <div class="table-wrap"><table class="table">
    <thead><tr><th>{t("Instance / process")}</th><th>{t("Memory RSS")}</th><th>{t("Processes")}</th></tr></thead>
    <tbody>{[...groups.entries()].map(([instance, processes]) => <tr key={instance}>
      <td class="mono">{instance}</td>
      <td>{resourceSize(processes.some((p) => p.memoryRssBytes == null) ? null : processes.reduce((n, p) => n + (p.memoryRssBytes ?? 0), 0))}</td>
      <td>{processes.map((p) => <div key={`${p.scope}/${p.pid}`} class="small"><span class="mono">{p.id}</span> · {p.scope} PID {p.pid} · {resourceSize(p.memoryRssBytes)}</div>)}</td>
    </tr>)}</tbody>
  </table></div>;
}

export function PackageResourceCard(props: { packages?: PackageResources[] | null; reportedAt?: string | null }) {
  return <Card title={t("Package resource usage")}>
    {!props.packages?.length ? <p class="muted">{t("Package resource measurements are unavailable; upgrade the node planner.")}</p> : <>
      {props.reportedAt && <p class="small faint">{t("Reported {time}", { time: relativeTime(props.reportedAt) })}</p>}
      <div class="table-wrap"><table class="table">
        <thead><tr><th>{t("Package")}</th><th>{t("Package disk")}</th><th>{t("Persistent data")}</th><th>{t("Memory RSS")}</th><th>{t("Instances / processes")}</th></tr></thead>
        <tbody>{props.packages.map((p) => <tr key={p.package}>
          <td><strong>{p.package.replace(/^meduza-np-/, "")}</strong>
            {p.reason === "container-image-shared" && <div class="small muted">{t("Shared image layers; no independent process")}</div>}
          </td>
          <td title={p.diskObservedAt ? `${t("Disk measured")}: ${p.diskObservedAt}` : undefined}>{resourceSize(p.diskBytes)}</td>
          <td>{resourceSize(p.dataBytes)}</td>
          <td title={p.memoryObservedAt ? `${t("Memory measured")}: ${p.memoryObservedAt}` : undefined}>{resourceSize(p.memoryRssBytes)}</td>
          <td>{p.processes.length ? <details open><summary>{t("{count} processes", { count: p.processes.length })}</summary><Instances processes={p.processes} /></details> : <span class="muted">{p.memoryRssBytes === 0 ? t("No running process") : t("Not measured")}</span>}</td>
        </tr>)}</tbody>
      </table></div>
      <p class="small muted">{t("Disk counts allocated package files including retained versions; persistent data is separate. Download archives, shared system libraries and image layers are excluded. Disk samples refresh every 60 seconds.")}</p>
      <p class="small muted">{t("RSS includes shared pages and is charged to the service instance, not again to its binary provider. Instances share package files. WireGuard kernel memory is not process RSS. Unavailable measurements are not zero.")}</p>
    </>}
  </Card>;
}
