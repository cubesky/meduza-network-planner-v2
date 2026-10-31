// Import of the previous service planner's etcd export (D58). The file is read in the browser and sent to the
// manager, which converts it and returns what it found: nodes, the values the export carries redacted (keys,
// certificates, subscription URLs, WireGuard peer keys) and what is not taken over. The operator supplies what they
// have; a tunnel whose value is still missing is imported switched off. Staging creates the nodes, stores the
// secrets, asks the nodes to generate their Tinc keys and puts the document into the account's saved changes, to be
// reviewed and applied like any other change.

import { useState } from "preact/hooks";
import { Badge, Button, Card, Field, PageHead, Select, ToggleField, ask, msg, t, toast } from "@meduza-np/ui-kit";
import { errorText, post } from "../api.ts";
import { reload, isError, type Problem } from "../store.ts";
import { ProblemList } from "../ApplyBar.tsx";

interface Need {
  id: string;
  kind: string;
  label: string;
  node?: string;
  usedBy: string;
  value?: string;
  generated?: boolean;
  disables?: string;
}

interface Preview {
  nodes: Array<{ source: string; id: string; displayName: string; behavior: string }>;
  needs: Need[];
  nodeKeys: Array<{ node: string; secretId: string }>;
  notes: string[];
  document: Record<string, any>;
  existingNodes: string[];
  existingSecrets: string[];
  problems: Problem[];
  disabled: string[];
}

interface Result {
  draftId: string;
  created: string[];
  stored: string[];
  keys: string[];
  disabled: string[];
  errors: number;
}

const MODES = [
  { value: "replace", label: msg("Replace the whole configuration") },
  { value: "merge", label: msg("Merge: the export's nodes and global settings replace those parts") },
];

const multiline = (kind: string) => kind.startsWith("pem-") || kind.startsWith("tinc-") || kind.startsWith("openvpn-");

export function ImportPage(props: { role: "admin" | "operator" | "viewer" }) {
  const [file, setFile] = useState<{ name: string; data: unknown } | null>(null);
  const [createNodes, setCreateNodes] = useState(true);
  const [mode, setMode] = useState("replace");
  const [values, setValues] = useState<Record<string, string>>({});
  const [preview, setPreview] = useState<Preview | null>(null);
  const [result, setResult] = useState<Result | null>(null);
  const [busy, setBusy] = useState(false);
  if (props.role !== "admin") return <div class="callout warn">{t("Only administrators can import configuration.")}</div>;

  const body = (data: unknown) => ({ export: data, createNodes, mode, values: Object.fromEntries(Object.entries(values).filter(([, v]) => v.trim())) });
  const check = async (data = file?.data) => {
    if (!data) return;
    setBusy(true);
    try {
      setPreview(await post<Preview>("/api/v1/import/etcd/preview", body(data)));
      setResult(null);
    } catch (e) {
      setPreview(null);
      toast(errorText(e), "bad");
    } finally {
      setBusy(false);
    }
  };
  const load = async (input: HTMLInputElement) => {
    const f = input.files?.[0];
    if (!f) return;
    try {
      const data = JSON.parse(await f.text()) as unknown;
      setFile({ name: f.name, data });
      setValues({});
      await check(data);
    } catch (e) {
      toast(t("Not a JSON file: {error}", { error: errorText(e) }), "bad");
    }
  };
  const stage = async () => {
    if (!file || !preview) return;
    const question = mode === "replace" ? t("Stage the import? Your saved changes are replaced by the imported configuration (nothing is applied yet).") : t("Stage the import into your saved changes? Nothing is applied yet.");
    if (!(await ask(question, { ok: t("Stage import") }))) return;
    setBusy(true);
    try {
      const r = await post<Result>("/api/v1/import/etcd/apply", body(file.data));
      setResult(r);
      await reload();
      toast(t("Import staged: review it in Changes, then apply."), "ok");
    } catch (e) {
      toast(errorText(e), "bad");
    } finally {
      setBusy(false);
    }
  };

  const d = preview?.document;
  const status = (n: Need) => {
    if (values[n.id]?.trim()) return <Badge tone="ok">{t("supplied")}</Badge>;
    if (n.value) return <Badge tone="ok">{t("in the export")}</Badge>;
    if (n.kind !== "public-key" && preview?.existingSecrets.includes(n.id)) return <Badge tone="ok">{t("already stored")}</Badge>;
    if (n.generated) return <Badge>{t("generated")}</Badge>;
    return <Badge tone="warn">{n.disables ? t("missing: imported switched off") : t("missing")}</Badge>;
  };
  const groups = new Map<string, Need[]>();
  for (const n of preview?.needs ?? []) groups.set(n.node ?? "", [...(groups.get(n.node ?? "") ?? []), n]);
  const missing = (preview?.needs ?? []).filter((n) => !n.value && !n.generated && !values[n.id]?.trim() && !(n.kind !== "public-key" && preview?.existingSecrets.includes(n.id))).length;

  return (
    <div>
      <PageHead title={t("Import")} sub={t("Takes over the configuration of the previous service planner from its etcd export (meduza-etcd-export-v1).")} />
      <Card title={t("Export file")}>
        <div class="form-grid">
          <Field label={t("etcd export (JSON)")} help={file ? file.name : t("Read in this browser and sent to the manager only for the preview and the import.")}>
            <input type="file" accept=".json,application/json" onChange={(e) => void load(e.target as HTMLInputElement)} />
          </Field>
          <Field label={t("Mode")}>
            <Select value={mode} options={MODES.map((m) => ({ value: m.value, label: t(m.label) }))} onChange={setMode} />
          </Field>
        </div>
        <div class="toggle-grid">
          <ToggleField label={t("Create missing nodes")} help={t("Nodes the manager does not know yet are created; each enrolls later with its own one-time token.")} checked={createNodes} onChange={setCreateNodes} />
        </div>
        <p class="small muted">{t("Taken over: DNS hosts, BGP prefix filters, Clash subscriptions (as Mihomo subscriptions), MosDNS, Tinc mesh settings and every node's settings except OSPF and EasyTier. Node status uploads (/update, /updated) are not configuration and are skipped.")}</p>
      </Card>

      {preview && d && (
        <>
          <Card title={t("What the export contains")}>
            <div class="form-grid">
              <Field label={t("Internal ASN")}><span class="mono">{d.routing?.internalAsn ?? "—"}</span></Field>
              <Field label={t("BGP prefix filters")}><span>{t("{in} in / {out} out", { in: d.routing?.bgpFilter?.in?.length ?? 0, out: d.routing?.bgpFilter?.out?.length ?? 0 })}</span></Field>
              <Field label={t("DNS hosts")}><span>{d.dns?.hosts?.length ?? 0}</span></Field>
              <Field label={t("Mihomo subscriptions")}><span>{(d.mihomo?.subscriptions ?? []).map((s: { id: string }) => s.id).join(", ") || "—"}</span></Field>
              <Field label={t("MosDNS rule files")}><span>{d.mosdns?.ruleFiles?.length ?? 0}</span></Field>
              <Field label={t("Tinc cipher / digest")}><span class="mono">{[d.tinc?.cipher, d.tinc?.digest].filter(Boolean).join(" / ") || "—"}</span></Field>
            </div>
            <div class="table-wrap">
              <table class="table">
                <thead>
                  <tr><th>{t("In the export")}</th><th>{t("Node ID")}</th><th>{t("Behavior")}</th><th>{t("Services")}</th><th>{t("Manager")}</th></tr>
                </thead>
                <tbody>
                  {preview.nodes.map((n) => {
                    const svc = (d.nodes[n.id]?.services ?? {}) as Record<string, { enabled?: boolean; instances?: unknown[] }>;
                    return (
                      <tr>
                        <td>{n.source}</td>
                        <td class="mono">{n.id}</td>
                        <td>{t(n.behavior)}</td>
                        <td class="small">
                          {Object.entries(svc)
                            .filter(([, v]) => v.enabled !== false)
                            .map(([k, v]) => (v.instances ? `${k} ×${v.instances.length}` : k))
                            .join(", ")}
                        </td>
                        <td>{preview.existingNodes.includes(n.id) ? <Badge>{t("exists")}</Badge> : createNodes ? <Badge tone="ok">{t("created")}</Badge> : <Badge tone="warn">{t("not registered")}</Badge>}</td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
            {preview.nodeKeys.length > 0 && (
              <p class="small muted">{t("Tinc private keys are redacted in the export: each node generates a new key ({nodes}) and the manager distributes the public keys.", { nodes: preview.nodeKeys.map((k) => k.node).join(", ") })}</p>
            )}
          </Card>

          <Card title={t("Values to supply")} actions={<Badge tone={missing ? "warn" : "ok"}>{t("{n} missing", { n: missing })}</Badge>}>
            <p class="small muted">{t("Secrets are stored encrypted and delivered only to the node that uses them. Leave a value empty to import its tunnel switched off; supply it later in the node's settings.")}</p>
            {[...groups].map(([node, needs]) => (
              <>
                <h3>{node || t("Cluster-wide")}</h3>
                <div class="table-wrap">
                  <table class="table">
                    <thead>
                      <tr><th>{t("Value")}</th><th>{t("Status")}</th><th style={{ width: "45%" }} /></tr>
                    </thead>
                    <tbody>
                      {needs.map((n) => (
                        <tr>
                          <td>
                            {n.label}
                            <div class="small muted mono">{n.kind === "public-key" ? n.id.slice("public:".length) : n.id}</div>
                          </td>
                          <td>{status(n)}</td>
                          <td>
                            {n.value || n.generated ? null : multiline(n.kind) ? (
                              <textarea rows={2} class="mono" style={{ width: "100%" }} spellcheck={false} value={values[n.id] ?? ""} placeholder={t("paste PEM / key file")} onInput={(e) => setValues({ ...values, [n.id]: (e.target as HTMLTextAreaElement).value })} />
                            ) : (
                              <input type={n.kind === "public-key" ? "text" : "password"} autoComplete="off" class="mono" style={{ width: "100%" }} value={values[n.id] ?? ""} placeholder={n.kind === "public-key" ? t("base64 public key") : ""} onInput={(e) => setValues({ ...values, [n.id]: (e.target as HTMLInputElement).value })} />
                            )}
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </>
            ))}
          </Card>

          <Card title={t("Check and stage")}>
            <ProblemList problems={preview.problems.filter(isError)} tone="bad" title={t("Problems to fix after the import")} />
            <ProblemList problems={preview.problems.filter((p) => !isError(p))} tone="warn" />
            {preview.disabled.length > 0 && <p class="small">{t("{n} tunnels are imported switched off until their values are supplied.", { n: preview.disabled.length })}</p>}
            {preview.notes.length > 0 && (
              <ul class="small muted">
                {preview.notes.map((n) => <li>{n}</li>)}
              </ul>
            )}
            <div class="row">
              <Button busy={busy} onClick={() => void check()}>{t("Check again")}</Button>
              <Button variant="primary" busy={busy} onClick={() => void stage()}>{t("Stage import")}</Button>
            </div>
            {result && (
              <div class="callout ok" style={{ marginTop: "10px" }}>
                {t("Staged. Created {created} nodes, stored {stored} secrets, asked {keys} nodes for a Tinc key; {errors} problems remain. Review and apply it from Changes.", {
                  created: result.created.length,
                  stored: result.stored.length,
                  keys: result.keys.length,
                  errors: result.errors,
                })}
              </div>
            )}
          </Card>
        </>
      )}
    </div>
  );
}
