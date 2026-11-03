// Upgrade requests use the authenticated node job channel. Acknowledgement means scheduled, not completed.
import { useState } from "preact/hooks";
import { Badge, Button, getLocale, t, toast } from "@meduza-np/ui-kit";
import { PackageProgress, type InstallProgress } from "@meduza-np/ui-kit/package-progress";
import { errorText, post } from "../api.ts";
import type { NodeDetail } from "./Node.tsx";

export function NodeUpdates(props: { node: NodeDetail; canAct: boolean; reload: () => void }) {
  const { node: n } = props;
  const [busy, setBusy] = useState<string | null>(null);
  const [submitted, setSubmitted] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const p: InstallProgress | undefined = n.update ?? n.report?.update;
  const jobs = n.jobs.filter((j) => j.kind === "update-check" || j.kind === "update-apply");
  const pending = (j: Record<string, unknown>) => ["pending", "delivered"].includes(String(j.status)) && Date.parse(String(j.expires_at)) > Date.now();
  const job = jobs.find(pending) ?? jobs.find((j) => j.id === submitted) ?? jobs[0];
  const waiting = !!busy || jobs.some(pending) || !!submitted && !jobs.some((j) => j.id === submitted);
  const supported = p?.remoteControl === true;
  const disabled = waiting || !!p?.active || !supported || !!n.revokedAt;
  const request = async (kind: "update-check" | "update-apply") => {
    if (disabled) return;
    setBusy(kind); setError(null);
    try {
      const response = await post<{ jobId: string }>(`/api/v1/nodes/${encodeURIComponent(n.id)}/jobs`, { kind });
      setSubmitted(response.jobId);
      toast(t("Update request queued; waiting for the node."), "ok");
      props.reload();
    } catch (e) { setError(errorText(e)); }
    finally { setBusy(null); }
  };
  const status = job && (["pending", "delivered"].includes(String(job.status)) && !pending(job) ? "expired" : String(job.status));
  return <PackageProgress progress={p} reportedAt={n.report?.observedAt} actions={props.canAct && <div class="row">
    <Button small disabled={disabled} busy={busy === "update-check"} onClick={() => void request("update-check")}>{t("Check for updates")}</Button>
    <Button small variant="primary" disabled={disabled} busy={busy === "update-apply"} onClick={() => void request("update-apply")}>{t("Upgrade now")}</Button>
  </div>}>
    {props.canAct && <p class="small muted">{supported
      ? t("Check only refreshes available versions. Upgrade now uses the node's configured channel or pinned group, bypasses pause and maintenance windows, and retries a previously failed group.")
      : t("Remote upgrade control is unavailable. Update planner and ensure updater is running on this node.")}</p>}
    {error && <div role="alert" class="callout bad">{error}</div>}
    {waiting && !job && <p class="small muted">{t("Update request queued; waiting for the node.")}</p>}
    {job && <div class={`callout ${status === "failed" || status === "expired" ? "bad" : "info"}`} data-update-request>
      <div class="row"><strong>{job.kind === "update-apply" ? t("Upgrade request") : t("Update check request")}</strong>
        <Badge tone={status === "failed" || status === "expired" ? "bad" : status === "succeeded" ? "ok" : "info"}>
          {status === "succeeded" ? t("Updater accepted the request") : status === "delivered" ? t("Delivered; waiting for updater acknowledgement")
            : status === "pending" ? t("Waiting for the node") : status === "expired" ? t("Update request expired") : status === "failed" ? t("Update request failed") : t("Update request cancelled")}
        </Badge></div>
      {status === "succeeded" && <p class="small">{t("Acceptance confirms scheduling. Follow the reported upgrade progress for completion or rollback.")}</p>}
      {(job.error_code || job.message) && <p class="small" style={{ overflowWrap: "anywhere" }}>{[job.error_code, job.message].filter(Boolean).map(String).join(": ")}</p>}
      <p class="small muted">{t("Requested at")}: {new Date(String(job.created_at)).toLocaleString(getLocale())}</p>
    </div>}
  </PackageProgress>;
}
