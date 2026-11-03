// Node settings, edited directly in the cluster working copy (D21) and applied through the apply bar. Features can be
// configured before their package is installed (D20). Keys and certificates are created right in the field that
// needs them (generated on the node, issued from the cluster CA, or entered once) instead of separate key pages.

import { useState } from "preact/hooks";
import { Badge, Button, Card, Checkbox, Field, ListInput, Loadable, Modal, Panel, Select, Tabs, TextInput, ToggleField, ask, msg, t, toast, useAsync } from "@meduza-np/ui-kit";
import { DescriptorView, NestedList, childSchema, translator, type ActionRequest, type JsonSchema, type RuntimeInstance, type SecretOption, type SecretRequest, type UiDescriptor } from "@meduza-np/descriptor-renderer";
import { errorText, get, post } from "../api.ts";
import { isError, nodeSpec, problemsFor, reload, setNodeSpec, useConfig, type MihomoCatalog } from "../store.ts";
import { ProblemList } from "../ApplyBar.tsx";
import type { NodeDetail } from "./Node.tsx";
import { packageInstallLabel } from "@meduza-np/ui-kit/package-progress";

const SYNC_FIELDS = [
  ["heartbeatSeconds", msg("Heartbeat")],
  ["pushKeepaliveSeconds", msg("Push keepalive")],
  ["roamingPollSeconds", msg("Roaming poll")],
  ["calibrationSeconds", msg("Clock calibration")],
] as const;

type Role = "admin" | "operator" | "viewer";

interface CatalogEntry {
  manifest: { package: string; role: string; provides: string[]; providers?: Array<{ requiresPreparation?: boolean }> };
  ui: UiDescriptor | null;
  configSchema: JsonSchema | null;
}

interface Secret extends SecretOption {
  nodeScope: string[];
  holderNode: string | null;
}

export function NodeSettings(props: { id: string; detail: NodeDetail; role: Role }) {
  const s = useConfig();
  const catalog = useAsync(() => get<CatalogEntry[]>("/api/v1/packages"), []);
  const nodeSchema = useAsync(() => get<JsonSchema>("/api/v1/schemas/node-config"), []);
  const secrets = useAsync(() => get<Secret[]>("/api/v1/secrets"), []);
  const [section, setSection] = useState("features");
  const [rawText, setRawText] = useState<string | null>(null);
  const editable = props.role !== "viewer";
  if (s.error && !s.loaded) {
    return (
      <div class="callout bad">
        {t("Could not load the configuration: {error}", { error: s.error })} <Button small onClick={() => void reload()}>{t("Retry")}</Button>
      </div>
    );
  }
  if (!s.loaded || !catalog.data || !nodeSchema.data || !secrets.data) return <Loadable state={catalog}>{() => <div class="state-box">{t("Loading…")}</div>}</Loadable>;

  const spec = nodeSpec(s.doc, props.id);
  const setSpec = (next: Record<string, any>) => setNodeSpec(props.id, next);
  const problems = problemsFor(s.preview, props.id);
  // Only keys this node may receive: its own node-held keys and manager-held secrets scoped to it.
  const usable = secrets.data.filter((x) => (x.holder === "node" ? x.holderNode === props.id : x.nodeScope.includes(props.id) || x.kind === "pem-certificate" || x.kind === "pem-crl"));

  return (
    <div>
      {!s.doc.nodes[props.id] && <div class="callout info">{t("This node has no settings yet. Anything you set here is applied once you review and apply it.")}</div>}
      <ProblemList problems={problems.filter((p) => p.severity !== "warning")} tone="bad" />
      <ProblemList problems={problems.filter((p) => p.severity === "warning")} tone="warn" />
      <Tabs
        tabs={[
          { id: "features", label: t("Features") },
          { id: "general", label: t("General") },
          { id: "network", label: t("Network") },
          { id: "raw", label: t("Advanced: raw JSON") },
        ]}
        active={section}
        onChange={(id) => {
          if (id === "raw") setRawText(JSON.stringify(spec, null, 2));
          setSection(id);
        }}
      />
      <fieldset disabled={!editable} style={{ border: 0, padding: 0, margin: 0, minWidth: 0 }}>
        {section === "general" && <GeneralEditor spec={spec} onChange={setSpec} />}
        {section === "network" && <NetworkEditor spec={spec} onChange={setSpec} schema={nodeSchema.data} problems={problems} internalAsn={s.doc.routing?.internalAsn} />}
      </fieldset>
      {section === "features" && (
        <FeaturesEditor node={props.id} detail={props.detail} spec={spec} onChange={setSpec} catalog={catalog.data} secrets={usable} problems={problems} role={props.role} reloadSecrets={secrets.reload} mihomo={s.doc.mihomo} />
      )}
      {section === "raw" && (
        <Card title={t("Node configuration (advanced)")}>
          <p class="small muted">{t("The same schema, permissions and publish flow apply; invalid JSON is not accepted.")}</p>
          <textarea style={{ minHeight: "420px" }} value={rawText ?? ""} onInput={(e) => setRawText((e.target as HTMLTextAreaElement).value)} disabled={!editable} />
          {editable && (
            <Button onClick={() => {
              try {
                setSpec(JSON.parse(rawText ?? "{}"));
                toast(t("Taken over into the form; save to keep it."), "ok");
              } catch (e) {
                toast(t("Invalid JSON: {error}", { error: (e as Error).message }), "bad");
              }
            }}>{t("Use this JSON")}</Button>
          )}
        </Card>
      )}
    </div>
  );
}

// ---- general / network ------------------------------------------------------------------------------------

function GeneralEditor(props: { spec: Record<string, any>; onChange: (s: Record<string, any>) => void }) {
  const s = props.spec;
  const set = (k: string, v: unknown) => props.onChange({ ...s, [k]: v === "" ? undefined : v });
  return (
    <>
      <Card title={t("Identity")}>
        <div class="form-grid">
          <Field label={t("Display name")}><TextInput value={s.displayName} onInput={(v) => set("displayName", v)} /></Field>
          <Field label={t("Behavior")} help={t("static: WebSocket push; roaming: periodic pull without a long-lived subscription.")}>
            <Select value={s.behavior ?? "static"} options={[{ value: "static", label: t("static") }, { value: "roaming", label: t("roaming") }]} onChange={(v) => set("behavior", v)} />
          </Field>
          <Field label={t("Router ID")} help={t("Protocol identifier only; does not create addresses or routes.")}><TextInput mono value={s.routerId} onInput={(v) => set("routerId", v)} /></Field>
          <Field label={t("Tags")}><ListInput value={s.tags} onChange={(v) => set("tags", v.length ? v : undefined)} /></Field>
        </div>
      </Card>
      <Card title={t("Updates")}>
        <div class="form-grid">
          <Field label={t("Channel")}><TextInput value={s.updatePolicy?.channel ?? "stable"} onInput={(v) => set("updatePolicy", { automatic: false, ...s.updatePolicy, channel: v || "stable" })} /></Field>
          <Field label={t("Maintenance window")} help={t("HH:MM-HH:MM")}><TextInput value={s.updatePolicy?.maintenanceWindow} onInput={(v) => set("updatePolicy", { channel: "stable", automatic: false, ...s.updatePolicy, maintenanceWindow: v || undefined })} /></Field>
          <Field label={t("Timezone")}><TextInput value={s.updatePolicy?.timezone} onInput={(v) => set("updatePolicy", { channel: "stable", automatic: false, ...s.updatePolicy, timezone: v || undefined })} /></Field>
        </div>
        <div class="toggle-grid">
          <ToggleField label={t("Automatic")} checked={!!s.updatePolicy?.automatic} onChange={(v) => set("updatePolicy", { channel: "stable", ...s.updatePolicy, automatic: v })} />
        </div>
      </Card>
      <Card title={t("Sync intervals (seconds, jittered)")}>
        <div class="form-grid">
          {SYNC_FIELDS.map(([k, label]) => (
            <Field label={t(label)}>
              <input type="number" value={s.sync?.[k] ?? ""} placeholder={t("default")} onInput={(e) => {
                const v = (e.target as HTMLInputElement).value;
                set("sync", { ...s.sync, [k]: v ? Number(v) : undefined });
              }} />
            </Field>
          ))}
        </div>
      </Card>
    </>
  );
}

function NetworkEditor(props: { spec: Record<string, any>; onChange: (s: Record<string, any>) => void; schema: JsonSchema; problems: Array<{ path: string; message: string }>; internalAsn?: number }) {
  const net = props.spec.networks ?? {};
  const setNet = (k: string, v: unknown) => props.onChange({ ...props.spec, networks: { ...net, [k]: v } });
  const networks = childSchema(props.schema, "networks", props.schema);
  const origin = props.spec.routing?.originate ?? {};
  const setOrigin = (k: string, v: boolean) => props.onChange({ ...props.spec, routing: { ...props.spec.routing, originate: { ...origin, [k]: v } } });
  const sub = (p: string) => props.problems.filter((x) => x.path.includes(`/networks/${p}`)).map((x) => ({ ...x, path: x.path.slice(x.path.indexOf(`/networks/${p}`) + `/networks/${p}`.length) }));
  return (
    <>
      <Card title={t("LAN / private LAN")}>
        <div class="form-grid">
          <Field label={t("LAN prefixes")} help={t("Explicit CIDRs; routing, Access push and proxy sources refer to the same list with separate policies.")}>
            <ListInput value={net.lan} onChange={(v) => setNet("lan", v)} placeholder="10.42.10.0/24" />
          </Field>
          <Field label={t("Private LAN prefixes")} help={t("Internal only by default; never advertised externally.")}>
            <ListInput value={net.privateLan} onChange={(v) => setNet("privateLan", v)} placeholder="10.99.10.0/24" />
          </Field>
        </div>
        <h3>{t("Originate into routing (only these sets are ever advertised)")}</h3>
        <div class="toggle-grid">
          {["lan", "privateLan", "mappings"].map((k) => <ToggleField label={t(k)} checked={!!origin[k]} onChange={(v) => setOrigin(k, v)} />)}
        </div>
      </Card>
      <RoutingCard spec={props.spec} onChange={props.onChange} problems={props.problems} internalAsn={props.internalAsn} />
      {(
        [
          ["mappings", msg("1:1 network mappings"), msg("advertised = external prefix, actual = real local prefix; same family and length.")],
          ["portForwards", msg("Port forwards"), msg("Listen address/port → target; TCP/UDP; optional source restriction and SNAT.")],
          ["healthPorts", msg("Health ports"), msg("Plain TCP listeners with an explicitly declared meaning.")],
        ] as const
      ).map(([key, title, help]) => (
        <Card title={t(title)}>
          <p class="small muted">{t(help)}</p>
          <NestedList
            section={{ title, path: key }}
            schema={networks ? childSchema(networks, key, props.schema) : undefined}
            root={props.schema}
            value={net[key] ?? []}
            problems={sub(key)}
            onChange={(v) => setNet(key, v)}
          />
        </Card>
      ))}
      <Card title={t("Local DNS hosts")}>
        <NestedList
          section={{ title: "hosts", path: "hosts", fields: ["hostname", "ips"] }}
          schema={childSchema(childSchema(props.schema, "dns", props.schema), "hosts", props.schema)}
          root={props.schema}
          value={props.spec.dns?.hosts ?? []}
          onChange={(v) => props.onChange({ ...props.spec, dns: { ...props.spec.dns, hosts: v } })}
        />
      </Card>
    </>
  );
}

/**
 * The node's BGP identity (D24). Tinc mesh members are in the internal AS and need nothing here; a node outside the
 * internal network (a branch reached over WireGuard/OpenVPN) sets its own AS. Sessions themselves are set on the
 * WireGuard peers and OpenVPN instances that carry them, Anycast hosts in the global settings.
 */
function RoutingCard(props: { spec: Record<string, any>; onChange: (s: Record<string, any>) => void; problems: Array<{ path: string; message: string }>; internalAsn?: number }) {
  const routing = props.spec.routing ?? {};
  const member = !!props.spec.services?.tinc && props.spec.services.tinc.enabled !== false;
  const error = props.problems.find((p) => p.path.endsWith("/routing/asn"))?.message;
  const anycast: string[] = routing.anycast ?? [];
  return (
    <Card title={t("BGP")}>
      <div class="form-grid">
        <Field
          label={t("Own AS")}
          error={error}
          help={member ? t("Tinc mesh members are in the internal AS {asn}.", { asn: props.internalAsn ?? "—" }) : t("Only for a node outside the internal network, e.g. a branch connected over WireGuard or OpenVPN.")}
        >
          <input type="number" value={routing.asn ?? ""} placeholder={props.internalAsn !== undefined ? String(props.internalAsn) : ""} onInput={(e) => {
            const v = (e.target as HTMLInputElement).value;
            const { asn: _old, ...rest } = routing;
            const next = v ? { ...rest, asn: Number(v) } : rest;
            const { routing: _r, ...spec } = props.spec;
            props.onChange(Object.keys(next).length ? { ...spec, routing: next } : spec);
          }} />
        </Field>
        <Field label={t("Anycast services")} help={t("Chosen per service in the global settings.")}>
          <div class="small">{anycast.length ? anycast.join(", ") : "—"}</div>
        </Field>
      </div>
      <p class="small muted">{t("External BGP sessions (remote ASN, accepted and advertised prefixes) are set on the WireGuard peer or OpenVPN instance that carries them.")}</p>
      {props.spec.services?.frr && <div class="callout info">{t("This node also has FRR settings written as raw JSON (services.frr); they still apply on top.")}</div>}
    </Card>
  );
}

// ---- features ---------------------------------------------------------------------------------------------

/** The node's Mihomo subscription (D29): the global default, or another entry of the global list. */
function MihomoChoice(props: { catalog?: MihomoCatalog; value?: string; refresh?: number; readOnly: boolean; error?: string; onChange: (v: string | undefined) => void; onRefresh: (v: number | undefined) => void }) {
  const list = (props.catalog?.subscriptions ?? []).filter((s) => s.enabled !== false || s.id === props.value);
  const def = list.find((s) => s.id === props.catalog?.default);
  if (!(props.catalog?.subscriptions ?? []).length) {
    return <div class="callout info">{t("Add Mihomo subscriptions under Global settings; nodes use the default one automatically.")}</div>;
  }
  return (
    <div class="card">
      <h3 style={{ marginTop: 0 }}>{t("Subscription")}</h3>
      <fieldset disabled={props.readOnly} style={{ border: 0, padding: 0, margin: 0, minWidth: 0 }}>
      <div class="form-grid">
      <Field label={t("Subscription")} error={props.error} help={t("Managed in Global settings, refreshed automatically; use “Refresh subscription now” to download it at once.")}>
        <Select
          value={props.value ?? ""}
          options={[
            { value: "", label: t("Default ({name})", { name: def ? def.name ?? def.id : "—" }) },
            ...list.map((s) => ({ value: s.id, label: s.name ? `${s.name} (${s.id})` : s.id })),
          ]}
          onChange={(v) => props.onChange(v || undefined)}
        />
      </Field>
      <Field label={t("Automatic refresh (seconds)")} help={t("This node's own interval; empty uses the one in Global settings ({seconds}).", { seconds: props.catalog?.refreshSeconds ?? 43200 })}>
        <input type="number" min={60} value={props.refresh ?? ""} placeholder={String(props.catalog?.refreshSeconds ?? 43200)} onInput={(e) => {
          const v = (e.target as HTMLInputElement).value;
          props.onRefresh(v ? Number(v) : undefined);
        }} />
      </Field>
      </div>
      </fieldset>
    </div>
  );
}

function FeaturesEditor(props: {
  node: string;
  detail: NodeDetail;
  spec: Record<string, any>;
  onChange: (s: Record<string, any>) => void;
  catalog: CatalogEntry[];
  secrets: SecretOption[];
  problems: Array<{ path: string; message: string }>;
  role: Role;
  reloadSecrets: () => void;
  mihomo?: MihomoCatalog;
}) {
  const [open, setOpen] = useState<Record<string, boolean>>({});
  const [secretReq, setSecretReq] = useState<null | { req: SecretRequest; resolve: (patch: Record<string, unknown> | undefined) => void }>(null);
  const [profileFor, setProfileFor] = useState<string[] | null>(null);
  const readOnly = props.role === "viewer";
  const pkgs = new Map(props.detail.packages.map((p) => [p.package, p]));
  const hasUpdater = !!pkgs.get("meduza-np-updater")?.installed;
  const runtime = ((props.detail.report?.instances ?? []) as RuntimeInstance[]).filter((i) => i && i.id);
  // packages without settings of their own (FRR, compiled from the other features: D24) are not listed
  const configurable = props.catalog.filter((c) => c.ui && (c.ui.settings?.length || c.ui.collections?.length) && (c.manifest.role === "service-adapter" || c.manifest.role === "binary-provider"));
  const services = props.spec.services ?? {};

  const secretAction = (req: SecretRequest) => new Promise<Record<string, unknown> | undefined>((resolve) => setSecretReq({ req, resolve }));

  const onAction = async (req: ActionRequest, pkg: string) => {
    const short = pkg.replace(/^meduza-np-/, "");
    const tx = translator(props.catalog.find((c) => c.manifest.package === pkg)?.ui);
    try {
      if (req.action.id === "generate-keypair") {
        const item = req.item ?? {};
        const secretId = String(item.privateKeyRef ?? "") || `${short}-${props.node}-${String(item.id)}`;
        if (item.privateKeyRef && !(await ask(t("Regenerate {id}? Existing peers must be updated and current connections break.", { id: secretId }), { ok: t("Regenerate"), danger: true }))) return;
        const queued = await generateOnNode(props.node, pkg, short, secretId, item, !!item.privateKeyRef);
        if (!item.privateKeyRef) {
          const list = (services[short]?.instances ?? []) as Array<Record<string, unknown>>;
          props.onChange({ ...props.spec, services: { ...services, [short]: { ...services[short], instances: list.map((i) => (i.id === item.id ? { ...i, privateKeyRef: secretId } : i)) } } });
        }
        toast(keyToast(props.node, queued), "ok");
        return;
      }
      if (req.action.confirm && !(await ask(tx.text(req.action.confirm), { ok: tx.text(req.action.label) }))) return;
      if (req.action.id === "restart-instance") {
        await post(`/api/v1/nodes/${encodeURIComponent(props.node)}/jobs`, { kind: "restart-instance", package: pkg, instance: req.instance });
      } else {
        await post(`/api/v1/nodes/${encodeURIComponent(props.node)}/jobs`, { kind: "run-action", package: pkg, instance: req.instance, params: { action: req.action.id } });
      }
      toast(t("{action} queued for {node}", { action: tx.text(req.action.label), node: props.node }), "ok");
    } catch (e) {
      toast(errorText(e), "bad");
    }
  };

  return (
    <>
      <p class="small muted">
        {hasUpdater
          ? t("Features whose package is not installed yet are installed by this node's updater once you apply.")
          : t("This node has no updater: install the packages of new features by hand; their settings apply then.")}
      </p>
      {configurable.map((c) => {
        const pkg = c.manifest.package;
        const short = pkg.replace(/^meduza-np-/, "");
        const binaryProvider = c.manifest.role === "binary-provider";
        const tx = translator(c.ui);
        const cfg = services[short];
        const present = cfg !== undefined;
        const p = pkgs.get(pkg);
        const installed = p?.registration === "registered";
        const installation = packageInstallLabel(props.detail.report?.update, pkg);
        const isOpen = open[short] ?? false;
        const canOpen = present || (binaryProvider && p?.installed);
        const hasCollection = (c.ui!.collections ?? []).some((col) => col.path === `services.${short}.instances`);
        const access = short === "openvpn" ? ((cfg?.instances ?? []) as Array<Record<string, any>>).filter((i) => i.kind === "access").map((i) => String(i.id)) : [];
        // warnings (e.g. "not installed yet") are not problems of the feature's settings
        const count = props.problems.filter((x) => x.path.includes(`/services/${short}`) && isError(x as Parameters<typeof isError>[0])).length;
        return (
          <Panel
            open={isOpen && !!canOpen}
            onToggle={() => canOpen && setOpen((previous) => ({ ...previous, [short]: !previous[short] }))}
            title={tx.text(c.ui!.title)}
            badges={
              <>
                {present ? (cfg.enabled === false ? <Badge tone="idle">{t("disabled")}</Badge> : <Badge tone="info">{t("configured")}</Badge>) : <Badge tone="idle">{t("not configured")}</Badge>}
                {installed ? (
                  <Badge tone="ok">{t("installed")}</Badge>
                ) : p?.installed ? (
                  <Badge tone="bad" title={p.reason ?? ""}>{t("installed, not registered")}</Badge>
                ) : (
                  <Badge tone={installation && props.detail.report?.update?.phase === "failed" ? "bad" : present ? "warn" : "idle"} title={installation ? props.detail.report?.update?.error ?? "" : ""}>{present ? (hasUpdater ? installation ?? t("installed after apply") : t("install by hand")) : t("not installed")}</Badge>
                )}
                {count > 0 && <Badge tone="bad">{t("{count} problem(s)", { count })}</Badge>}
                {binaryProvider && p?.installedVersion && <Badge>{p.installedVersion}</Badge>}
              </>
            }
            actions={
              !readOnly && (
                <>
                  {access.length > 0 && props.role === "admin" && <Button small onClick={() => setProfileFor(access)}>{t("Client profile…")}</Button>}
                  {present && <Checkbox label={binaryProvider ? t("Request packaged binary") : t("Enabled")} checked={cfg.enabled !== false} onChange={(v) => props.onChange({ ...props.spec, services: { ...services, [short]: { ...cfg, enabled: v } } })} />}
                  {!present ? (
                    <Button small variant="primary" onClick={() => {
                      props.onChange({ ...props.spec, services: { ...services, [short]: hasCollection ? { enabled: true, instances: [] } : { enabled: true } } });
                      setOpen((previous) => ({ ...previous, [short]: true }));
                    }}>{binaryProvider ? t("Install packaged binary") : t("Add")}</Button>
                  ) : (
                    <Button small variant="danger" onClick={async () => {
                      const message = binaryProvider
                        ? t("Stop requesting {package}? Installed or deployment-pinned binaries may remain; the provider status shows what is active.", { package: pkg })
                        : t("Remove all {feature} configuration from {node}? Its instances are stopped and cleaned up after publishing.", { feature: tx.text(c.ui!.title), node: props.node });
                      if (!(await ask(message, { ok: binaryProvider ? t("Stop requesting") : t("Remove"), danger: true }))) return;
                      const { [short]: _gone, ...rest } = services;
                      props.onChange({ ...props.spec, services: rest });
                    }}>{binaryProvider ? t("Stop requesting") : t("Remove")}</Button>
                  )}
                </>
              )
            }
          >
            {present && ["tinc", "wireguard", "openvpn"].includes(short) && <p class="small muted">{t("Tinc and external connections also install FRR routing support automatically.")}</p>}
            {!installed && (
              <div class={`callout ${installation && props.detail.report?.update?.error ? "bad" : "info"}`} style={{ overflowWrap: "anywhere" }}>
                {hasUpdater
                  ? installation ? `${installation}${props.detail.report?.update?.error ? ` · ${props.detail.report.update.error}` : ""}` : t("{package} is not installed on this node yet; its updater installs it after you apply, then these settings take effect.", { package: pkg })
                  : t("{package} is not installed on this node; install it by hand, then these settings take effect.", { package: pkg })}
              </div>
            )}
            {(short === "mihomo" || short === "mihomo-openwrt") && present && (
              <MihomoChoice catalog={props.mihomo} value={cfg.subscription} refresh={cfg.refreshSeconds} readOnly={readOnly} error={props.problems.find((x) => isError(x as Parameters<typeof isError>[0]) && (x.path.endsWith(`/services/${short}/subscription`) || (x.path.endsWith(`/services/${short}`) && /subscription/i.test(x.message))))?.message}
                onChange={(v) => props.onChange({ ...props.spec, services: { ...services, [short]: { ...cfg, subscription: v } } })}
                onRefresh={(v) => props.onChange({ ...props.spec, services: { ...services, [short]: { ...cfg, refreshSeconds: v } } })} />
            )}
            <DescriptorView
              descriptor={binaryProvider && !present ? { ...c.ui!, settings: [], actions: [] } : c.ui!}
              schema={c.configSchema}
              value={props.spec}
              onChange={props.onChange}
              runtime={binaryProvider ? runtime.map((row) => row.id.startsWith(`${short}/`) ? {
                ...row,
                packageVersion: p?.installedVersion ?? row.packageVersion,
                upstreamVersion: row.upstreamVersion ?? row.version,
              } : row) : runtime}
              secrets={props.secrets}
              problems={props.problems}
              readOnly={readOnly}
              onAction={!binaryProvider || (installed && present && cfg.enabled !== false) ? (req) => void onAction(req, pkg) : undefined}
              secretAction={readOnly ? undefined : secretAction}
            />
          </Panel>
        );
      })}
      {secretReq && (
        <SecretDialog
          node={props.node}
          req={secretReq.req}
          onDone={(patch) => {
            secretReq.resolve(patch);
            setSecretReq(null);
            if (patch) props.reloadSecrets();
          }}
        />
      )}
      {profileFor && <ProfileDialog node={props.node} instances={profileFor} onClose={() => setProfileFor(null)} />}
    </>
  );
}

/** Queues key generation on the node; the private key never leaves it, the public key returns with the job result. */
/**
 * Requests a key generated on the node (D28). Returns whether generation is already queued; otherwise the key is
 * pending until the node has the package installed (after applying), then generated and distributed automatically.
 */
async function generateOnNode(node: string, pkg: string, short: string, secretId: string, item: Record<string, unknown>, replace: boolean): Promise<boolean> {
  const keyKind = short === "wireguard" ? "wireguard-private-key" : short === "tinc" ? (item.keyFormat === "ed25519" ? "tinc-ed25519-private" : "tinc-rsa-private") : "pem-private-key";
  const r = await post<{ pending: boolean; jobId: string | null }>(`/api/v1/nodes/${encodeURIComponent(node)}/keys`, { secretId, keyKind, package: pkg, replace });
  return !!r.jobId || !r.pending;
}

const keyToast = (node: string, queued: boolean) =>
  queued
    ? t("Key generation queued on {node}; the private key stays on the node.", { node })
    : t("{node} generates the key once its package is installed (after you apply); it is then distributed automatically.", { node });

// ---- key creation ----------------------------------------------------------------------------------------

type Ca = { id: string; name: string; certRef: string; crlRef: string };

/** What a key field needs: generated on the node, issued from the CA, a generated shared key, or a typed value. */
function secretKindOf(req: SecretRequest): "node-key" | "certificate" | "ca" | "static-key" | "psk" | "value" {
  const f = req.field.split(".").pop()!;
  if (f === "privateKeyRef" && (req.short === "wireguard" || req.short === "tinc")) return "node-key";
  if (f === "certRef" || f === "keyRef") return "certificate";
  if (f === "caRef" || f === "crlRef") return "ca";
  if (f === "tlsCryptRef" || f === "tlsAuthRef" || f === "staticKeyRef") return "static-key";
  if (f === "presharedKeyRef") return "psk";
  return "value";
}

function SecretDialog(props: { node: string; req: SecretRequest; onDone: (patch: Record<string, unknown> | undefined) => void }) {
  const kind = secretKindOf(props.req);
  const field = props.req.field.split(".").pop()!;
  const itemId = String(props.req.item.id ?? props.req.short);
  const cas = useAsync(() => (kind === "certificate" || kind === "ca" ? get<Ca[]>("/api/v1/pki/cas") : Promise.resolve([] as Ca[])), [kind]);
  const [id, setId] = useState(`${props.req.short}-${props.node}-${itemId}-${field.replace(/Ref$/, "")}`.toLowerCase().replace(/[^a-z0-9_-]/g, "-"));
  const [ca, setCa] = useState("");
  const isServer = props.req.item.role === "server" || props.req.item.kind === "access";
  const [cn, setCn] = useState(`${props.node}-${itemId}`);
  const [value, setValue] = useState("");
  const [busy, setBusy] = useState(false);
  const caId = ca || cas.data?.[0]?.id || "";

  const run = async () => {
    setBusy(true);
    try {
      let patch: Record<string, unknown> | undefined;
      if (kind === "node-key") {
        const queued = await generateOnNode(props.node, `meduza-np-${props.req.short}`, props.req.short, id, props.req.item, false);
        toast(keyToast(props.node, queued), "ok");
        patch = { [field]: id };
      } else if (kind === "certificate") {
        const r = await post<{ certRef: string; keyRef: string; caRef: string; crlRef: string }>("/api/v1/pki/certs", { caId, commonName: cn, usage: isServer ? "server" : "client", node: props.node });
        patch = { certRef: r.certRef, keyRef: r.keyRef, caRef: r.caRef, crlRef: r.crlRef };
      } else if (kind === "ca") {
        const c = cas.data?.find((x) => x.id === caId);
        if (c) patch = { caRef: c.certRef, crlRef: c.crlRef };
      } else if (kind === "static-key") {
        await post("/api/v1/pki/static-keys", { id, nodes: [props.node] });
        patch = { [field]: id };
      } else if (kind === "psk") {
        await post("/api/v1/secrets/generate", { id, kind: "wireguard-psk", nodeScope: [props.node] });
        patch = { [field]: id };
      } else {
        await post("/api/v1/secrets", { id, kind: "text", value, nodeScope: [props.node] });
        setValue("");
        patch = { [field]: id };
      }
      props.onDone(patch);
    } catch (e) {
      toast(errorText(e), "bad");
    } finally {
      setBusy(false);
    }
  };

  const titles = {
    "node-key": msg("Generate a key on this node"),
    certificate: msg("Issue a certificate"),
    ca: msg("Use a certificate authority"),
    "static-key": msg("Generate a shared static key"),
    psk: msg("Generate a preshared key"),
    value: msg("Enter a secret value"),
  } as const;
  return (
    <Modal title={t(titles[kind])} onClose={() => props.onDone(undefined)} actions={<Button variant="primary" busy={busy} disabled={(kind === "value" && !value) || ((kind === "certificate" || kind === "ca") && !caId)} onClick={run}>{t("Create")}</Button>}>
      {kind === "node-key" && <p class="small muted">{t("The private key is generated on {node} and never leaves it; its public key is shared with the peers automatically.", { node: props.node })}</p>}
      {kind === "certificate" && <p class="small muted">{t("Issues a {usage} certificate and key from the cluster CA; they are delivered only to {node}.", { usage: t(isServer ? "server" : "client"), node: props.node })}</p>}
      {kind === "static-key" && <p class="small muted">{t("Both ends of the tunnel need this key; give it to an external peer from its export.")}</p>}
      {(kind === "certificate" || kind === "ca") && (
        <Loadable state={cas} empty={(c) => !c.length} emptyText={t("No certificate authority yet: create one under Global settings.")}>
          {(list) => (
            <Field label={t("Certificate authority")}>
              <Select value={caId} options={list.map((c) => ({ value: c.id, label: c.name }))} onChange={setCa} />
            </Field>
          )}
        </Loadable>
      )}
      {kind === "certificate" && <Field label={t("Common name")}><TextInput value={cn} onInput={setCn} /></Field>}
      {kind !== "certificate" && kind !== "ca" && <Field label={t("Secret ID")}><TextInput mono value={id} onInput={setId} /></Field>}
      {kind === "value" && (
        <Field label={t("Value")} help={t("Stored encrypted; delivered only to this node.")}>
          <input type="password" value={value} onInput={(e) => setValue((e.target as HTMLInputElement).value)} autoComplete="off" />
        </Field>
      )}
    </Modal>
  );
}

function ProfileDialog(props: { node: string; instances: string[]; onClose: () => void }) {
  const [instance, setInstance] = useState(props.instances[0] ?? "");
  const [remote, setRemote] = useState("");
  const [busy, setBusy] = useState(false);
  return (
    <Modal title={t("Export Access client profile")} onClose={props.onClose} actions={
      <Button variant="primary" busy={busy} disabled={!instance} onClick={async () => {
        setBusy(true);
        try {
          const r = await post<{ filename: string; content: string }>("/api/v1/pki/access-profile", { node: props.node, instance, remoteHost: remote || undefined });
          const url = URL.createObjectURL(new Blob([r.content], { type: "application/x-openvpn-profile" }));
          const a = document.createElement("a");
          a.href = url;
          a.download = r.filename;
          a.click();
          URL.revokeObjectURL(url);
          props.onClose();
        } catch (e) {
          toast(errorText(e), "bad");
        } finally {
          setBusy(false);
        }
      }}>{t("Download .ovpn")}</Button>
    }>
      <p class="small muted">{t("Contains the CA and tls-crypt key (and optionally a client certificate); the export is audited.")}</p>
      <Field label={t("Access instance")}><Select value={instance} options={props.instances} onChange={setInstance} /></Field>
      <Field label={t("Remote host (optional)")} help={t("Defaults to the instance publicEndpoint.")}><TextInput value={remote} onInput={setRemote} /></Field>
    </Modal>
  );
}
