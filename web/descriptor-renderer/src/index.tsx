// Renders a package UI descriptor (AGENT.md §5.3) against the package's config schema. Only built-in widgets
// and declared action ids are used; nothing from the descriptor is executed.

import { createContext, type ComponentChildren } from "preact";
import { useContext, useEffect, useMemo, useState } from "preact/hooks";
import { Badge, Button, Field, FieldGroup, ListInput, Modal, NumberInput, Select, StateBadge, Table, Tabs, TextInput, ToggleField, ask, formatCell, getLocale, t, type Column } from "@meduza-np/ui-kit";
import { childSchema, evalWhen, fieldError, fieldInfo, getAt, humanize, itemSchema, resolve, setAt, splitPath, unwrapNullable, walk, type FieldInfo, type JsonSchema } from "./schema.ts";
import "./messages.ts";
import { accessFirst } from "@meduza-np/ui-kit/instance-tree";

export * from "./schema.ts";

/** A descriptor's own translations for one interface language (see `UiDescriptor.i18n`). */
export interface UiTranslations {
  text?: Record<string, string>;
  fields?: Record<string, string>;
}

/** Looks up descriptor texts and field labels in the current language. */
export interface Tx {
  text: (s: string) => string;
  field: (name: string) => string;
}

/**
 * Translator for a descriptor: its own `i18n` entry first, then the app dictionaries (`t`), then the text as written;
 * field labels fall back to the humanized field name.
 */
export function translator(d?: { i18n?: Record<string, UiTranslations | undefined> } | null): Tx {
  const tr = d?.i18n?.[getLocale()];
  return {
    text: (s) => tr?.text?.[s] ?? t(s),
    field: (name) => {
      const last = name.split(".").pop() ?? name;
      return tr?.fields?.[name] ?? tr?.fields?.[last] ?? t(humanize(last));
    },
  };
}

export interface UiAction {
  id: string;
  label: string;
  mutating?: boolean;
  confirm?: string;
  role?: string;
  reauth?: boolean;
}

export interface UiSection {
  title: string;
  fields?: string[];
  widget?: string;
  path?: string;
  when?: string;
  advanced?: boolean;
}

export interface UiDescriptor {
  uiVersion: number;
  package: string;
  requiresCapability: string;
  title: string;
  description?: string;
  settings?: Array<{ path: string; title: string; sections: UiSection[] }>;
  collections?: Array<{ path: string; itemId: string; title?: string; columns: string[]; sections: UiSection[]; actions?: UiAction[]; runtimeKind?: string }>;
  actions?: UiAction[];
  status?: Array<{ title: string; instanceKind: string; columns: string[] }>;
  i18n?: Record<string, UiTranslations | undefined>;
}

export interface RuntimeInstance {
  id: string;
  kind: string;
  state: string;
  reason?: string;
  [k: string]: unknown;
}

export interface SecretOption {
  id: string;
  kind: string;
  publicValue: string | null;
  holder: string;
}

export interface Problem {
  path: string;
  message: string;
}

export interface ActionRequest {
  action: UiAction;
  /** `<short>/<item id>` for collection actions. */
  instance?: string;
  item?: Record<string, unknown>;
}

/** A key field asks the console to create its secret (generate on the node, issue a certificate, enter a value). */
export interface SecretRequest {
  /** Package short name (`wireguard`). */
  short: string;
  /** Field name as used in the descriptor (`privateKeyRef`, `tls.certRef`). */
  field: string;
  /** The object the field belongs to (a WireGuard instance, an OpenVPN instance…). */
  item: Record<string, unknown>;
}

/** Creates a secret for a field; resolves to the fields to set next to it (e.g. certRef + keyRef), or nothing. */
export type SecretAction = (req: SecretRequest) => Promise<Record<string, unknown> | undefined>;

/** Console-provided numeric choices, shared by settings, collections and nested dialogs. */
type NumberOptions = Record<string, Array<{ value: number; label: string }>>;
const NumericChoices = createContext<NumberOptions>({});

export interface RendererProps {
  numberOptions?: NumberOptions;
  descriptor: UiDescriptor;
  schema: JsonSchema | null;
  /** The node spec (`services.*` lives below it). */
  value: Record<string, unknown>;
  onChange: (next: Record<string, unknown>) => void;
  runtime?: RuntimeInstance[];
  secrets?: SecretOption[];
  problems?: Problem[];
  onAction?: (req: ActionRequest) => void;
  secretAction?: SecretAction;
  readOnly?: boolean;
}

const SUPPORTED_UI_VERSION = 1;

function shortOf(pkg: string): string {
  return pkg.replace(/^meduza-np-/, "");
}

/** Schema for a dotted descriptor path; the package schema root corresponds to `services.<short>`. */
function schemaAt(root: JsonSchema | null, short: string, dotted: string): JsonSchema | undefined {
  if (!root) return undefined;
  const parts = splitPath(dotted);
  if (parts[0] !== "services" || parts[1] !== short) return undefined;
  return parts.length === 2 ? root : walk(root, parts.slice(2), root);
}

export function DescriptorView(props: RendererProps) {
  const d = props.descriptor;
  if (d.uiVersion !== SUPPORTED_UI_VERSION) {
    return (
      <div class="card">
        {t("This package uses UI protocol {version}; this console supports {supported}. Editing is disabled (incompatible).", { version: d.uiVersion, supported: SUPPORTED_UI_VERSION })}
      </div>
    );
  }
  const short = shortOf(d.package);
  const tx = translator(d);
  return (
    <NumericChoices.Provider value={props.numberOptions ?? {}}><div>
      {d.description && <p class="muted">{tx.text(d.description)}</p>}
      {(d.settings ?? []).map((block) => {
        const path = splitPath(block.path);
        const schema = schemaAt(props.schema, short, block.path);
        const obj = (getAt(props.value, path) ?? {}) as Record<string, unknown>;
        return (
          <div class="card">
            <div class="row" style={{ justifyContent: "space-between", alignItems: "center" }}>
              <h3 style={{ marginTop: 0 }}>{tx.text(block.title)}</h3>
              <span class="row">
                {(d.actions ?? [])
                  .filter((a) => !(props.readOnly && a.mutating) && props.onAction)
                  .map((a) => (
                    <Button small onClick={() => props.onAction!({ action: a, instance: settingsInstance(props.runtime, short) })}>{tx.text(a.label)}</Button>
                  ))}
              </span>
            </div>
            <SectionsForm
              tabbed
              tx={tx}
              short={short}
              secretAction={props.secretAction}
              sections={block.sections}
              schema={schema}
              root={props.schema}
              value={obj}
              readOnly={props.readOnly}
              secrets={props.secrets}
              problems={prefixProblems(props.problems, `/services/${short}`)}
              onChange={(next) => props.onChange(setAt(props.value, path, next))}
            />
          </div>
        );
      })}
      {(d.collections ?? []).map((c) => (
        <CollectionEditor key={c.path} collection={c} short={short} tx={tx} {...props} />
      ))}
      {(d.status ?? []).map((panel) => {
        const rows = (props.runtime ?? []).filter((r) => r.kind === panel.instanceKind);
        return (
          <div class="card">
            <h3 style={{ marginTop: 0 }}>{tx.text(panel.title)}</h3>
            <Table
              rows={rows as unknown as Record<string, unknown>[]}
              rowKey={(r) => String(r.id)}
              empty={t("No status reported yet (not the same as healthy)")}
              columns={panel.columns.map((col) => ({
                key: col,
                label: tx.field(col),
                render: col === "state" ? (r) => <StateBadge state={String(r.state)} /> : undefined,
              }))}
            />
          </div>
        );
      })}
    </div></NumericChoices.Provider>
  );
}

/** Runtime instance of a settings-block package (`mihomo/main`), for its actions. */
function settingsInstance(runtime: RendererProps["runtime"], short: string): string {
  const own = (runtime ?? []).map((r) => String(r.id)).filter((id) => id.startsWith(`${short}/`) && id.split("/").length === 2);
  return own[0] ?? `${short}/main`;
}

function prefixProblems(problems: Problem[] | undefined, prefix: string): Problem[] {
  return (problems ?? []).filter((p) => p.path.includes(prefix)).map((p) => ({ ...p, path: p.path.slice(p.path.indexOf(prefix) + prefix.length) }));
}

/** The client-side checks of `fieldError` in the interface language. */
function localError(e: string | undefined): string | undefined {
  const bound = e && /^must be ([≥≤]) (.+)$/.exec(e);
  return bound ? t(`must be ${bound[1]} {value}`, { value: bound[2]! }) : e && t(e);
}

/** Problem path of a descriptor field: `tproxy.port` → `/tproxy/port`. */
function pointer(name: string): string {
  return `/${splitPath(name).join("/")}`;
}

function problemFor(problems: Problem[] | undefined, path: string): string | undefined {
  return problems?.find((p) => p.path === path || p.path.startsWith(`${path}/`))?.message;
}

// ---- collections ----------------------------------------------------------------------------------------

/**
 * A collection (WireGuard interfaces, OpenVPN instances…) as a compact table, edited like LuCI: Edit opens the item
 * in a dialog with its sections as tabs; Save writes it back, Dismiss drops the edits. Adding works the same way.
 */
function CollectionEditor(props: RendererProps & { collection: NonNullable<UiDescriptor["collections"]>[number]; short: string; tx: Tx }) {
  const c = props.collection;
  const tx = props.tx;
  const path = splitPath(c.path);
  const listSchema = schemaAt(props.schema, props.short, c.path);
  const root = props.schema;
  const schema = root ? itemSchema(listSchema, root) : undefined;
  const items = ((getAt(props.value, path) ?? []) as Array<Record<string, unknown>>).filter((x) => x && typeof x === "object");
  const [editing, setEditing] = useState<{ index: number | null; item: Record<string, unknown> } | null>(null);
  const relPath = `/${path.slice(2).join("/")}`;
  const title = c.title ? tx.text(c.title) : tx.field(path[path.length - 1] ?? "");

  const write = (next: Array<Record<string, unknown>>) => props.onChange(setAt(props.value, path, next));
  const idOf = (it: Record<string, unknown>) => String(it[c.itemId] ?? "");
  const runtimeOf = (it: Record<string, unknown>) => props.runtime?.find((r) => r.id === `${props.short}/${idOf(it)}`
    && (!c.runtimeKind || r.kind === (props.short === "openvpn" && it.kind === "access" ? "openvpn-access" : c.runtimeKind)));
  const collectionProblems = prefixProblems(props.problems, `/services/${props.short}${relPath}`);
  const uniqueId = (base: string) => {
    let n = 1;
    while (items.some((it) => idOf(it) === `${base}${n}`)) n++;
    return `${base}${n}`;
  };

  const columns: Column<Record<string, unknown>>[] = [
    ...c.columns.map((col) => ({
      key: col,
      label: tx.field(col.replace(/^runtime\./, "")),
      render: (r: Record<string, unknown>) => {
        if (col.startsWith("runtime.")) {
          const rt = runtimeOf(r);
          const field = col.slice("runtime.".length);
          if (!rt) return <span class="faint">{t("not reported")}</span>;
          return field === "state" ? <StateBadge state={rt.state} /> : formatCell(rt[field]);
        }
        if (col === "enabled") return r.enabled === false ? <Badge tone="idle">{t("disabled")}</Badge> : <Badge tone="ok">{t("enabled")}</Badge>;
        if (col === c.itemId) {
          const i = items.indexOf(r);
          return (
            <span class="row" style={{ gap: "6px", flexWrap: "nowrap" }}>
              <strong>{idOf(r)}</strong>
              {prefixProblems(collectionProblems, `/${i}`).length > 0 && <Badge tone="bad">{t("problem")}</Badge>}
            </span>
          );
        }
        return formatCell(getAt(r, splitPath(col)));
      },
    })),
    {
      key: "__actions",
      label: "",
      render: (r) => {
        const i = items.indexOf(r);
        return (
          <span class="row" style={{ justifyContent: "flex-end", flexWrap: "nowrap" }} onClick={(e) => e.stopPropagation()}>
            {(c.actions ?? [])
              .filter((a) => !(props.readOnly && a.mutating)
                && !(a.id === "restart-instance" && (r.enabled === false || getAt(props.value, ["services", props.short, "enabled"]) === false || runtimeOf(r)?.state === "disabled")))
              .map((a) => (
                <Button small onClick={() => props.onAction?.({ action: a, instance: `${props.short}/${idOf(r)}`, item: r })}>{tx.text(a.label)}</Button>
              ))}
            <Button small onClick={() => setEditing({ index: i, item: structuredClone(r) })}>{props.readOnly ? t("Show") : t("Edit")}</Button>
            {!props.readOnly && (
              <>
                <Button small onClick={() => write(items.map((it, j) => (j === i ? { ...it, enabled: it.enabled === false } : it)))}>
                  {r.enabled === false ? t("Enable") : t("Disable")}
                </Button>
                <Button small variant="danger" onClick={async () => {
                  if (!(await ask(t("Remove {id}? Its runtime resources are cleaned up after publishing.", { id: idOf(r) }), { ok: t("Delete"), danger: true }))) return;
                  write(items.filter((_, j) => j !== i));
                }}>{t("Delete")}</Button>
              </>
            )}
          </span>
        );
      },
    },
  ];

  const editIndex = editing?.index ?? null;
  const editId = editing ? idOf(editing.item) : "";
  const duplicate = editing ? items.some((it, j) => j !== editIndex && idOf(it) === editId) : false;

  return (
    <div class="card">
      <div class="row" style={{ marginBottom: "10px" }}>
        <h3 style={{ margin: 0 }}>{title}</h3>
        <span class="spacer" />
        {!props.readOnly && (
          <Button small variant="primary" onClick={() => setEditing({ index: null, item: { enabled: true, ...defaultsFor(schema, root), [c.itemId]: uniqueId("new-") } })}>
            {t("Add")}
          </Button>
        )}
      </div>
      <Table columns={columns} rows={props.short === "openvpn" ? accessFirst(items, (i) => i.kind) : items} rowKey={idOf} onSelect={(r) => setEditing({ index: items.indexOf(r), item: structuredClone(r) })} empty={t("No instances. Add one to configure this feature.")} />
      {editing && (
        <Modal
          wide
          title={`${title}: ${editId || t("new")}`}
          onClose={() => setEditing(null)}
          actions={
            !props.readOnly && (
              <Button variant="primary" disabled={!editId || duplicate} onClick={() => {
                write(editIndex === null ? [...items, editing.item] : items.map((it, j) => (j === editIndex ? editing.item : it)));
                setEditing(null);
              }}>{t("Save")}</Button>
            )
          }
        >
          {duplicate && <div class="callout bad">{t("Another entry already uses the id {id}.", { id: editId })}</div>}
          <SectionsForm
            tabbed
            tx={tx}
            short={props.short}
            secretAction={props.secretAction}
            sections={c.sections}
            schema={schema}
            root={root}
            value={editing.item}
            readOnly={props.readOnly}
            secrets={props.secrets}
            problems={editIndex === null ? [] : prefixProblems(collectionProblems, `/${editIndex}`)}
            onChange={(next) => setEditing({ ...editing, item: next })}
          />
          {editIndex !== null && runtimeOf(items[editIndex]!) && (
            <details>
              <summary class="small muted">{t("Runtime status")}</summary>
              <pre>{JSON.stringify(runtimeOf(items[editIndex]!), null, 2)}</pre>
            </details>
          )}
        </Modal>
      )}
    </div>
  );
}

function defaultsFor(schema: JsonSchema | undefined, root: JsonSchema | null): Record<string, unknown> {
  if (!schema || !root) return {};
  const out: Record<string, unknown> = {};
  for (const [k, s] of Object.entries(schema.properties ?? {})) {
    const info = fieldInfo(s, root, schema.required?.includes(k));
    if (info.defaultValue !== undefined) out[k] = structuredClone(info.defaultValue);
    else if (info.required && !info.nullable) {
      if (info.kind === "string") out[k] = "";
      else if (info.kind === "string-list" || info.kind === "object-list") out[k] = [];
      else if (info.kind === "boolean") out[k] = false;
    }
  }
  return out;
}

// ---- sections and fields --------------------------------------------------------------------------------

function SectionsForm(props: {
  tx: Tx;
  short: string;
  secretAction?: SecretAction;
  sections: UiSection[];
  schema: JsonSchema | undefined;
  root: JsonSchema | null;
  value: Record<string, unknown>;
  onChange: (v: Record<string, unknown>) => void;
  secrets?: SecretOption[];
  problems?: Problem[];
  readOnly?: boolean;
  /** Sections as tabs (inside an edit dialog) instead of one long form. */
  tabbed?: boolean;
}) {
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [tab, setTab] = useState(0);
  const visible = props.sections.filter((s) => evalWhen(s.when, props.value));
  const hasAdvanced = visible.some((s) => s.advanced);
  const sectionProblems = (s: UiSection) =>
    s.path ? prefixProblems(props.problems, `/${s.path}`) : (props.problems ?? []).filter((p) => (s.fields ?? []).map(pointer).some((f) => p.path === f || p.path.startsWith(`${f}/`)));

  const body = (section: UiSection, heading: boolean) => (
    <fieldset style={{ border: 0, padding: 0, margin: "0 0 10px", minWidth: 0 }} disabled={props.readOnly}>
      {heading && <h3>{props.tx.text(section.title)}</h3>}
      {section.path ? (
        <NestedList
          tx={props.tx}
          short={props.short}
          secretAction={props.secretAction}
          section={section}
          schema={props.schema && props.root ? childSchema(props.schema, section.path, props.root) : undefined}
          root={props.root}
          value={(props.value[section.path] ?? []) as Array<Record<string, unknown>>}
          secrets={props.secrets}
          problems={prefixProblems(props.problems, `/${section.path}`)}
          readOnly={props.readOnly}
          onChange={(v) => props.onChange(setAt(props.value, [section.path!], v))}
        />
      ) : (
        <FieldGrid
          names={section.fields ?? []}
          schema={props.schema}
          root={props.root}
          value={props.value}
          tx={props.tx}
          onChange={props.onChange}
          render={(name) => (
            <FieldEditor
              tx={props.tx}
              short={props.short}
              secretAction={props.secretAction}
              name={name}
              widget={section.widget}
              schema={props.schema}
              root={props.root}
              value={props.value}
              secrets={props.secrets}
              error={problemFor(props.problems, pointer(name))}
              onChange={(v) => props.onChange(setAt(props.value, splitPath(name), v))}
              onPatch={(patch) => props.onChange(patchSiblings(props.value, name, patch))}
            />
          )}
        />
      )}
    </fieldset>
  );

  if (props.tabbed && visible.length > 1) {
    const current = Math.min(tab, visible.length - 1);
    return (
      <div>
        <Tabs
          tabs={visible.map((s, i) => ({ id: String(i), label: `${props.tx.text(s.title)}${sectionProblems(s).length ? " ⚠" : ""}` }))}
          active={String(current)}
          onChange={(id) => setTab(Number(id))}
        />
        {body(visible[current]!, false)}
      </div>
    );
  }
  return (
    <div>
      {visible.filter((s) => !s.advanced || showAdvanced).map((s) => body(s, true))}
      {hasAdvanced && (
        <Button small onClick={() => setShowAdvanced(!showAdvanced)}>
          {showAdvanced ? t("Hide advanced") : t("Show advanced")}
        </Button>
      )}
    </div>
  );
}

/**
 * The fields of a form. On/off settings are gathered as tiles below the other fields. An object field
 * (`tproxy.sources`) is shown as a box of its own fields; when the object is optional (`tproxy.sideRouter`, or the
 * parent of `bgp.remoteAsn`) the box has a switch and the object exists only while it is on, so its defaults never
 * pretend a setting that is absent.
 */
function FieldGrid(props: {
  names: string[];
  schema: JsonSchema | undefined;
  root: JsonSchema | null;
  value: Record<string, unknown>;
  tx: Tx;
  onChange: (v: Record<string, unknown>) => void;
  render: (name: string) => ComponentChildren;
  depth?: number;
  /** The group being rendered (its own fields are not grouped again). */
  inside?: string;
}) {
  const depth = props.depth ?? 0;
  const infoAt = (path: string[]) => (props.schema && props.root ? fieldInfo(walk(props.schema, path, props.root), props.root) : undefined);
  const plain: string[] = [];
  const toggles: string[] = [];
  const groups = new Map<string, { names: string[]; switchable: boolean; help?: string }>();
  for (const name of props.names) {
    const path = splitPath(name);
    const own = infoAt(path);
    if (own?.kind === "object" && own.schema.properties && depth < 3) {
      groups.set(name, { names: Object.keys(own.schema.properties).map((k) => `${name}.${k}`), switchable: own.nullable, help: own.description });
      continue;
    }
    const parent = path.slice(0, -1);
    const outer = parent.length ? infoAt(parent) : undefined;
    if (outer?.kind === "object" && outer.nullable && parent.join(".") !== props.inside) {
      const key = parent.join(".");
      const group = groups.get(key) ?? { names: [], switchable: true };
      group.names.push(name);
      groups.set(key, group);
    } else if (own?.kind === "boolean") toggles.push(name);
    else plain.push(name);
  }
  return (
    <>
      {plain.length > 0 && <div class="form-grid">{plain.map(props.render)}</div>}
      {toggles.length > 0 && <div class="toggle-grid">{toggles.map(props.render)}</div>}
      {[...groups].map(([key, group]) => {
        const path = splitPath(key);
        const on = getAt(props.value, path) !== undefined;
        return (
          <FieldGroup
            title={props.tx.field(key)}
            help={group.help && <Prose text={group.help} />}
            enabled={on}
            onToggle={group.switchable ? (v) => props.onChange(setAt(props.value, path, v ? {} : undefined)) : undefined}
          >
            <FieldGrid {...props} names={group.names} depth={depth + 1} inside={key} />
          </FieldGroup>
        );
      })}
    </>
  );
}

/** Schema descriptions as help text: line breaks from doc comments are joined and `code` spans kept. */
function Prose(props: { text: string }) {
  const parts = props.text.replace(/\s*\n\s*/g, " ").split("`");
  return <>{parts.map((p, i) => (i % 2 ? <code>{p}</code> : p))}</>;
}

/** Sets fields next to `name` (same parent object): a created certificate fills certRef and keyRef together. */
function patchSiblings(value: Record<string, unknown>, name: string, patch: Record<string, unknown>): Record<string, unknown> {
  const parent = splitPath(name).slice(0, -1);
  return Object.entries(patch).reduce((acc, [k, v]) => setAt(acc, [...parent, k], v), value);
}

export function FieldEditor(props: {
  tx?: Tx;
  short?: string;
  secretAction?: SecretAction;
  onPatch?: (patch: Record<string, unknown>) => void;
  name: string;
  widget?: string;
  schema: JsonSchema | undefined;
  root: JsonSchema | null;
  value: Record<string, unknown>;
  onChange: (v: unknown) => void;
  secrets?: SecretOption[];
  error?: string;
}) {
  const choices = useContext(NumericChoices)[props.name.split(".").at(-1)!];
  const path = splitPath(props.name);
  const fieldSchema = props.schema && props.root ? walk(props.schema, path, props.root) : undefined;
  const parentSchema = props.schema && props.root && path.length > 1 ? walk(props.schema, path.slice(0, -1), props.root) : props.schema;
  const required = !!(props.root ? unwrapNullable(resolve(parentSchema, props.root) ?? {}, props.root).schema : parentSchema)?.required?.includes(path[path.length - 1]!);
  const info: FieldInfo = props.root ? fieldInfo(fieldSchema, props.root, required) : { kind: "json", schema: {}, nullable: true, required: false };
  const current = getAt(props.value, path);
  const error = props.error ?? localError(fieldError(info, current));
  const label = (props.tx ?? translator()).field(props.name);
  const help = info.description ? <Prose text={info.description} /> : undefined;

  // Secret references choose from the secret store, never take secret text inline.
  if (props.name.endsWith("Ref") && info.kind === "string") {
    const options = (props.secrets ?? []).map((s) => ({ value: s.id, label: `${s.id} (${s.kind}${s.holder === "node" ? `, ${t("node-held")}` : ""})` }));
    const selected = props.secrets?.find((s) => s.id === current);
    return (
      <Field label={label} help={help} error={error}>
        <div class="row" style={{ flexWrap: "nowrap" }}>
          <Select value={current as string} options={options} allowEmpty onChange={(v) => props.onChange(v || undefined)} invalid={!!error} />
          {props.secretAction && (
            <Button small onClick={async () => {
              const patch = await props.secretAction!({ short: props.short ?? "", field: props.name, item: props.value });
              if (!patch) return;
              if (props.onPatch) props.onPatch(patch);
              else props.onChange(patch[path[path.length - 1]!]);
            }}>{t("New…")}</Button>
          )}
        </div>
        {props.widget === "keypair" && selected?.publicValue && <div class="small mono">{t("public:")} {selected.publicValue}</div>}
        {current && !selected && <div class="small" style={{ color: "var(--warn)" }}>{t("Secret {id} does not exist yet", { id: String(current) })}</div>}
        {selected && selected.holder === "node" && !selected.publicValue && (
          <div class="small muted">{t("Pending: generated on the node once its package is installed, then distributed automatically.")}</div>
        )}
      </Field>
    );
  }
  if (props.widget === "keypair" && props.name === "publicKey") {
    return (
      <Field label={label} help={t("Derived from the private key; filled in when publishing.")}>
        <code>{current ? String(current) : "—"}</code>
      </Field>
    );
  }
  if (info.kind === "boolean") {
    return <ToggleField label={label} help={help} error={error} checked={current === undefined ? info.defaultValue === true : current === true} onChange={props.onChange} />;
  }
  return (
    <Field label={label} help={help} error={error} wide={info.kind === "json" || info.kind === "object"}>
      <Input info={info} value={current} onChange={props.onChange} invalid={!!error} />
      {(info.kind === "integer" || info.kind === "number") && !!choices?.length && <Select
        value={choices.some((c) => c.value === current) ? String(current) : ""}
        options={choices.map((c) => ({ value: String(c.value), label: c.label }))}
        allowEmpty emptyLabel={t("Enter a number or select a saved value")}
        onChange={(v) => { if (v) props.onChange(Number(v)); }}
      />}
    </Field>
  );
}

function Input(props: { info: FieldInfo; value: unknown; onChange: (v: unknown) => void; invalid?: boolean }) {
  const { info } = props;
  // An unset field shows its default, so an empty box never looks like "nothing".
  const fallback = info.defaultValue !== undefined && info.defaultValue !== null && typeof info.defaultValue !== "object" ? String(info.defaultValue) : undefined;
  switch (info.kind) {
    case "integer":
    case "number":
      return <NumberInput value={props.value as number | undefined} min={info.min} max={info.max} placeholder={fallback} invalid={props.invalid} onInput={props.onChange} />;
    case "enum":
      return (
        <Select
          value={props.value as string}
          options={info.enumValues ?? []}
          allowEmpty={info.nullable || fallback !== undefined}
          emptyLabel={fallback !== undefined ? t("Default ({value})", { value: fallback }) : undefined}
          invalid={props.invalid}
          onChange={(v) => props.onChange(v || undefined)}
        />
      );
    case "string":
      return <TextInput value={props.value as string} placeholder={fallback} invalid={props.invalid} onInput={(v) => props.onChange(v === "" && (info.nullable || fallback !== undefined) ? undefined : v)} />;
    case "string-list":
      return <ListInput value={props.value as string[]} invalid={props.invalid}
        onChange={(v) => props.onChange(v.length === 0 && Array.isArray(info.defaultValue) ? undefined : v)}
        placeholder={Array.isArray(info.defaultValue) ? info.defaultValue.join("\n") : t("one per line")} />;
    case "number-list":
      return <NumberListInput value={props.value as number[] | undefined} invalid={props.invalid} onChange={props.onChange} />;
    default:
      return <JsonInput value={props.value} onChange={props.onChange} />;
  }
}

/** Numbers separated by commas or spaces (`22, 53`); the value updates only while every entry is a number. */
function NumberListInput(props: { value: number[] | undefined; onChange: (v: number[]) => void; invalid?: boolean }) {
  const [text, setText] = useState((props.value ?? []).join(", "));
  const [bad, setBad] = useState(false);
  const parse = (raw: string) => raw.split(/[\s,]+/).filter(Boolean).map(Number);
  useEffect(() => {
    if (JSON.stringify(parse(text)) !== JSON.stringify(props.value ?? [])) setText((props.value ?? []).join(", "));
  }, [props.value]);
  return (
    <input
      type="text"
      class={["mono", bad || props.invalid ? "invalid" : ""].join(" ")}
      value={text}
      placeholder={t("e.g. 22, 53")}
      onInput={(e) => {
        const raw = (e.target as HTMLInputElement).value;
        setText(raw);
        const nums = parse(raw);
        const ok = nums.every((n) => Number.isInteger(n));
        setBad(!ok);
        if (ok) props.onChange(nums);
      }}
    />
  );
}

function JsonInput(props: { value: unknown; onChange: (v: unknown) => void }) {
  const [text, setText] = useState(props.value === undefined ? "" : JSON.stringify(props.value, null, 2));
  const [bad, setBad] = useState(false);
  return (
    <textarea
      class={bad ? "invalid" : ""}
      value={text}
      onInput={(e) => {
        const t = (e.target as HTMLTextAreaElement).value;
        setText(t);
        if (t.trim() === "") {
          setBad(false);
          props.onChange(undefined);
          return;
        }
        try {
          props.onChange(JSON.parse(t));
          setBad(false);
        } catch {
          setBad(true);
        }
      }}
    />
  );
}

/** Table for nested arrays such as WireGuard peers; each entry is edited in its own dialog (Save / Dismiss). */
export function NestedList(props: {
  tx?: Tx;
  short?: string;
  secretAction?: SecretAction;
  section: UiSection;
  schema: JsonSchema | undefined;
  root: JsonSchema | null;
  value: Array<Record<string, unknown>>;
  onChange: (v: Array<Record<string, unknown>>) => void;
  secrets?: SecretOption[];
  problems?: Problem[];
  readOnly?: boolean;
}) {
  const items = Array.isArray(props.value) ? props.value : [];
  const item = props.schema && props.root ? itemSchema(props.schema, props.root) : undefined;
  const fields = props.section.fields?.length ? props.section.fields : Object.keys(item?.properties ?? {});
  const [editing, setEditing] = useState<{ index: number | null; item: Record<string, unknown> } | null>(null);
  const summary = useMemo(() => fields.slice(0, 4), [fields.join(",")]);
  const tx = props.tx ?? translator();
  const label = (it: Record<string, unknown>, i: number) => String(it.id ?? it.name ?? i + 1);
  return (
    <div>
      {items.length > 0 ? (
        <div class="table-wrap">
          <table class="table">
            <thead>
              <tr>
                {summary.map((f) => <th>{tx.field(f)}</th>)}
                <th />
              </tr>
            </thead>
            <tbody>
              {items.map((it, i) => (
                <tr style={{ cursor: "pointer" }} onClick={() => setEditing({ index: i, item: structuredClone(it) })}>
                  {summary.map((f) => <td>{formatCell(getAt(it, splitPath(f)))}</td>)}
                  <td class="actions" onClick={(e) => e.stopPropagation()}>
                    {problemFor(props.problems, `/${i}`) && <Badge tone="bad">{t("problem")}</Badge>}{" "}
                    <Button small onClick={() => setEditing({ index: i, item: structuredClone(it) })}>{props.readOnly ? t("Show") : t("Edit")}</Button>{" "}
                    {!props.readOnly && <Button small variant="danger" onClick={() => props.onChange(items.filter((_, j) => j !== i))}>{t("Delete")}</Button>}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : (
        <div class="empty">{t("No entries")}</div>
      )}
      {!props.readOnly && (
        <div style={{ marginTop: "8px" }}>
          <Button small onClick={() => {
            const ids = new Set(items.map((x) => String(x.id)));
            let n = items.length + 1;
            while (ids.has(`item-${n}`)) n++;
            setEditing({ index: null, item: { id: `item-${n}`, enabled: true, ...defaultsFor(item, props.root) } });
          }}>{t("Add")}</Button>
        </div>
      )}
      {editing && (
        <Modal
          wide
          title={`${tx.text(props.section.title)}: ${editing.index === null ? t("new") : label(editing.item, editing.index)}`}
          onClose={() => setEditing(null)}
          actions={
            !props.readOnly && (
              <Button variant="primary" onClick={() => {
                props.onChange(editing.index === null ? [...items, editing.item] : items.map((x, j) => (j === editing.index ? editing.item : x)));
                setEditing(null);
              }}>{t("Save")}</Button>
            )
          }
        >
          <fieldset style={{ border: 0, padding: 0, margin: 0, minWidth: 0 }} disabled={props.readOnly}>
            <FieldGrid
              names={fields}
              schema={item}
              root={props.root}
              value={editing.item}
              tx={tx}
              onChange={(v) => setEditing({ ...editing, item: v })}
              render={(name) => (
                <FieldEditor
                  tx={tx}
                  short={props.short}
                  secretAction={props.secretAction}
                  name={name}
                  schema={item}
                  root={props.root}
                  value={editing.item}
                  secrets={props.secrets}
                  error={editing.index === null ? undefined : problemFor(props.problems, `/${editing.index}${pointer(name)}`)}
                  onChange={(v) => setEditing({ ...editing, item: setAt(editing.item, splitPath(name), v) })}
                  onPatch={(patch) => setEditing({ ...editing, item: patchSiblings(editing.item, name, patch) })}
                />
              )}
            />
          </fieldset>
        </Modal>
      )}
    </div>
  );
}

export function Section(props: { title: string; children: ComponentChildren }) {
  return (
    <div class="card">
      <h3 style={{ marginTop: 0 }}>{t(props.title)}</h3>
      {props.children}
    </div>
  );
}
