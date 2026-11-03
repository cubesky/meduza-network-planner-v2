// Node list and the separate "add node" flow (create → enrollment token → install instructions).

import { useState } from "preact/hooks";
import { Badge, Button, Card, CopyBlock, Field, Loadable, PageHead, Select, StatusDot, Table, TextInput, msg, relativeTime, t, toast, useAsync } from "@meduza-np/ui-kit";
import { errorText, get, post } from "../api.ts";
import { needsAttention, revisionBadge, syncCell, type NodeSummary } from "./Overview.tsx";
import { PackageUpdateSummary } from "@meduza-np/ui-kit/package-progress";

type Role = "admin" | "operator" | "viewer";

const SHOW = [
  ["all", msg("All nodes")],
  ["attention", msg("Need attention")],
  ["offline", msg("Offline")],
  ["pending", msg("Not at the desired revision")],
] as const;

/** Whether a node matches the search text (id, name, tags, behavior) and the chosen state filter. */
function matches(n: NodeSummary, q: string, show: string): boolean {
  if (show === "attention" && !needsAttention(n)) return false;
  if (show === "offline" && n.freshness !== "offline") return false;
  if (show === "pending" && (n.inSync || !n.desiredRevision)) return false;
  if (!q) return true;
  return [n.id, n.displayName, n.behavior, n.arch, ...n.tags].some((x) => x?.toLowerCase().includes(q));
}

export function NodesPage(props: { role: Role }) {
  const state = useAsync(() => get<NodeSummary[]>("/api/v1/nodes"), [], 15_000);
  const [query, setQuery] = useState("");
  const [show, setShow] = useState("all");
  return (
    <div>
      <PageHead
        title={t("Nodes")}
        sub={t("Open a node to see its status and edit its settings.")}
        actions={props.role !== "viewer" && <Button variant="primary" onClick={() => (location.hash = "/nodes/new")}>{t("Add node")}</Button>}
      />
      <Loadable state={state}>
        {(all) => {
          const nodes = all.filter((n) => matches(n, query.trim().toLowerCase(), show));
          return (
            <>
              {all.length > 0 && (
                <div class="filter-bar">
                  <input type="search" class="search" value={query} placeholder={t("Search by name, ID or tag")} aria-label={t("Search nodes")}
                    onInput={(e) => setQuery((e.target as HTMLInputElement).value)} />
                  <div class="seg" role="group" aria-label={t("Show")}>
                    {SHOW.map(([id, label]) => {
                      const n = all.filter((x) => matches(x, "", id)).length;
                      return (
                        <button type="button" class={show === id ? "active" : ""} aria-pressed={show === id} onClick={() => setShow(id)}>
                          {t(label)} <span class="faint">{n}</span>
                        </button>
                      );
                    })}
                  </div>
                </div>
              )}
              <Table
                rows={nodes as unknown as Record<string, unknown>[]}
                rowKey={(r) => String(r.id)}
                onSelect={(r) => (location.hash = `/nodes/${encodeURIComponent(String(r.id))}`)}
                empty={all.length ? t("No node matches.") : t("No nodes yet. Add one to enroll a machine.")}
                columns={[
                  {
                    key: "id",
                    label: t("Node"),
                    render: (r) => (
                      <span class="row" style={{ gap: "10px", flexWrap: "nowrap" }}>
                        <StatusDot state={String(r.freshness)} />
                        <span>
                          <strong>{String(r.displayName || r.id)}</strong>
                          {r.displayName && r.displayName !== r.id && <div class="faint small mono">{String(r.id)}</div>}
                        </span>
                      </span>
                    ),
                  },
                  { key: "behavior", label: t("Behavior"), render: (r) => t(String(r.behavior)) },
                  { key: "sync", label: t("Sync"), render: (r) => syncCell(r as unknown as NodeSummary) },
                  { key: "rev", label: t("Configuration"), render: (r) => revisionBadge(r as unknown as NodeSummary) },
                  { key: "update", label: t("Packages and updates"), render: (r) => <PackageUpdateSummary progress={(r as unknown as NodeSummary).update} /> },
                  {
                    key: "problems",
                    label: t("Problems"),
                    render: (r) => ((r.problems as unknown[]).length ? <Badge tone="bad">{(r.problems as unknown[]).length}</Badge> : <span class="faint">—</span>),
                  },
                  { key: "lastSeenAt", label: t("Last contact"), render: (r) => <span class="nowrap">{relativeTime(r.lastSeenAt as string | null)}</span> },
                ]}
              />
            </>
          );
        }}
      </Loadable>
    </div>
  );
}

/** Separate page for adding a node: create it, then show how to enroll the machine. */
export function AddNodePage(props: { role: Role }) {
  const [id, setId] = useState("");
  const [name, setName] = useState("");
  const [behavior, setBehavior] = useState("static");
  const [created, setCreated] = useState<string | null>(null);
  const [token, setToken] = useState<{ token: string; expiresAt: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const admin = props.role === "admin";

  const create = async () => {
    setBusy(true);
    try {
      await post("/api/v1/nodes", { id, displayName: name || undefined, behavior });
      setCreated(id);
      if (admin) setToken(await post(`/api/v1/nodes/${encodeURIComponent(id)}/enrollment-tokens`, { ttlSeconds: 86400 }));
    } catch (e) {
      toast(errorText(e), "bad");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div style={{ maxWidth: "760px" }}>
      <PageHead crumbs={<a href="#/nodes">{t("Nodes")}</a>} title={t("Add node")} sub={t("A node is one machine (router, gateway, server) managed by this console.")} />
      {!created ? (
        <Card>
          <div class="form-grid">
            <Field label={t("Node ID")} help={t("Stable id: lowercase letters, digits, '-' and '_'. Never derived from the display name.")}>
              <TextInput mono value={id} onInput={(v) => setId(v.trim().toLowerCase())} placeholder="gw-a" />
            </Field>
            <Field label={t("Display name")}>
              <TextInput value={name} onInput={setName} placeholder={t("e.g. Office gateway")} />
            </Field>
            <Field label={t("Behavior")} help={t("static: push channel; roaming: periodic pull. Declared, never guessed from IP changes.")}>
              <Select value={behavior} options={[{ value: "static", label: t("static") }, { value: "roaming", label: t("roaming") }]} onChange={setBehavior} />
            </Field>
          </div>
          <div class="row">
            <Button variant="primary" busy={busy} disabled={!id || props.role === "viewer"} onClick={create}>{t("Create node")}</Button>
            <Button onClick={() => (location.hash = "/nodes")}>{t("Cancel")}</Button>
          </div>
        </Card>
      ) : (
        <Card title={t("Enroll {id}", { id: created })}>
          {token ? (
            <>
              <p>{t("Single use, expires {time}. Start the node with these settings:", { time: new Date(token.expiresAt).toLocaleString() })}</p>
              <h3>{t("Container (Podman)")}</h3>
              <CopyBlock text={`# /etc/meduza-np/node.env
MEDUZA_NP_ENROLL_URL=${location.origin}
MEDUZA_NP_ENROLL_TOKEN=${token.token}

./run.sh --env-file /etc/meduza-np/node.env --network host`} />
              <h3>{t("Installed node")}</h3>
              <CopyBlock text={`meduza-np enroll --url ${location.origin} --token ${token.token}`} />
              <p class="small muted">
                {t("The node generates its own identity key; the token cannot be shown again. Features you configure on the node page are installed by its updater, or by hand where there is none.")}
              </p>
            </>
          ) : (
            <p class="muted">{t("The node exists. An administrator creates its enrollment token on the node page.")}</p>
          )}
          <Button variant="primary" onClick={() => (location.hash = `/nodes/${encodeURIComponent(created)}/settings`)}>{t("Configure {id}", { id: created })}</Button>
        </Card>
      )}
    </div>
  );
}
