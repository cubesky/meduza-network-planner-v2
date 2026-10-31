import { render } from "preact";
import { useEffect, useState } from "preact/hooks";
import "@meduza-np/ui-kit/styles.css";
import { Button, ThemeSelect, ToastHost, msg, setLocale, t, toast, useLocale } from "@meduza-np/ui-kit";
import { errorText, post, saveLocale, session, type Session } from "./api.ts";
import { LanguageSelect, effectiveLocale } from "./language.tsx";
import { ChangesButton, ChangesDialog, FormBar } from "./ApplyBar.tsx";
import "./messages.ts";
import { LoginPage } from "./pages/Login.tsx";
import { OverviewPage } from "./pages/Overview.tsx";
import { AddNodePage, NodesPage } from "./pages/Nodes.tsx";
import { NodePage } from "./pages/Node.tsx";
import { SettingsPage } from "./pages/Settings.tsx";
import { HistoryPage } from "./pages/History.tsx";
import { TopologyPage } from "./pages/Topology.tsx";
import { AccessPage } from "./pages/Access.tsx";
import { AccountsPage } from "./pages/Accounts.tsx";
import { AuditPage } from "./pages/Audit.tsx";
import { ImportPage } from "./pages/Import.tsx";
import { DashboardLoginPage } from "./pages/DashboardLogin.tsx";

function useHash(): string {
  const [hash, setHash] = useState(location.hash.slice(1) || "/overview");
  useEffect(() => {
    const on = () => setHash(location.hash.slice(1) || "/overview");
    addEventListener("hashchange", on);
    return () => removeEventListener("hashchange", on);
  }, []);
  return hash;
}

// Labels are English message keys, translated at render time; `null` starts a titled group.
const NAV: Array<[string, string] | [null, string]> = [
  ["/overview", msg("Overview")],
  ["/nodes", msg("Nodes")],
  [null, msg("Network")],
  ["/topology", msg("Topology")],
  ["/settings", msg("Global settings")],
  ["/history", msg("History")],
  [null, msg("Administration")],
  ["/access", msg("Access identities")],
  ["/accounts", msg("Accounts & passkeys")],
  ["/audit", msg("Audit")],
  ["/import", msg("Import")],
];

setLocale(effectiveLocale());

function App() {
  useLocale(); // re-render everything when the language changes
  const [s, setS] = useState<Session | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [changesOpen, setChangesOpen] = useState(false);
  const hash = useHash();
  const refresh = () =>
    session().then((next) => {
      if (next.authenticated) setLocale(effectiveLocale(next.user?.settings?.locale));
      setS(next);
    }, (e) => setErr(errorText(e)));
  useEffect(() => void refresh(), []);

  if (err) return <div class="state-box">{t("Manager unreachable: {error}", { error: err })}</div>;
  if (!s) return <div class="state-box">{t("Loading…")}</div>;
  if (!s.authenticated || hash.startsWith("/invite/")) return <LoginPage session={s} hash={hash} onSignedIn={refresh} />;

  const role = s.user!.role;
  const [, page, arg, sub] = hash.split("/");
  let content;
  switch (page) {
    case "nodes":
      content = arg === "new" ? <AddNodePage role={role} /> : arg ? <NodePage id={decodeURIComponent(arg)} tab={sub} role={role} /> : <NodesPage role={role} />;
      break;
    case "settings":
      content = <SettingsPage role={role} tab={arg} />;
      break;
    case "topology":
      content = <TopologyPage />;
      break;
    case "history":
      content = <HistoryPage role={role} />;
      break;
    case "access":
      content = <AccessPage role={role} />;
      break;
    case "accounts":
      content = <AccountsPage session={s} />;
      break;
    case "audit":
      content = <AuditPage />;
      break;
    case "import":
      content = <ImportPage role={role} />;
      break;
    case "dashboard-login":
      content = <DashboardLoginPage id={decodeURIComponent(arg ?? "")} role={role} />;
      break;
    default:
      content = <OverviewPage />;
  }
  return (
    <div class="layout">
      <nav class="nav">
        <div class="brand">Meduza NP</div>
        {NAV.map(([href, label]) =>
          href === null ? (
            <div class="section">{t(label)}</div>
          ) : (
            <a href={`#${href}`} class={hash.startsWith(href) ? "active" : ""}>
              {t(label)}
            </a>
          ),
        )}
        <div class="account small muted">
          <span>{s.user!.displayName} · {t(role)}</span>
          <LanguageSelect account={s.user!.settings?.locale ?? null} onSave={async (l) => {
            await saveLocale(l);
            await refresh();
          }} />
          <ThemeSelect />
          <Button small onClick={async () => {
            try {
              await post("/api/v1/auth/logout");
            } catch (e) {
              toast(errorText(e), "bad");
            }
            location.hash = "";
            void refresh();
          }}>{t("Sign out")}</Button>
        </div>
      </nav>
      <main class="main">
        <div class="topstrip">
          <ChangesButton onOpen={() => setChangesOpen(true)} />
        </div>
        {content}
      </main>
      <FormBar editable={role !== "viewer"} onReview={() => setChangesOpen(true)} />
      {changesOpen && <ChangesDialog editable={role !== "viewer"} onClose={() => setChangesOpen(false)} />}
    </div>
  );
}

render(
  <>
    <App />
    <ToastHost />
  </>,
  document.getElementById("app")!,
);
