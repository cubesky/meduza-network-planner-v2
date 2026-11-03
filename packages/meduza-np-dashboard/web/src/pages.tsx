import { useState } from "preact/hooks";
import { BinaryProgress, PackageProgress, progressFromStatus } from "@meduza-np/ui-kit/package-progress";
import { PackageResourceCard, type PackageResources } from "@meduza-np/ui-kit/package-resources";
import { Badge, Button, Card, Field, Loadable, StateBadge, Table, TextInput, NumberInput, Checkbox, bytes, relativeTime, toast, useAsync } from "@meduza-np/ui-kit";
import { errorText, get, post, roleAtLeast, type Feature, type PackageStatus, type Report, type Session } from "./api.ts";

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

export function OverviewPage(props: { session: Session }) {
  const st = useAsync(() => get<Report>("/api/status"), [], 5000);
  return (
    <Loadable state={st}>
      {(r) => (
        <div>
          <Card
            title={<>同步 <StateBadge state={r.sync.state} /></>}
            actions={roleAtLeast(props.session.role, "operator") && <Button small onClick={() => act("/api/sync", {}, "已请求同步")}>立即同步</Button>}
          >
            <dl class="kv">
              <dt>节点</dt><dd>{r.nodeId || "未注册"}</dd>
              <dt>通道</dt><dd>{r.sync.transport} · {r.sync.mode} · {r.sync.behavior}</dd>
              <dt>期望 / 已接收 / 已应用</dt>
              <dd>{r.sync.desiredRevision ?? "—"} / {r.sync.receivedRevision ?? "—"} / {r.sync.appliedRevision ?? "—"}</dd>
              <dt>最后成功</dt><dd>{r.sync.lastSuccessfulRevision ?? "—"}</dd>
              {r.sync.applyStage && (<><dt>应用阶段</dt><dd>{r.sync.applyStage}</dd></>)}
              {r.sync.degradedReason && (<><dt>降级原因</dt><dd><Badge tone="warn">{r.sync.degradedReason}</Badge></dd></>)}
              {r.sync.lastError && (<><dt>最近错误</dt><dd class="bad">{r.sync.lastError}</dd></>)}
              {r.sync.recoveryError && (<><dt>恢复历史配置</dt><dd class="bad">{r.sync.recoveryError}</dd></>)}
            </dl>
            {r.sync.backoff.failureLevel > 0 && (
              <p class="small">
                <Badge tone="warn">退避 L{r.sync.backoff.failureLevel}</Badge> 窗口 {r.sync.backoff.windowSeconds}s，
                {r.sync.backoff.nextAttemptInMs != null && <> 下次尝试 {Math.ceil(r.sync.backoff.nextAttemptInMs / 1000)}s 后，</>}
                {r.sync.backoff.serverCooldownMs ? <> 服务端冷却 {Math.ceil(r.sync.backoff.serverCooldownMs / 1000)}s，</> : null}
                {r.sync.backoff.probing ? " 恢复探测中，" : ""} 原因：{r.sync.backoff.lastFailure ?? "—"}
                <br />退避期间继续运行最后成功的配置。
              </p>
            )}
          </Card>
          <MaintenanceCard report={r} session={props.session} />
          <PackageProgress progress={r.update} reportedAt={r.observedAt} />
          {r.instances.filter((i) => i.kind === "mihomo-provider" && (i.mode === "auto-download" || i.downloadProgress)).map((i) => <BinaryProgress
            name="Mihomo" progress={i.downloadProgress} providerState={i.providerState} reason={i.reason}
            configurationError={r.sync.state === "apply-failed" ? r.sync.lastError : undefined}
          />)}
          <Card title="资源">
            <dl class="kv">
              <dt>planner RSS</dt><dd>{kib(r.resources?.plannerRssKib)}</dd>
              <dt>可用内存</dt><dd>{kib(r.resources?.memAvailableKib)} / {kib(r.resources?.memTotalKib)}</dd>
              <dt>状态盘剩余</dt><dd>{kib(r.resources?.stateDiskFreeKib)}</dd>
              <dt>负载</dt><dd>{r.resources?.load1Milli != null ? (r.resources.load1Milli / 1000).toFixed(2) : "—"}</dd>
              <dt>受监管进程</dt><dd>{r.resources?.supervisedProcesses ?? "—"}</dd>
              <dt>系统</dt><dd>{String(r.system?.os ?? "")} {String(r.system?.osRelease ?? "")} · {String(r.system?.arch ?? "")}{r.system?.container ? ` · 容器${r.system?.networkMode === "bridge" ? "（bridge 网络）" : r.system?.networkMode === "host" ? "（host 网络）" : ""}` : ""}{r.system?.openwrt ? " · OpenWrt" : ""}</dd>
              <dt>上报时间</dt><dd>{relativeTime(r.observedAt)}</dd>
            </dl>
          </Card>
          <PackageResourceCard packages={r.resources?.packages} reportedAt={r.observedAt} />
        </div>
      )}
    </Loadable>
  );
}

function MaintenanceCard(props: { report: Report; session: Session }) {
  const m = props.report.maintenance;
  const [reason, setReason] = useState("");
  const [minutes, setMinutes] = useState<number | undefined>(30);
  const [pause, setPause] = useState(true);
  const canAct = roleAtLeast(props.session.role, "operator");
  return (
    <Card title={<>维护模式 {m?.active ? <Badge tone="warn">进行中</Badge> : <Badge tone="idle">未启用</Badge>}</>}>
      {m?.active ? (
        <div>
          <p>{m.reason} · 发起者 {m.by} · 到期 {new Date(m.until).toLocaleString()} {m.applyPaused && <Badge tone="warn">暂停应用配置</Badge>}</p>
          {canAct && <Button onClick={() => act("/api/maintenance/stop", {}, "维护已结束")}>结束维护</Button>}
        </div>
      ) : canAct ? (
        <div class="form-row">
          <Field label="原因"><TextInput value={reason} onInput={setReason} placeholder="例如：更换上联" /></Field>
          <Field label="时长（分钟）"><NumberInput value={minutes} onInput={setMinutes} min={1} max={1440} /></Field>
          <Checkbox checked={pause} onChange={setPause} label="期间暂停自动应用配置" />
          <Button
            disabled={!reason.trim() || !minutes}
            onClick={() => act("/api/maintenance/start", { reason, durationSeconds: (minutes ?? 30) * 60, pauseApply: pause, stopInstances: [] }, "维护已开始")}
          >
            开始维护
          </Button>
        </div>
      ) : (
        <p class="muted">需要 operator 权限。</p>
      )}
    </Card>
  );
}

// ---- features / packages ----------------------------------------------------------------------------------------

export function FeaturesPage() {
  const st = useAsync(() => get<Feature[]>("/api/features"), [], 10000);
  return (
    <Card title="功能能力（仅显示本节点已安装的包）">
      <Loadable state={st} empty={(f) => f.length === 0}>
        {(f) => (
          <Table
            rowKey={(x) => String(x.id)}
            rows={f as never}
            columns={[
              { key: "id", label: "功能" },
              { key: "package", label: "包" },
              { key: "state", label: "状态", render: (x: Feature) => <StateBadge state={x.available ? (x.healthy === false ? "degraded" : "ok") : x.registered ? "idle" : "failed"} label={featureLabel(x)} /> },
              { key: "provider", label: "provider", render: (x: Feature) => x.provider ?? "—" },
              {
                key: "reason",
                label: "说明",
                render: (x: Feature) =>
                  x.reason ?? (x.limitationCode === "bridge-network" ? "仅在容器的 bridge 网络命名空间内生效：宿主/LAN 流量需路由进容器才会受影响" : x.limitation ?? ""),
              },
            ] as never}
          />
        )}
      </Loadable>
    </Card>
  );
}

function featureLabel(f: Feature): string {
  if (!f.registered) return "注册失败";
  if (!f.available) return "缺依赖";
  if (!f.enabled) return "未启用";
  if (f.running === false) return "未运行";
  if (f.healthy === false) return "异常";
  return f.limitationCode ? "正常（受限）" : "正常";
}

export function PackagesPage() {
  const st = useAsync(() => get<{ packages: PackageStatus[]; resources?: PackageResources[]; providers?: Record<string, { path: string; source: string; version?: string }>; providerErrors?: Record<string, string> }>("/api/packages"), [], 15000);
  return (
    <Loadable state={st}>
      {(p) => (
        <div>
          <Card title="已安装的包">
            <Table
              rowKey={(x) => String(x.name)}
              rows={p.packages as never}
              columns={[
                { key: "name", label: "包" },
                { key: "version", label: "版本" },
                { key: "role", label: "角色" },
                { key: "registration", label: "注册", render: (x: PackageStatus) => <StateBadge state={x.registration === "registered" ? "ok" : "failed"} label={x.registration} /> },
                { key: "reason", label: "说明", render: (x: PackageStatus) => x.reason ?? "" },
              ] as never}
            />
          </Card>
          <Card title="软件本体 provider">
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
      <Card title="实例">
        <Loadable state={procs} empty={(p) => Object.keys(p.applied ?? {}).length === 0}>
          {(p) => (
            <Table
              rowKey={(x) => String(x.id)}
              rows={Object.entries(p.applied ?? {}).map(([id, a]) => ({ id, package: a.package, processes: a.processes.join(", ") })) as never}
              columns={[
                { key: "id", label: "实例" },
                { key: "package", label: "包" },
                { key: "processes", label: "进程" },
                {
                  key: "actions",
                  label: "",
                  render: (x: { id: string }) =>
                    canAct && (
                      <span style={{ display: "flex", gap: "4px" }}>
                        <Button small onClick={() => act("/api/instances/restart", { instance: x.id }, `${x.id} 已重启`, procs.reload)}>重启</Button>
                        <Button small disabled={!maintenance} title={maintenance ? "" : "先开始维护，否则自动 reconcile 会恢复"} onClick={() => act("/api/instances/stop", { instance: x.id }, `${x.id} 已停止`, procs.reload)}>停止</Button>
                        <Button small onClick={() => act("/api/instances/start", { instance: x.id }, `${x.id} 已启动`, procs.reload)}>启动</Button>
                      </span>
                    ),
                },
              ] as never}
            />
          )}
        </Loadable>
      </Card>
      <Card title="进程">
        <Loadable state={procs}>
          {(p) => (
            <Table
              rowKey={(x) => String(x.id)}
              rows={p.processes as never}
              columns={[
                { key: "id", label: "进程" },
                { key: "state", label: "状态", render: (x: Proc) => <StateBadge state={x.state} /> },
                { key: "pid", label: "PID" },
                { key: "restarts", label: "重启次数" },
                { key: "lastExit", label: "最近退出" },
              ] as never}
            />
          )}
        </Loadable>
      </Card>
      <Card title="状态 / 邻居">
        <Loadable state={report} empty={(r) => r.instances.length === 0}>
          {(r) => (
            <Table
              rowKey={(x) => String(x.id)}
              rows={r.instances as never}
              columns={[
                { key: "id", label: "对象" },
                { key: "kind", label: "类型" },
                { key: "state", label: "状态", render: (x: { state: string }) => <StateBadge state={x.state} /> },
                { key: "reason", label: "说明" },
              ] as never}
            />
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
    <Card title="最后成功的配置（只读，秘密已隐藏）">
      <p class="small muted">配置变更请在 manager 中编辑草稿并发布；本地不建立另一套主配置。</p>
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

export function UpdatesPage(props: { session: Session }) {
  const st = useAsync(() => get<Record<string, any>>("/api/update/status"), [], 3000);
  const op = roleAtLeast(props.session.role, "operator");
  const admin = roleAtLeast(props.session.role, "admin");
  return (
    <Loadable state={st}>
      {(u) =>
        u.available === false ? (
          <Card title="升级"><p>本节点未安装 meduza-np-updater，不提供自动升级。</p></Card>
        ) : (
          <div>
            <PackageProgress progress={progressFromStatus(u)} />
            <Card
              title={<>升级 {u.busy ? <Badge tone="info">{u.busy}</Badge> : null} {u.control?.paused ? <Badge tone="warn">已暂停</Badge> : null}</>}
              actions={
                <span style={{ display: "flex", gap: "6px" }}>
                  {op && <Button small disabled={!!u.busy} onClick={() => act("/api/update/check", {}, "已开始检查", st.reload)}>检查</Button>}
                  {admin && <Button small variant="primary" disabled={!!u.busy} onClick={() => act("/api/update/apply", {}, "已开始升级", st.reload)}>{u.progress?.phase === "failed" || u.state?.lastResult?.outcome === "failed" ? "重试安装" : "立即升级"}</Button>}
                  {op && (u.control?.paused
                    ? <Button small onClick={() => act("/api/update/resume", {}, "已恢复", st.reload)}>恢复</Button>
                    : <Button small onClick={() => act("/api/update/pause", {}, "已暂停", st.reload)}>暂停</Button>)}
                  {op && <Button small onClick={() => act("/api/update/defer", { seconds: 86400 }, "已延迟 24 小时", st.reload)}>延迟一天</Button>}
                </span>
              }
            >
              <dl class="kv">
                <dt>通道</dt><dd>{u.config?.channel}{u.config?.pinnedGroupVersion ? ` · 固定 ${u.config.pinnedGroupVersion}` : ""}</dd>
                <dt>已安装包组</dt><dd>{u.installed?.groupVersion ?? "—"} (第 {u.installed?.generation ?? 0} 代)</dd>
                <dt>可用包组</dt><dd>{u.state?.available?.groupVersion ?? "—"}</dd>
                <dt>上次检查</dt><dd>{relativeTime(u.state?.lastCheck)} {u.state?.lastCheckError && <span class="bad">{u.state.lastCheckError}</span>}</dd>
                <dt>上次结果</dt><dd>{u.state?.lastResult ? `${u.state.lastResult.groupVersion} ${u.state.lastResult.outcome} ${u.state.lastResult.detail ?? ""}` : "—"}</dd>
                {u.journal && (<><dt>进行中</dt><dd><Badge tone="info">{u.journal.groupVersion} · {u.journal.phase}</Badge></dd></>)}
              </dl>
            </Card>
            <Card title="已安装的包版本">
              <Table
                rowKey={(x) => String(x.name)}
                rows={Object.entries(u.installed?.packages ?? {}).map(([name, p]: [string, any]) => ({ name, version: p.version, source: p.source })) as never}
                columns={[{ key: "name", label: "包" }, { key: "version", label: "版本" }, { key: "source", label: "来源" }] as never}
              />
            </Card>
            {u.lastPlan && (
              <Card title={`计划：${u.lastPlan.groupVersion}`}>
                {u.lastPlan.blocked?.length > 0 && <p class="bad">阻塞：{u.lastPlan.blocked.join("；")}</p>}
                <Table
                  rowKey={(x) => String(x.package)}
                  rows={u.lastPlan.actions as never}
                  columns={[{ key: "package", label: "包" }, { key: "kind", label: "动作" }, { key: "from", label: "当前" }, { key: "to", label: "目标" }] as never}
                />
              </Card>
            )}
          </div>
        )
      }
    </Loadable>
  );
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
    const cut = Array.isArray(b.truncated) ? `（已缩减：${(b.truncated as string[]).join("、")}）` : "";
    toast(`支持包已导出，凭据已脱敏${cut}`, "ok");
  } catch (e) {
    toast(`导出失败：${errorText(e)}`, "bad");
  }
}

export function DiagnosticsPage() {
  const doctor = useAsync(() => get<Array<{ check: string; ok: boolean; detail?: string }>>("/api/doctor"), []);
  const logs = useAsync(() => get<string[]>("/api/logs?lines=300"), [], 5000);
  return (
    <div>
      <Card
        title="诊断"
        actions={
          <div class="row">
            <Button small onClick={() => void exportBundle()}>导出支持包</Button>
            <Button small onClick={doctor.reload}>重新检查</Button>
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
      <Card title="planner 日志">
        <Loadable state={logs}>{(l) => <pre class="code">{(Array.isArray(l) ? l : []).join("\n")}</pre>}</Loadable>
      </Card>
    </div>
  );
}
