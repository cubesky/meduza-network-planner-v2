import { render } from "preact";
import { useEffect, useState } from "preact/hooks";
import "@meduza-np/ui-kit/styles.css";
import "./app.css";
import { Button, Card, ThemeSelect, ToastHost, msg, setLocale, t, toast, useAsync, useLocale } from "@meduza-np/ui-kit";
import { UpdateNotice } from "@meduza-np/ui-kit/package-progress";
import { errorText, get, post, setCsrf, type Report, type Session } from "./api.ts";
import { LanguageSelect, effectiveLocale } from "./language.tsx";
import "./messages.ts";
import { ConfigPage, DiagnosticsPage, FeaturesPage, InstancesPage, MihomoPanelCard, OverviewPage, PackagesPage, UpdatesPage } from "./pages.tsx";

const NAV: Array<[string, string]> = [
  ["/overview", msg("Overview")],
  ["/features", msg("Features")],
  ["/instances", msg("Instances")],
  ["/packages", msg("Packages and providers")],
  ["/config", msg("Configuration")],
  ["/updates", msg("Updates and downloads")],
  ["/diagnostics", msg("Diagnostics and logs")],
];

function useHash(): string {
  const [h, setH] = useState(location.hash.slice(1) || "/overview");
  useEffect(() => {
    const on = () => setH(location.hash.slice(1) || "/overview");
    addEventListener("hashchange", on);
    return () => removeEventListener("hashchange", on);
  }, []);
  return h;
}

function Login(props: { session: Session }) {
  return (
    <div class="center">
      <div class="row" style={{ justifyContent: "end", marginBottom: "12px" }}><LanguageSelect /><ThemeSelect /></div>
      <Card title={t("Meduza NP node dashboard")}>
        {props.session.managerLogin ? (
          <p>
            <Button variant="primary" onClick={() => location.assign("/auth/login")}>{t("Sign in with Manager")}</Button>
          </p>
        ) : (
          <p class="muted">{t("Manager sign-in is unavailable because MEDUZA_NP_DASHBOARD_PUBLIC_URL is not configured.")}</p>
        )}
        <p class="small muted">
          {t("If Manager is unreachable, run the following command as root on the node and open its one-time link (valid for 10 minutes).")}
          <br /><code>meduza-np dashboard-link</code>
        </p>
      </Card>
      <MihomoPanelCard />
    </div>
  );
}

function App() {
  const locale = useLocale();
  useEffect(() => { document.title = t("Meduza NP node"); }, [locale]);
  const [s, setS] = useState<Session | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const hash = useHash();
  const refresh = () =>
    get<Session>("/api/session").then(
      (x) => {
        setCsrf(x.csrf ?? null);
        setS(x);
      },
      (e) => setErr(errorText(e)),
    );
  useEffect(() => void refresh(), []);
  if (err) return <div class="state-box">{t("Dashboard unreachable: {error}", { error: err })}</div>;
  if (!s) return <div class="state-box">{t("Loading…")}</div>;
  if (!s.authenticated) return <Login session={s} />;

  return <Authenticated session={s} hash={hash} refresh={refresh} />;
}

function Authenticated({ session: s, hash, refresh }: { session: Session; hash: string; refresh: () => void }) {
  const report = useAsync(() => get<Report>("/api/status"), [], 5000);

  const page = hash.split("/")[1];
  const content =
    page === "features" ? <FeaturesPage /> :
    page === "instances" ? <InstancesPage session={s} /> :
    page === "packages" ? <PackagesPage /> :
    page === "config" ? <ConfigPage /> :
    page === "updates" ? <UpdatesPage session={s} status={report} /> :
    page === "diagnostics" ? <DiagnosticsPage /> :
    <OverviewPage session={s} status={report} />;

  return (
    <div class="layout">
      <nav class="nav">
        <div class="brand">{t("Meduza NP node")}</div>
        {NAV.map(([href, label]) => (
          <a href={`#${href}`} class={hash.startsWith(href) ? "active" : ""}>{t(label)}</a>
        ))}
        <div style={{ padding: "14px 10px" }} class="small muted">
          {s.user} · {t(s.role ?? "viewer")} {s.via === "local" ? t("(local link)") : ""}
          <div style={{ margin: "8px 0", display: "grid", gap: "8px" }}>
            <LanguageSelect />
            <ThemeSelect />
          </div>
          <div>
            <Button small onClick={async () => {
              try {
                await post("/auth/logout");
              } catch (e) {
                toast(errorText(e), "bad");
              }
              void refresh();
            }}>{t("Sign out")}</Button>
          </div>
        </div>
      </nav>
      <main class="main">
        {page !== "updates" && <UpdateNotice progress={report.data?.update}
          binaries={(report.data?.instances ?? []).filter((i) => i.kind === "mihomo-provider").map((i) => ({ name: "Mihomo", progress: i.downloadProgress }))}
          href="#/updates" />}
        {content}
      </main>
    </div>
  );
}

setLocale(effectiveLocale());

render(
  <>
    <App />
    <ToastHost />
  </>,
  document.getElementById("app")!,
);
