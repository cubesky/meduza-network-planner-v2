// OpenVPN Access identity (AGENT.md §6.5): OpenLDAP connection, search/group mapping tests, authorization
// policies, online sessions with disconnect (D39) and recent authentication results. LDAP never grants manager roles.

import { useEffect, useState } from "preact/hooks";
import { Badge, Button, Card, Field, ListInput, Loadable, NumberInput, Select, StateBadge, Table, TextInput, ToggleField, ask, bytes, relativeTime, t, toast, useAsync } from "@meduza-np/ui-kit";
import { del, errorText, get, post, put } from "../api.ts";

type Role = "admin" | "operator" | "viewer";

interface Ldap {
  url: string;
  startTls: boolean;
  baseDn: string;
  bindDn: string;
  bindPasswordRef: string;
  userFilter: string;
  groupBaseDn?: string;
  groupFilter?: string;
  groupNameAttribute: string;
  useMemberOf: boolean;
  caPem?: string;
  verifyCertificate: boolean;
  connectTimeoutMs: number;
  timeoutMs: number;
}

interface Session {
  nodeId: string;
  instanceId: string;
  username: string;
  commonName: string;
  realAddress: string;
  virtualAddress: string;
  rxBytes: number;
  txBytes: number;
  connectedSince: string;
  clientId?: number;
  reportedAt: string;
  disconnecting?: boolean;
}

interface Policy {
  id: string;
  name: string;
  enabled: boolean;
  groups: string[];
  nodes: string[];
  instances: string[];
  sessionSeconds: number;
  priority: number;
}

const EMPTY: Ldap = {
  url: "ldaps://ldap.example.test",
  startTls: false,
  baseDn: "",
  bindDn: "",
  bindPasswordRef: "ldap-bind-password",
  userFilter: "(&(objectClass=inetOrgPerson)(uid={username}))",
  groupBaseDn: "",
  groupFilter: "(&(objectClass=groupOfNames)(member={dn}))",
  groupNameAttribute: "cn",
  useMemberOf: false,
  verifyCertificate: true,
  connectTimeoutMs: 3000,
  timeoutMs: 5000,
};

export function AccessPage(props: { role: Role }) {
  const admin = props.role === "admin";
  return (
    <div>
      <div class="topbar">
        <h1>{t("Access identities")}</h1>
      </div>
      <p class="muted small">{t("Used only to authenticate OpenVPN Access users. Manager logins always use passkeys.")}</p>
      {admin ? <LdapSettings /> : <Card>{t("LDAP settings are visible to administrators only.")}</Card>}
      <Policies admin={admin} />
      {props.role !== "viewer" && <Sessions />}
      {props.role !== "viewer" && <AuthLog />}
    </div>
  );
}

function LdapSettings() {
  const state = useAsync(() => get<Ldap | null>("/api/v1/access/ldap"), []);
  const [s, setS] = useState<Ldap | null>(null);
  const [testUser, setTestUser] = useState("");
  const [password, setPassword] = useState("");
  const [result, setResult] = useState<string | null>(null);
  useEffect(() => {
    if (state.data !== undefined) setS(state.data ?? EMPTY);
  }, [state.data]);
  if (!s) return <Loadable state={state}>{() => null}</Loadable>;
  const set = <K extends keyof Ldap>(k: K, v: Ldap[K]) => setS({ ...s, [k]: v });
  return (
    <Card
      title={t("OpenLDAP connection")}
      actions={
        <div class="row">
          <Button onClick={async () => {
            try {
              const r = await post<{ ok: boolean; message: string; userDn?: string; groups?: string[] }>("/api/v1/access/ldap/test", { username: testUser || undefined });
              setResult(`${r.ok ? t("OK") : t("FAILED")}: ${r.message}${r.userDn ? ` — ${r.userDn}` : ""}${r.groups ? ` — ${t("groups: {list}", { list: r.groups.join(", ") })}` : ""}`);
            } catch (e) {
              setResult(errorText(e));
            }
          }}>{testUser ? t("Test user lookup") : t("Test connection")}</Button>
          <Button variant="primary" onClick={async () => {
            try {
              // The bind password is a manager-held secret; typing one stores (or replaces) it.
              if (password) {
                await post("/api/v1/secrets", { id: s.bindPasswordRef || "ldap-bind-password", kind: "ldap-bind-password", value: password, nodeScope: [] });
                setPassword("");
              }
              await put("/api/v1/access/ldap", { ...s, bindPasswordRef: s.bindPasswordRef || "ldap-bind-password" });
              toast(t("LDAP settings saved"), "ok");
            } catch (e) {
              toast(errorText(e), "bad");
            }
          }}>{t("Save")}</Button>
        </div>
      }
    >
      <div class="form-grid">
        <Field label={t("URL")} help={t("ldaps:// or ldap:// with StartTLS. Passwords are never sent in clear text.")}><TextInput value={s.url} onInput={(v) => set("url", v)} /></Field>
        <Field label={t("Base DN")}><TextInput value={s.baseDn} onInput={(v) => set("baseDn", v)} /></Field>
        <Field label={t("Service bind DN")}><TextInput value={s.bindDn} onInput={(v) => set("bindDn", v)} /></Field>
        <Field label={t("Bind password")} help={t("Stored encrypted on the manager. Leave empty to keep the current password.")}>
          <input type="password" value={password} autoComplete="new-password" placeholder="••••••••" onInput={(e) => setPassword((e.target as HTMLInputElement).value)} />
        </Field>
        <Field label={t("User filter")} help={t("{username} is RFC 4515 escaped; exactly one entry must match.")}><TextInput value={s.userFilter} onInput={(v) => set("userFilter", v)} /></Field>
        {!s.useMemberOf && <Field label={t("Group base DN")}><TextInput value={s.groupBaseDn} onInput={(v) => set("groupBaseDn", v)} /></Field>}
        {!s.useMemberOf && <Field label={t("Group filter")} help={t("{dn} and {username} are escaped.")}><TextInput value={s.groupFilter} onInput={(v) => set("groupFilter", v)} /></Field>}
        <Field label={t("Group name attribute")}><TextInput value={s.groupNameAttribute} onInput={(v) => set("groupNameAttribute", v)} /></Field>
        <Field label={t("Connect timeout (ms)")}><NumberInput value={s.connectTimeoutMs} onInput={(v) => set("connectTimeoutMs", v ?? 3000)} /></Field>
        <Field label={t("Operation timeout (ms)")}><NumberInput value={s.timeoutMs} onInput={(v) => set("timeoutMs", v ?? 5000)} /></Field>
      </div>
      <div class="toggle-grid">
        <ToggleField label={t("StartTLS")} checked={s.startTls} onChange={(v) => set("startTls", v)} />
        <ToggleField label={<>{t("Verify certificate")} {!s.verifyCertificate && <Badge tone="bad">{t("insecure")}</Badge>}</>} checked={s.verifyCertificate} onChange={(v) => set("verifyCertificate", v)} />
        <ToggleField label={t("Use memberOf")} checked={s.useMemberOf} onChange={(v) => set("useMemberOf", v)} />
      </div>
      <Field label={t("CA certificate (PEM, optional)")}>
        <textarea value={s.caPem ?? ""} onInput={(e) => set("caPem", (e.target as HTMLTextAreaElement).value || undefined)} />
      </Field>
      <div class="row">
        <TextInput value={testUser} onInput={setTestUser} placeholder={t("username to look up (optional)")} />
      </div>
      {result && <pre>{result}</pre>}
    </Card>
  );
}

function Policies(props: { admin: boolean }) {
  const state = useAsync(() => get<Policy[]>("/api/v1/access/policies"), []);
  const [edit, setEdit] = useState<Policy | null>(null);
  return (
    <Card title={t("Authorization policies")} actions={props.admin && <Button onClick={() => setEdit({ id: "", name: "", enabled: true, groups: [], nodes: [], instances: [], sessionSeconds: 3600, priority: 100 })}>{t("Add policy")}</Button>}>
      <p class="small muted">{t("The first enabled policy (by priority) whose group matches and whose node/instance scope includes the request allows the login.")}</p>
      <Loadable state={state} empty={(p) => !p.length} emptyText={t("No policies: every Access login is denied (not-authorized).")}>
        {(rows) => (
          <Table
            rows={rows as unknown as Record<string, unknown>[]}
            rowKey={(r) => String(r.id)}
            onSelect={props.admin ? (r) => setEdit(r as unknown as Policy) : undefined}
            columns={[
              { key: "priority", label: t("Prio") },
              { key: "id", label: "ID" },
              { key: "name", label: t("Name") },
              { key: "enabled", label: t("Enabled") },
              { key: "groups", label: t("LDAP groups") },
              { key: "nodes", label: t("Nodes"), render: (r) => ((r.nodes as string[]).length ? (r.nodes as string[]).join(", ") : t("all")) },
              { key: "instances", label: t("Instances"), render: (r) => ((r.instances as string[]).length ? (r.instances as string[]).join(", ") : t("all")) },
              { key: "sessionSeconds", label: t("Session (s)") },
            ]}
          />
        )}
      </Loadable>
      {edit && (
        <div class="card">
          <div class="form-grid">
            <Field label="ID"><TextInput value={edit.id} onInput={(v) => setEdit({ ...edit, id: v })} /></Field>
            <Field label={t("Name")}><TextInput value={edit.name} onInput={(v) => setEdit({ ...edit, name: v })} /></Field>
            <Field label={t("Priority")}><NumberInput value={edit.priority} onInput={(v) => setEdit({ ...edit, priority: v ?? 100 })} /></Field>
            <Field label={t("Session seconds")}><NumberInput value={edit.sessionSeconds} onInput={(v) => setEdit({ ...edit, sessionSeconds: v ?? 3600 })} /></Field>
          </div>
          <div class="toggle-grid">
            <ToggleField label={t("Enabled")} checked={edit.enabled} onChange={(v) => setEdit({ ...edit, enabled: v })} />
          </div>
          <div class="form-grid">
            <Field label={t("LDAP groups (name or DN)")}><ListInput value={edit.groups} onChange={(v) => setEdit({ ...edit, groups: v })} /></Field>
            <Field label={t("Nodes (empty = all)")}><ListInput value={edit.nodes} onChange={(v) => setEdit({ ...edit, nodes: v })} /></Field>
            <Field label={t("Access instances (empty = all)")}><ListInput value={edit.instances} onChange={(v) => setEdit({ ...edit, instances: v })} /></Field>
          </div>
          <div class="row">
            <Button variant="primary" disabled={!edit.id || !edit.groups.length} onClick={async () => {
              try {
                await put(`/api/v1/access/policies/${encodeURIComponent(edit.id)}`, edit);
                setEdit(null);
                state.reload();
              } catch (e) {
                toast(errorText(e), "bad");
              }
            }}>{t("Save policy")}</Button>
            {state.data?.some((p) => p.id === edit.id) && (
              <Button variant="danger" onClick={async () => {
                await del(`/api/v1/access/policies/${encodeURIComponent(edit.id)}`);
                setEdit(null);
                state.reload();
              }}>{t("Delete")}</Button>
            )}
            <Button onClick={() => setEdit(null)}>{t("Cancel")}</Button>
          </div>
        </div>
      )}
    </Card>
  );
}

function Sessions() {
  const state = useAsync(() => get<Session[]>("/api/v1/access/sessions"), [], 15_000);
  const [ending, setEnding] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const disconnect = async (r: Session) => {
    if (!(await ask(t("Disconnect {user} from {node}/{instance}? Their token stops working there; they can log in again with their password.", { user: r.username, node: r.nodeId, instance: r.instanceId }), { ok: t("Disconnect"), danger: true }))) return;
    const key = `${r.nodeId}/${r.instanceId}/${r.username}`;
    setEnding(key);
    try {
      await post<{ jobId: string }>("/api/v1/access/sessions/disconnect", { node: r.nodeId, instance: r.instanceId, username: r.username });
      toast(t("Disconnect sent to {node}; the list updates with its next report.", { node: r.nodeId }), "ok");
      setTimeout(() => state.reload(), 5_000);
    } catch (e) {
      toast(errorText(e), "bad");
    } finally {
      setEnding(null);
    }
  };
  const q = filter.trim().toLowerCase();
  return (
    <Card title={t("Online Access sessions")} actions={<TextInput value={filter} onInput={setFilter} placeholder={t("filter by user, node or address")} />}>
      <p class="small muted">{t("From the nodes' latest reports. Disconnecting ends every session of the user on that instance; to keep someone out, remove them from the granting LDAP group or policy.")}</p>
      <Loadable state={state} empty={(r) => !r.length} emptyText={t("No Access clients are connected")}>
        {(rows) => (
          <Table
            rows={(rows as unknown as Array<Session & Record<string, unknown>>).filter((r) => !q || [r.username, r.nodeId, r.instanceId, r.realAddress, r.virtualAddress].some((v) => v.toLowerCase().includes(q)))}
            rowKey={(r) => `${r.nodeId}/${r.instanceId}/${r.username}/${r.clientId ?? r.realAddress}`}
            columns={[
              { key: "username", label: t("User") },
              { key: "nodeId", label: t("Node") },
              { key: "instanceId", label: t("Instance") },
              { key: "virtualAddress", label: t("VPN address") },
              { key: "realAddress", label: t("From") },
              { key: "connectedSince", label: t("Connected"), render: (r) => relativeTime(r.connectedSince) },
              { key: "rxBytes", label: t("Traffic"), render: (r) => `↓ ${bytes(r.rxBytes)} ↑ ${bytes(r.txBytes)}` },
              { key: "reportedAt", label: t("Reported"), render: (r) => relativeTime(r.reportedAt) },
              {
                key: "actions",
                label: "",
                render: (r) =>
                  r.disconnecting ? (
                    <Badge tone="warn" title={t("Waiting for the node's next report")}>{t("disconnecting")}</Badge>
                  ) : (
                    <Button small variant="danger" busy={ending === `${r.nodeId}/${r.instanceId}/${r.username}`} onClick={(e) => { e.stopPropagation(); void disconnect(r); }}>
                      {t("Disconnect")}
                    </Button>
                  ),
              },
            ]}
          />
        )}
      </Loadable>
    </Card>
  );
}

function AuthLog() {
  const state = useAsync(() => get<Array<Record<string, unknown>>>("/api/v1/access/log"), [], 15_000);
  const [filter, setFilter] = useState("all");
  return (
    <Card title={t("Recent Access authentications")} actions={<Select value={filter} options={["all", "allow", "deny", "end"].map((v) => ({ value: v, label: t(v) }))} onChange={setFilter} />}>
      <Loadable state={state} empty={(r) => !r.length} emptyText={t("No authentication attempts yet")}>
        {(rows) => (
          <Table
            rows={rows.filter((r) => filter === "all" || r.decision === filter)}
            rowKey={(r) => String(r.id)}
            columns={[
              { key: "at", label: t("When"), render: (r) => relativeTime(String(r.at)) },
              { key: "node_id", label: t("Node") },
              { key: "instance_id", label: t("Instance") },
              { key: "username", label: t("User") },
              { key: "decision", label: t("Decision"), render: (r) => <StateBadge state={r.decision === "allow" ? "up" : r.decision === "end" ? "unknown" : "down"} label={t(String(r.decision))} /> },
              { key: "reason_code", label: t("Reason") },
              { key: "duration_ms", label: "ms" },
            ]}
          />
        )}
      </Loadable>
    </Card>
  );
}
