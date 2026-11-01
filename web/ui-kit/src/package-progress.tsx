// Presentation shared by manager and dashboard; consumes node-reported progress without fetching or mutating it.
import { Badge, Card, bytes, getLocale, t, type Tone } from "./index.tsx";

export interface InstallProgress {
  phase: string;
  active?: boolean;
  groupVersion?: string | null;
  currentPackage?: string | null;
  packages?: string[] | null;
  pendingPackages?: string[];
  completedPackages?: number;
  totalPackages?: number | null;
  downloadedBytes?: number | null;
  bytesTotal?: number | null;
  error?: string | null;
  nextCheckUnix?: number | null;
  attempt?: number | null;
  maxAttempts?: number | null;
  retryAtUnix?: number | null;
  retryError?: string | null;
}

const labels: Record<string, string> = {
  waiting: "Waiting for installation", checking: "Checking packages", downloading: "Downloading packages",
  retrying: "Waiting to retry download",
  staged: "Installing packages", installing: "Installing packages", locked: "Preparing package activation",
  switched: "Activating packages", verifying: "Verifying package registration", "self-restart": "Restarting updater",
  done: "Packages ready", idle: "Idle", paused: "Installation paused", failed: "Package installation failed",
  "rolled-back": "Package installation rolled back",
};
export const installPhaseLabel = (phase: string) => t(labels[phase] ?? "Installing packages");
const tone = (p: InstallProgress): Tone => p.phase === "failed" || p.phase === "rolled-back" ? "bad" : p.phase === "done" ? "ok" : p.phase === "retrying" ? "warn" : p.active ? "info" : "warn";

/** Older updater APIs still provide journal/error phases; never invent byte progress when they do not report it. */
export function progressFromStatus(status: Record<string, any>): InstallProgress | null {
  if (status.progress) return status.progress as InstallProgress;
  const j = status.journal;
  const error = status.state?.lastCheckError || (status.state?.lastResult?.outcome === "failed" ? status.state.lastResult.detail : null);
  return { phase: j?.phase ?? (status.busy ? "checking" : status.control?.paused ? "paused" : error ? "failed" : "idle"),
    active: !!j || !!status.busy, groupVersion: j?.groupVersion ?? status.installed?.groupVersion,
    packages: j?.changed, error: j || status.busy ? null : error, nextCheckUnix: status.state?.nextCheckUnix };
}

export function packageInstallLabel(progress: InstallProgress | null | undefined, pkg: string): string | null {
  if (!progress || !(progress.pendingPackages ?? progress.packages ?? []).includes(pkg)) return null;
  if (progress.phase === "failed" || progress.phase === "rolled-back") return installPhaseLabel(progress.phase);
  if (progress.currentPackage === pkg) return installPhaseLabel(progress.phase);
  if (["downloading", "retrying"].includes(progress.phase)) {
    return (progress.packages ?? []).slice(0, progress.completedPackages ?? 0).includes(pkg) ? t("Download verified") : t("Queued for download");
  }
  return installPhaseLabel(progress.phase === "done" ? "waiting" : progress.phase);
}

export function PackageProgress(props: { progress?: InstallProgress | null; reportedAt?: string }) {
  const p = props.progress;
  if (!p) return null;
  const total = p.bytesTotal != null && p.bytesTotal > 0 ? p.bytesTotal : undefined;
  const downloaded = total != null && p.downloadedBytes != null ? Math.min(total, Math.max(0, p.downloadedBytes)) : undefined;
  const percentage = total != null && downloaded != null ? Math.floor(downloaded / total * 100) : null;
  const pending = p.pendingPackages ?? [];
  const packages = [...new Set([...(p.packages ?? []), ...pending])];
  return (
    <Card title={t("Package installation")}>
      <div class="row"><Badge tone={tone(p)}>{installPhaseLabel(p.phase)}</Badge>{p.groupVersion && <span class="mono small">{p.groupVersion}</span>}</div>
      {p.currentPackage && p.active && <p class="mono small">{t("Current package")}: {p.currentPackage}</p>}
      {p.active && (
        <div style={{ marginTop: "12px" }}>
          <progress aria-label={t("Package download progress")} value={downloaded} max={total ?? 1} style={{ width: "100%" }} />
          <p class="small muted">
            {percentage != null ? `${percentage}% · ${bytes(downloaded!)} / ${bytes(total!)}` : t("Waiting for progress data")}
            {p.totalPackages != null && ` · ${t("{done}/{total} downloads verified", { done: p.completedPackages ?? 0, total: p.totalPackages })}`}
          </p>
          {p.attempt != null && p.maxAttempts != null && <p class="small muted">{t("Download attempt {attempt}/{maximum}", { attempt: p.attempt, maximum: p.maxAttempts })}</p>}
          {p.retryAtUnix != null && <p class="small muted">{t("Automatic download retry")}: {new Date(p.retryAtUnix * 1000).toLocaleString(getLocale())}</p>}
          {!["downloading", "retrying"].includes(p.phase) && <p class="small muted">{installPhaseLabel(p.phase)} · {t("Download completion does not mean installation is complete.")}</p>}
        </div>
      )}
      {p.error && <div class="callout bad" style={{ overflowWrap: "anywhere", marginTop: "12px" }}>{p.error}</div>}
      {p.active && p.retryError && <div class="callout warn" style={{ overflowWrap: "anywhere", marginTop: "12px" }}>{p.retryError}</div>}
      {!p.active && p.phase !== "done" && p.phase !== "paused" && p.nextCheckUnix != null && <p class="small muted">{t("Next automatic check")}: {new Date(p.nextCheckUnix * 1000).toLocaleString(getLocale())}</p>}
      {packages.length > 0 && <div class="row" style={{ marginTop: "12px" }}>{packages.map((pkg) => <Badge tone={pkg === p.currentPackage && p.active ? "info" : pending.includes(pkg) ? "warn" : "idle"} title={packageInstallLabel(p, pkg) ?? ""}>{pkg.replace(/^meduza-np-/, "")}{pkg === p.currentPackage && p.active ? ` · ${t("current")}` : ""}</Badge>)}</div>}
      {props.reportedAt && <p class="small faint">{t("Last node report")}: {new Date(props.reportedAt).toLocaleString(getLocale())}</p>}
    </Card>
  );
}
