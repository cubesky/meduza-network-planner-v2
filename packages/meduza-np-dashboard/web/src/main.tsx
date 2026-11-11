import { render } from "preact";
import { useEffect, useState } from "preact/hooks";
import "@meduza-np/ui-kit/styles.css";
import "./app.css";
import { Button, Card, ThemeSelect, ToastHost, toast } from "@meduza-np/ui-kit";
import { errorText, get, post, setCsrf, type Session } from "./api.ts";
import { ConfigPage, DiagnosticsPage, FeaturesPage, InstancesPage, MihomoPanelCard, OverviewPage, PackagesPage, UpdatesPage } from "./pages.tsx";

const NAV: Array<[string, string]> = [
  ["/overview", "总览"],
  ["/features", "功能"],
  ["/instances", "实例"],
  ["/packages", "包与 provider"],
  ["/config", "配置"],
  ["/updates", "升级"],
  ["/diagnostics", "诊断与日志"],
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
      <Card title="Meduza NP 节点 dashboard">
        {props.session.managerLogin ? (
          <p>
            <Button variant="primary" onClick={() => location.assign("/auth/login")}>使用 manager 账号登录</Button>
          </p>
        ) : (
          <p class="muted">未配置 MEDUZA_NP_DASHBOARD_PUBLIC_URL，无法通过 manager 登录。</p>
        )}
        <p class="small muted">
          离线或 manager 不可达时，在节点上以 root 执行 <code>meduza-np dashboard-link</code>，打开输出的一次性链接（10 分钟内有效）。
        </p>
      </Card>
      <MihomoPanelCard />
    </div>
  );
}

function App() {
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
  if (err) return <div class="state-box">dashboard 服务不可达：{err}</div>;
  if (!s) return <div class="state-box">加载中…</div>;
  if (!s.authenticated) return <Login session={s} />;

  const page = hash.split("/")[1];
  const content =
    page === "features" ? <FeaturesPage /> :
    page === "instances" ? <InstancesPage session={s} /> :
    page === "packages" ? <PackagesPage /> :
    page === "config" ? <ConfigPage /> :
    page === "updates" ? <UpdatesPage session={s} /> :
    page === "diagnostics" ? <DiagnosticsPage /> :
    <OverviewPage session={s} />;

  return (
    <div class="layout">
      <nav class="nav">
        <div class="brand">Meduza NP 节点</div>
        {NAV.map(([href, label]) => (
          <a href={`#${href}`} class={hash.startsWith(href) ? "active" : ""}>{label}</a>
        ))}
        <div style={{ padding: "14px 10px" }} class="small muted">
          {s.user} · {s.role} {s.via === "local" ? "（本地链接）" : ""}
          <div style={{ margin: "8px 0" }}>
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
            }}>退出</Button>
          </div>
        </div>
      </nav>
      <main class="main">{content}</main>
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
