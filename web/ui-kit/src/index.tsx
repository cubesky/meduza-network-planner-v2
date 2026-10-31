// Shared Preact controls for the manager and the node dashboard. No business logic here.

import { render, type ComponentChildren, type JSX } from "preact";
import { useEffect, useId, useLayoutEffect, useRef, useState } from "preact/hooks";
import { getLocale, t } from "./i18n.ts";
import "./messages.ts";

export * from "./i18n.ts";
export * from "./theme.tsx";
import { applyTheme } from "./theme.tsx";

applyTheme();

export type Tone = "ok" | "warn" | "bad" | "info" | "idle";

/** Maps generic service/feature states (§12.2) to a tone. Unknown/never-reported is never shown as ok. */
export function stateTone(state: string | null | undefined): Tone {
  switch (state) {
    case "up":
    case "in-sync":
    case "online":
    case "healthy":
    case "registered":
    case "applied":
    case "succeeded":
      return "ok";
    case "degraded":
    case "behind":
    case "applying":
    case "stale":
    case "pending":
    case "starting":
    case "validated":
    case "received":
    case "delivered":
      return "warn";
    case "down":
    case "apply-failed":
    case "credential-error":
    case "offline":
    case "failed":
    case "rejected":
    case "conflict":
    case "invalid":
    case "incompatible":
    case "missing-dependency":
    case "revoked":
      return "bad";
    case "disabled":
    case "unsupported":
    case "never":
    case "not-enrolled":
    case "unchanged":
      return "idle";
    default:
      return "info";
  }
}

export function Badge(props: { tone?: Tone; children: ComponentChildren; title?: string }) {
  return (
    <span class={`badge ${props.tone ?? "info"}`} title={props.title}>
      {props.children}
    </span>
  );
}

export function StateBadge(props: { state: string | null | undefined; label?: string }) {
  const s = props.state ?? "unknown";
  return <Badge tone={stateTone(s)}>{props.label ?? t(s)}</Badge>;
}

/** A coloured dot for a state; the state word is the tooltip. */
export function StatusDot(props: { state: string | null | undefined }) {
  const s = props.state ?? "unknown";
  return <span class={`dot ${stateTone(s)}`} title={t(s)} />;
}

/** Page title with optional breadcrumb, subtitle and actions. */
export function PageHead(props: { title: ComponentChildren; sub?: ComponentChildren; crumbs?: ComponentChildren; actions?: ComponentChildren }) {
  return (
    <div class="page-head">
      <div>
        {props.crumbs && <div class="crumbs">{props.crumbs}</div>}
        <h1>{props.title}</h1>
        {props.sub && <div class="sub">{props.sub}</div>}
      </div>
      {props.actions && <div class="row">{props.actions}</div>}
    </div>
  );
}

/** Collapsible panel: the head is a button-like row, the body renders only while open. */
export function Panel(props: { title: ComponentChildren; badges?: ComponentChildren; actions?: ComponentChildren; open: boolean; onToggle: () => void; children: ComponentChildren }) {
  return (
    <section class={`panel${props.open ? " open" : ""}`}>
      <div class="panel-head" role="button" tabIndex={0} onClick={props.onToggle} onKeyDown={(e) => (e.key === "Enter" || e.key === " ") && props.onToggle()}>
        <span class="chev">▸</span>
        <span class="title">{props.title}</span>
        {props.badges}
        <span class="spacer" />
        {/* actions do not toggle the panel */}
        <span class="row" onClick={(e) => e.stopPropagation()}>{props.actions}</span>
      </div>
      {props.open && <div class="panel-body">{props.children}</div>}
    </section>
  );
}

export function Button(props: JSX.ButtonHTMLAttributes<HTMLButtonElement> & { variant?: "primary" | "danger"; small?: boolean; busy?: boolean }) {
  const { variant, small, busy, children, class: cls, disabled, ...rest } = props;
  return (
    <button type="button" {...rest} disabled={disabled || busy} class={["btn", variant, small ? "small" : "", cls].filter(Boolean).join(" ")}>
      {busy ? "…" : children}
    </button>
  );
}

export function Card(props: { title?: ComponentChildren; actions?: ComponentChildren; children: ComponentChildren }) {
  return (
    <section class="card">
      {(props.title || props.actions) && (
        <div class="row" style={{ marginBottom: "8px" }}>
          {props.title && <h2 style={{ margin: 0 }}>{props.title}</h2>}
          <span class="spacer" />
          {props.actions}
        </div>
      )}
      {props.children}
    </section>
  );
}

/**
 * A labelled control. The label is linked to the first input/select/textarea inside (clicking it focuses the
 * control, screen readers announce it), and the help or error text is attached as its description.
 */
export function Field(props: { label: ComponentChildren; help?: ComponentChildren; error?: string; wide?: boolean; children: ComponentChildren }) {
  const id = useId();
  const ref = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const el = ref.current;
    const control = el?.querySelector<HTMLElement>("input, select, textarea");
    const label = el?.querySelector<HTMLLabelElement>(":scope > label");
    if (!control || !label) return;
    if (!control.id) control.id = id;
    label.htmlFor = control.id;
    if (el!.querySelector(":scope > .note")) control.setAttribute("aria-describedby", `${id}-note`);
    else control.removeAttribute("aria-describedby");
  });
  return (
    <div ref={ref} class={["field", props.wide ? "wide" : "", props.error ? "has-error" : ""].filter(Boolean).join(" ")}>
      <label>{props.label}</label>
      {props.children}
      {props.error ? <div class="note error" id={`${id}-note`}>{props.error}</div> : props.help ? <div class="note help" id={`${id}-note`}>{props.help}</div> : null}
    </div>
  );
}

/** An on/off setting: the checkbox and its label on one line, the explanation underneath. */
export function ToggleField(props: { label: ComponentChildren; help?: ComponentChildren; error?: string; checked: boolean; onChange: (v: boolean) => void; disabled?: boolean }) {
  const id = useId();
  const note = props.error ?? props.help;
  return (
    <div class={["field", "toggle", props.checked ? "on" : "", props.error ? "has-error" : ""].filter(Boolean).join(" ")}>
      <label class="check">
        <input
          type="checkbox"
          checked={props.checked}
          disabled={props.disabled}
          aria-describedby={note ? `${id}-note` : undefined}
          onChange={(e) => props.onChange((e.target as HTMLInputElement).checked)}
        />
        <span>{props.label}</span>
      </label>
      {note && <div class={`note ${props.error ? "error" : "help"}`} id={`${id}-note`}>{note}</div>}
    </div>
  );
}

/** A titled block of related fields inside a form, optionally switched on and off as a whole. */
export function FieldGroup(props: { title: ComponentChildren; help?: ComponentChildren; enabled?: boolean; onToggle?: (v: boolean) => void; children?: ComponentChildren }) {
  const switchable = props.onToggle !== undefined;
  const open = !switchable || props.enabled;
  return (
    <div class={`field-group${open ? "" : " off"}`}>
      <div class="field-group-head">
        {switchable ? (
          <label class="check">
            <input type="checkbox" checked={!!props.enabled} onChange={(e) => props.onToggle!((e.target as HTMLInputElement).checked)} />
            <span>{props.title}</span>
          </label>
        ) : (
          <span class="field-group-title">{props.title}</span>
        )}
        {props.help && <div class="note help">{props.help}</div>}
      </div>
      {open && props.children}
    </div>
  );
}

export function TextInput(props: { value: string | undefined | null; onInput: (v: string) => void; placeholder?: string; invalid?: boolean; type?: string; disabled?: boolean; mono?: boolean }) {
  return (
    <input
      type={props.type ?? "text"}
      class={[props.invalid ? "invalid" : "", props.mono ? "mono" : ""].join(" ")}
      value={props.value ?? ""}
      placeholder={props.placeholder}
      disabled={props.disabled}
      onInput={(e) => props.onInput((e.target as HTMLInputElement).value)}
    />
  );
}

export function NumberInput(props: { value: number | undefined | null; onInput: (v: number | undefined) => void; min?: number; max?: number; placeholder?: string; invalid?: boolean }) {
  return (
    <input
      type="number"
      class={props.invalid ? "invalid" : ""}
      value={props.value ?? ""}
      min={props.min}
      max={props.max}
      placeholder={props.placeholder}
      onInput={(e) => {
        const raw = (e.target as HTMLInputElement).value;
        props.onInput(raw === "" ? undefined : Number(raw));
      }}
    />
  );
}

export function Checkbox(props: { checked: boolean; onChange: (v: boolean) => void; label?: string }) {
  return (
    <label class="check">
      <input type="checkbox" checked={props.checked} onChange={(e) => props.onChange((e.target as HTMLInputElement).checked)} />
      {props.label}
    </label>
  );
}

export function Select(props: { value: string | undefined | null; options: Array<string | { value: string; label: string }>; onChange: (v: string) => void; allowEmpty?: boolean; emptyLabel?: string; invalid?: boolean }) {
  return (
    <select class={props.invalid ? "invalid" : ""} value={props.value ?? ""} onChange={(e) => props.onChange((e.target as HTMLSelectElement).value)}>
      {props.allowEmpty && <option value="">{props.emptyLabel ?? "—"}</option>}
      {props.options.map((o) => (typeof o === "string" ? <option value={o}>{o}</option> : <option value={o.value}>{o.label}</option>))}
    </select>
  );
}

/** One value per line (CIDR lists, host lists). Empty lines are dropped. */
export function ListInput(props: { value: string[] | undefined; onChange: (v: string[]) => void; placeholder?: string; invalid?: boolean }) {
  const [text, setText] = useState((props.value ?? []).join("\n"));
  useEffect(() => {
    const joined = (props.value ?? []).join("\n");
    if (joined !== text.split("\n").map((l) => l.trim()).filter(Boolean).join("\n")) setText(joined);
  }, [props.value]);
  return (
    <textarea
      class={props.invalid ? "invalid" : ""}
      style={{ minHeight: "64px" }}
      value={text}
      placeholder={props.placeholder}
      onInput={(e) => {
        const t = (e.target as HTMLTextAreaElement).value;
        setText(t);
        props.onChange(t.split("\n").map((l) => l.trim()).filter(Boolean));
      }}
    />
  );
}

export interface Column<T> {
  key: string;
  label: string;
  render?: (row: T) => ComponentChildren;
}

export function Table<T extends Record<string, unknown>>(props: { columns: Column<T>[]; rows: T[]; rowKey: (r: T) => string; selected?: string | null; onSelect?: (r: T) => void; empty?: string }) {
  if (!props.rows.length) return <div class="state-box">{props.empty ?? t("No entries")}</div>;
  return (
    <div class="table-wrap">
      <table class="table">
        <thead>
          <tr>{props.columns.map((c) => <th>{c.label}</th>)}</tr>
        </thead>
        <tbody>
          {props.rows.map((r) => {
            const key = props.rowKey(r);
            return (
              <tr key={key} class={props.selected === key ? "selected" : ""} style={props.onSelect ? { cursor: "pointer" } : undefined} onClick={() => props.onSelect?.(r)}>
                {props.columns.map((c) => <td>{c.render ? c.render(r) : formatCell(r[c.key])}</td>)}
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

export function formatCell(v: unknown): ComponentChildren {
  if (v === null || v === undefined || v === "") return <span class="muted">—</span>;
  if (typeof v === "boolean") return v ? t("yes") : t("no");
  if (Array.isArray(v)) return v.map(String).join(", ");
  if (typeof v === "object") return <code>{JSON.stringify(v)}</code>;
  return String(v);
}

/** Tab strip; a tab's `count` is a neutral number, or with `tone: "bad"` a problem count that stands out. */
export function Tabs(props: { tabs: Array<{ id: string; label: string; count?: number; tone?: "bad" | "warn" }>; active: string; onChange: (id: string) => void }) {
  return (
    <div class="tabs" role="tablist">
      {props.tabs.map((t) => (
        <button type="button" role="tab" aria-selected={t.id === props.active} class={t.id === props.active ? "active" : ""} onClick={() => props.onChange(t.id)}>
          {t.label}
          {t.count !== undefined && <span class={t.tone ? `count ${t.tone}` : "count"}>{t.count}</span>}
        </button>
      ))}
    </div>
  );
}

/**
 * Dialog over the page. Escape and a click on the backdrop close it; focus moves into it on open (the first input,
 * else the first action) and returns to where it was on close.
 */
export function Modal(props: { title: string; onClose: () => void; children: ComponentChildren; actions?: ComponentChildren; wide?: boolean; closeLabel?: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const close = useRef(props.onClose);
  close.current = props.onClose;
  useEffect(() => {
    const before = document.activeElement as HTMLElement | null;
    const box = ref.current;
    const first = box?.querySelector<HTMLElement>("input:not([disabled]):not([type=hidden]), select:not([disabled]), textarea:not([disabled])")
      ?? box?.querySelector<HTMLElement>(".modal-actions button:not([disabled])");
    first?.focus();
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && close.current();
    addEventListener("keydown", onKey);
    return () => {
      removeEventListener("keydown", onKey);
      before?.focus?.();
    };
  }, []);
  return (
    <div class="modal-back" onClick={(e) => e.target === e.currentTarget && props.onClose()}>
      <div ref={ref} class={props.wide ? "modal wide" : "modal"} role="dialog" aria-modal="true" aria-label={props.title}>
        <h2 style={{ marginTop: 0 }}>{props.title}</h2>
        {props.children}
        <div class="row modal-actions">
          <Button onClick={props.onClose}>{props.closeLabel ?? t("Close")}</Button>
          {props.actions}
        </div>
      </div>
    </div>
  );
}

/**
 * In-page replacement for `window.confirm`: resolves true when the user confirms. `danger` colours the confirm
 * button for destructive actions; `ok` names it after the action ("Revoke", "Restart").
 */
export function ask(message: ComponentChildren, opts: { title?: string; ok?: string; danger?: boolean } = {}): Promise<boolean> {
  return new Promise((resolve) => {
    const host = document.body.appendChild(document.createElement("div"));
    const done = (v: boolean) => {
      render(null, host);
      host.remove();
      resolve(v);
    };
    render(
      <Modal title={opts.title ?? t("Are you sure?")} onClose={() => done(false)} closeLabel={t("Cancel")}
        actions={<Button variant={opts.danger ? "danger" : "primary"} onClick={() => done(true)}>{opts.ok ?? t("Confirm")}</Button>}>
        <p class="ask-text">{message}</p>
      </Modal>,
      host,
    );
  });
}

/** In-page replacement for `window.prompt`: resolves the entered text, or null when cancelled. */
export function askText(label: string, opts: { title?: string; value?: string; ok?: string; placeholder?: string; mono?: boolean } = {}): Promise<string | null> {
  return new Promise((resolve) => {
    const host = document.body.appendChild(document.createElement("div"));
    const done = (v: string | null) => {
      render(null, host);
      host.remove();
      resolve(v);
    };
    function Body() {
      const [v, setV] = useState(opts.value ?? "");
      const submit = () => v.trim() && done(v.trim());
      return (
        <Modal title={opts.title ?? label} onClose={() => done(null)} closeLabel={t("Cancel")}
          actions={<Button variant="primary" disabled={!v.trim()} onClick={submit}>{opts.ok ?? t("OK")}</Button>}>
          <form onSubmit={(e) => { e.preventDefault(); submit(); }}>
            <Field label={label}><TextInput value={v} onInput={setV} placeholder={opts.placeholder} mono={opts.mono} /></Field>
          </form>
        </Modal>
      );
    }
    render(<Body />, host);
  });
}

/** Copies `text` to the clipboard and says so on the button for a moment. */
export function CopyButton(props: { text: string; label?: string }) {
  const [done, setDone] = useState(false);
  return (
    <Button small class="copy-btn" onClick={async () => {
      try {
        await navigator.clipboard.writeText(props.text);
      } catch {
        // http:// (non-secure) pages have no async clipboard API
        const area = document.body.appendChild(document.createElement("textarea"));
        area.value = props.text;
        area.select();
        document.execCommand("copy");
        area.remove();
      }
      setDone(true);
      setTimeout(() => setDone(false), 1500);
    }}>{done ? t("Copied") : props.label ?? t("Copy")}</Button>
  );
}

/** A command or value to paste somewhere else: monospace block with a copy button. */
export function CopyBlock(props: { text: string }) {
  return (
    <div class="copy-block">
      <pre>{props.text}</pre>
      <CopyButton text={props.text} />
    </div>
  );
}

/** Loading / empty / error / stale states instead of infinite spinners (§6.2). */
export function Loadable<T>(props: { state: AsyncState<T>; children: (data: T) => ComponentChildren; empty?: (data: T) => boolean; emptyText?: string }) {
  const s = props.state;
  if (s.error && s.data === undefined) return <div class="state-box" style={{ color: "var(--bad)" }}>{t("Failed to load: {error}", { error: s.error })}</div>;
  if (s.data === undefined) return <div class="state-box">{t("Loading…")}</div>;
  if (props.empty?.(s.data)) return <div class="state-box">{props.emptyText ?? t("Nothing here yet")}</div>;
  return (
    <>
      {s.error && <div class="small" style={{ color: "var(--warn)" }}>{t("Showing last loaded data — refresh failed: {error}", { error: s.error })}</div>}
      {props.children(s.data)}
    </>
  );
}

export interface AsyncState<T> {
  data?: T;
  error?: string;
  loading: boolean;
  reload: () => void;
}

/** Fetches with `load`, optionally re-polling every `intervalMs`. Keeps the last data on refresh failures. */
export function useAsync<T>(load: () => Promise<T>, deps: unknown[], intervalMs?: number): AsyncState<T> {
  const [state, setState] = useState<{ data?: T; error?: string; loading: boolean }>({ loading: true });
  const [tick, setTick] = useState(0);
  useEffect(() => {
    let alive = true;
    setState((s) => ({ ...s, loading: true }));
    load().then(
      (data) => alive && setState({ data, loading: false }),
      (e: unknown) => alive && setState((s) => ({ data: s.data, error: e instanceof Error ? e.message : String(e), loading: false })),
    );
    const timer = intervalMs ? setTimeout(() => alive && setTick((t) => t + 1), intervalMs) : undefined;
    return () => {
      alive = false;
      if (timer) clearTimeout(timer);
    };
  }, [...deps, tick]);
  return { ...state, reload: () => setTick((t) => t + 1) };
}

// ---- toasts -------------------------------------------------------------------------------------------

type ToastItem = { id: number; text: string; tone: "ok" | "bad" | "info" };
let pushToast: (t: Omit<ToastItem, "id">) => void = () => undefined;

export function toast(text: string, tone: "ok" | "bad" | "info" = "info"): void {
  pushToast({ text, tone });
}

export function ToastHost() {
  const [items, setItems] = useState<ToastItem[]>([]);
  useEffect(() => {
    let n = 0;
    pushToast = (t) => {
      const id = ++n;
      setItems((xs) => [...xs, { ...t, id }]);
      setTimeout(() => setItems((xs) => xs.filter((x) => x.id !== id)), t.tone === "bad" ? 9000 : 4000);
    };
  }, []);
  return (
    <div class="toast-stack" aria-live="polite">
      {items.map((t) => <div class={`toast ${t.tone}`}>{t.text}</div>)}
    </div>
  );
}

export function relativeTime(iso: string | null | undefined, now = Date.now()): string {
  if (!iso) return t("never");
  const s = Math.round((now - Date.parse(iso)) / 1000);
  if (Number.isNaN(s)) return iso;
  if (s < 5) return t("just now");
  const f = new Intl.RelativeTimeFormat(getLocale(), { numeric: "always", style: "narrow" });
  if (s < 60) return f.format(-s, "second");
  if (s < 3600) return f.format(-Math.floor(s / 60), "minute");
  if (s < 86400) return f.format(-Math.floor(s / 3600), "hour");
  return f.format(-Math.floor(s / 86400), "day");
}

export function bytes(n: number | null | undefined): string {
  if (n === null || n === undefined) return "—";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(i ? 1 : 0)} ${units[i]}`;
}
