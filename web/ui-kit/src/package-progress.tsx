// Presentation shared by manager and dashboard; consumes node-reported progress without fetching or mutating it.
import { Badge, Card, bytes, getLocale, t, type Tone } from "./index.tsx";
import type { ComponentChildren } from "preact";

export interface BinaryTransferProgress {
  phase: string;
  active: boolean;
  targetVersion: string;
  platform?: string;
  downloadedBytes: number;
  bytesTotal: number;
  attempt: number;
  retryAtUnix?: number | null;
  error?: string | null;
  updatedAt?: string;
}

const binaryPhases: Record<string, string> = {
  waiting: "Waiting for binary", downloading: "Downloading binary", verifying: "Verifying binary checksum",
  decompressing: "Extracting binary", validating: "Checking binary version and architecture", activating: "Activating binary",
  done: "Binary ready", failed: "Binary download failed", retrying: "Waiting to retry binary download", interrupted: "Binary preparation interrupted",
};

/** Binary acquisition is independent of installing the adapter package and of applying network configuration. */
export function BinaryProgress(props: { name: string; progress?: BinaryTransferProgress | null; providerState?: string; reason?: string; configurationError?: string }) {
  const p = props.progress;
  const phase = p?.phase ?? (props.providerState === "registered" ? "done" : "waiting");
  const bad = ["failed", "interrupted"].includes(phase);
  const total = p && p.bytesTotal > 0 ? p.bytesTotal : undefined;
  const done = p && total ? Math.min(total, Math.max(0, p.downloadedBytes)) : undefined;
  const percent = total && done != null ? Math.floor(done / total * 100) : null;
  return <Card title={t("{name} binary download", { name: props.name })}>
    <div class="row"><Badge tone={bad ? "bad" : phase === "done" ? "ok" : phase === "retrying" ? "warn" : "info"}>{t(binaryPhases[phase] ?? "Waiting for binary")}</Badge>
      {p?.targetVersion && <span class="mono small">{p.targetVersion}</span>}{p?.platform && <span class="small muted">{p.platform}</span>}
    </div>
    {p && <div style={{ marginTop: "12px" }}>
      <progress aria-label={t("Binary download progress")} value={done} max={total ?? 1} style={{ width: "100%" }} />
      <p class="small muted">{percent != null ? `${percent}% · ${bytes(done!)} / ${bytes(total!)}` : t("Waiting for progress data")}</p>
      {p.attempt > 0 && <p class="small muted">{t("Download attempt {attempt}", { attempt: p.attempt })}</p>}
      {p.active && phase !== "downloading" && <p class="small muted">{t("Download completion is followed by checksum, extraction and executable checks.")}</p>}
      {p.retryAtUnix != null && <p class="small muted">{t("Automatic download retry")}: {new Date(p.retryAtUnix * 1000).toLocaleString(getLocale())}</p>}
    </div>}
    {(!p?.active && phase !== "done") && (p?.error || props.reason) && <div class={`callout ${bad ? "bad" : "info"}`} style={{ overflowWrap: "anywhere", marginTop: "12px" }}>{p?.error ?? props.reason}</div>}
    {phase !== "done" && props.configurationError && <div class="callout warn" style={{ overflowWrap: "anywhere", marginTop: "12px" }}>{t("Binary preparation is affected by a configuration application failure: {error}", { error: props.configurationError })}</div>}
    {p?.updatedAt && <p class="small faint">{t("Last download update")}: {new Date(p.updatedAt).toLocaleString(getLocale())}</p>}
  </Card>;
}

export interface InstallProgress {
  remoteControl?: boolean;
  phase: string;
  active?: boolean;
  groupVersion?: string | null;
  installedGroupVersion?: string | null;
  availableGroupVersion?: string | null;
  installedAt?: string | null;
  lastCheck?: string | null;
  updatedAt?: string | null;
  lastResult?: { groupVersion?: string; outcome?: string; at?: string; detail?: string | null } | null;
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
const progressLabel = (p: InstallProgress) => p.phase === "done" && ((p.totalPackages ?? 0) > 0 || p.lastResult?.outcome === "done") ? t("Update completed") : installPhaseLabel(p.phase);
const resultLabel = (outcome?: string) => outcome === "done" ? t("Update completed") : outcome === "failed" ? installPhaseLabel("failed") : outcome === "rolledback" || outcome === "rolled-back" ? installPhaseLabel("rolled-back") : t(outcome ?? "unknown");

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

/** Node lists retain the installed group and finished progress instead of hiding all inactive upgrades. */
export function PackageUpdateSummary(props: { progress?: InstallProgress | null }) {
  const p = props.progress;
  if (!p) return <span class="faint small">{t("Update status unavailable")}</span>;
  const total = p.bytesTotal != null && p.bytesTotal > 0 ? p.bytesTotal : null;
  const done = total != null && p.downloadedBytes != null ? Math.min(total, Math.max(0, p.downloadedBytes)) : null;
  const percent = done != null && total != null ? Math.floor(done / total * 100) : null;
  const installed = p.installedGroupVersion ?? (!p.active && p.phase === "done" ? p.groupVersion : null);
  return <div class="stack" style={{ gap: "4px", minWidth: "165px" }}>
    <div class="row" style={{ gap: "6px" }}><Badge tone={tone(p)} title={p.error ?? p.retryError ?? p.currentPackage ?? ""}>{progressLabel(p)}</Badge>
      {(installed || p.groupVersion) && <span class="mono small" title={installed ? t("Installed group") : t("Target group")}>{installed ?? p.groupVersion}</span>}
    </div>
    {p.active && p.currentPackage && <span class="mono small">{p.currentPackage.replace(/^meduza-np-/, "")}</span>}
    {percent != null && <>
      <progress aria-label={t("Package download progress")} value={done!} max={total!} style={{ width: "100%", height: "5px" }} />
      <span class="small muted">{percent}% · {bytes(done!)} / {bytes(total!)}</span>
    </>}
    {p.totalPackages != null && <span class="small muted">{t("{done}/{total} downloads verified", { done: p.completedPackages ?? 0, total: p.totalPackages })}</span>}
    {p.active && percent == null && <span class="small muted">{t("Waiting for progress data")}</span>}
    {p.error && <span class="small" style={{ color: "var(--bad)", overflowWrap: "anywhere" }}>{p.error}</span>}
  </div>;
}

export function PackageProgress(props: { progress?: InstallProgress | null; reportedAt?: string; actions?: ComponentChildren; children?: ComponentChildren }) {
  const p = props.progress;
  if (!p) return <Card title={t("Packages and updates")} actions={props.actions}>
    <Badge tone="idle">{t("Update status unavailable")}</Badge>
    <p class="small muted">{t("Waiting for the node to report updater status. Check that updater is installed and running.")}</p>
    {props.reportedAt && <p class="small faint">{t("Last node report")}: {new Date(props.reportedAt).toLocaleString(getLocale())}</p>}
    {props.children}
  </Card>;
  const total = p.bytesTotal != null && p.bytesTotal > 0 ? p.bytesTotal : undefined;
  const downloaded = total != null && p.downloadedBytes != null ? Math.min(total, Math.max(0, p.downloadedBytes)) : undefined;
  const percentage = total != null && downloaded != null ? Math.floor(downloaded / total * 100) : null;
  const pending = p.pendingPackages ?? [];
  const packages = [...new Set([...(p.packages ?? []), ...pending])];
  const completedAt = p.phase === "done" ? (p.lastResult?.outcome === "done" ? p.lastResult.at : p.totalPackages ? p.updatedAt : null) : null;
  return (
    <Card title={t("Packages and updates")} actions={props.actions}>
      <div class="row"><Badge tone={tone(p)}>{progressLabel(p)}</Badge>{p.groupVersion && <span class="mono small">{p.groupVersion}</span>}</div>
      <div class="row small" style={{ gap: "20px", marginTop: "8px" }}>
        {(p.installedGroupVersion || !p.active && p.phase === "done" && p.groupVersion) && <span>{t("Installed group")}: <strong class="mono">{p.installedGroupVersion ?? p.groupVersion}</strong></span>}
        {p.availableGroupVersion && <span>{t("Available group")}: <span class="mono">{p.availableGroupVersion}</span></span>}
      </div>
      {p.currentPackage && p.active && <p class="mono small">{t("Current package")}: {p.currentPackage}</p>}
      {(p.active || percentage != null || p.totalPackages != null) && (
        <div style={{ marginTop: "12px" }}>
          <progress aria-label={t("Package download progress")} value={downloaded} max={total ?? 1} style={{ width: "100%" }} />
          <p class="small muted">
            {percentage != null ? `${percentage}% · ${bytes(downloaded!)} / ${bytes(total!)}` : t("Waiting for progress data")}
            {p.totalPackages != null && ` · ${t("{done}/{total} downloads verified", { done: p.completedPackages ?? 0, total: p.totalPackages })}`}
          </p>
          {p.attempt != null && p.maxAttempts != null && <p class="small muted">{t("Download attempt {attempt}/{maximum}", { attempt: p.attempt, maximum: p.maxAttempts })}</p>}
          {p.retryAtUnix != null && <p class="small muted">{t("Automatic download retry")}: {new Date(p.retryAtUnix * 1000).toLocaleString(getLocale())}</p>}
          {p.active && !["downloading", "retrying"].includes(p.phase) && <p class="small muted">{installPhaseLabel(p.phase)} · {t("Download completion does not mean installation is complete.")}</p>}
        </div>
      )}
      {p.error && <div class="callout bad" style={{ overflowWrap: "anywhere", marginTop: "12px" }}>{p.error}</div>}
      {p.active && p.retryError && <div class="callout warn" style={{ overflowWrap: "anywhere", marginTop: "12px" }}>{p.retryError}</div>}
      {completedAt && <p class="small muted">{t("Update completed at")}: {new Date(completedAt).toLocaleString(getLocale())}</p>}
      {p.lastResult && <p class="small muted">{t("Last update result")}: {p.lastResult.groupVersion && <span class="mono">{p.lastResult.groupVersion} · </span>}{resultLabel(p.lastResult.outcome)}{p.lastResult.detail ? ` · ${p.lastResult.detail}` : ""}</p>}
      {p.lastCheck && <p class="small muted">{t("Last update check")}: {new Date(p.lastCheck).toLocaleString(getLocale())}</p>}
      {!p.active && p.phase !== "paused" && p.nextCheckUnix != null && <p class="small muted">{t("Next automatic check")}: {new Date(p.nextCheckUnix * 1000).toLocaleString(getLocale())}</p>}
      {packages.length > 0 && <div class="row" style={{ marginTop: "12px" }}>{packages.map((pkg) => <Badge tone={pkg === p.currentPackage && p.active ? "info" : pending.includes(pkg) ? "warn" : "idle"} title={packageInstallLabel(p, pkg) ?? ""}>{pkg.replace(/^meduza-np-/, "")}{pkg === p.currentPackage && p.active ? ` · ${t("current")}` : ""}</Badge>)}</div>}
      {props.reportedAt && <p class="small faint">{t("Last node report")}: {new Date(props.reportedAt).toLocaleString(getLocale())}</p>}
      {props.children}
    </Card>
  );
}
