// LuCI-style editing (D23): a form bar for unsaved edits (Reset / Save / Save & Apply) and a "Changes" counter for
// saved but not applied changes, whose review lists every changed node or section with its diff and offers Apply
// and Revert (all, or per section).

import { useEffect, useState } from "preact/hooks";
import { Badge, Button, Field, Modal, TextInput, ask, msg, t, toast } from "@meduza-np/ui-kit";
import { errorText } from "./api.ts";
import { apply, changedSections, isError, reset, revert, revertSection, save, stagedChanges, unsaved, useConfig, validate, type Preview, type Problem } from "./store.ts";

/** Bottom bar while the page has unsaved form edits. */
export function FormBar(props: { editable: boolean; onReview: () => void }) {
  const s = useConfig();
  const [busy, setBusy] = useState(false);
  if (!s.loaded || !unsaved(s)) return null;
  const problems = s.preview?.problems ?? [];
  const errors = problems.filter(isError);
  const run = async (andApply: boolean) => {
    setBusy(true);
    try {
      await save();
      if (andApply) props.onReview();
      else toast(t("Saved. Apply the changes when you are ready."), "ok");
    } catch (e) {
      toast(errorText(e), "bad");
    } finally {
      setBusy(false);
    }
  };
  return (
    <div class="applybar" role="region" aria-label={t("Unsaved changes")}>
      <strong>{t("Unsaved changes")}</strong>
      {s.newer && <span class="small">{t("{revision} was applied meanwhile; saving merges your edits onto it.", { revision: s.newer })}</span>}
      {errors.length > 0 && <Badge tone="bad">{t("{count} error(s)", { count: errors.length })}</Badge>}
      <span class="spacer" />
      <Button onClick={reset}>{t("Reset")}</Button>
      {props.editable && <Button busy={busy} onClick={() => void run(false)}>{t("Save")}</Button>}
      {props.editable && <Button variant="primary" busy={busy} onClick={() => void run(true)}>{t("Save & Apply")}</Button>}
    </div>
  );
}

/** "Changes: N" button for saved but not applied changes (shown at the top of every page). */
export function ChangesButton(props: { onOpen: () => void }) {
  const s = useConfig();
  if (!s.loaded || !stagedChanges(s)) return null;
  const n = changedSections(s.staged, s.published).length;
  return (
    <Button small onClick={props.onOpen} title={t("Saved but not applied yet")}>
      <Badge tone={s.conflicts.length ? "bad" : "warn"}>{t("Changes: {count}", { count: n })}</Badge>
    </Button>
  );
}

const SECTION_LABELS: Record<string, string> = {
  routing: msg("Global settings: routing and Anycast"),
  mihomo: msg("Global settings: Mihomo subscriptions"),
  defaults: msg("Global settings: node defaults"),
  dns: msg("Global settings: DNS hosts"),
  tinc: msg("Global settings: Tinc mesh"),
  mosdns: msg("Global settings: MosDNS"),
};
const sectionLabel = (k: string) => (k.startsWith("node:") ? k.slice(5) : SECTION_LABELS[k] ? t(SECTION_LABELS[k]!) : k);

/** Review of the staged changes: diff per changed node, problems, Apply / Revert. */
export function ChangesDialog(props: { editable: boolean; onClose: () => void }) {
  const s = useConfig();
  const [preview, setPreview] = useState<Preview | null>(null);
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    void validate(s.staged).then(setPreview);
  }, [s.staged]);
  const sections = changedSections(s.staged, s.published);
  const problems = preview?.problems ?? [];
  const errors = problems.filter(isError);
  const warnings = problems.filter((p) => !isError(p));
  const diffOf = (k: string) => (k.startsWith("node:") ? preview?.nodes.find((n) => n.nodeId === k.slice(5))?.diff : undefined);

  return (
    <Modal
      wide
      title={t("Unapplied changes")}
      onClose={props.onClose}
      actions={
        props.editable && (
          <>
            <Button variant="danger" disabled={busy || !sections.length} onClick={async () => {
              if (!(await ask(t("Revert all saved changes?"), { ok: t("Revert"), danger: true }))) return;
              await revert();
              props.onClose();
            }}>{t("Revert")}</Button>
            <Button variant="primary" busy={busy} disabled={errors.length > 0 || !sections.length} onClick={async () => {
              setBusy(true);
              try {
                const r = await apply(message);
                if (r) {
                  toast(t("Applied as {revision}: {count} node(s) receive new settings", { revision: r.revision, count: r.nodes.filter((n) => n.changed).length }), "ok");
                  props.onClose();
                } else {
                  toast(t("The configuration changed meanwhile; review the conflicting sections and apply again."), "bad");
                }
              } catch (e) {
                toast(errorText(e), "bad");
              } finally {
                setBusy(false);
              }
            }}>{t("Apply")}</Button>
          </>
        )
      }
    >
      {s.conflicts.length > 0 && (
        <div class="callout bad">
          {t("Someone else applied changes to the same sections; your version is kept. Check them before applying: {sections}", { sections: s.conflicts.map((c) => c.replace("/nodes/", "")).join(", ") })}
        </div>
      )}
      {unsaved(s) && <div class="callout info">{t("The page also has unsaved edits; they are not part of these changes until you save them.")}</div>}
      <ProblemList problems={errors} tone="bad" title={t("Fix these before applying:")} />
      <ProblemList problems={warnings} tone="warn" title={t("Warnings (do not block):")} />
      {!sections.length && <p class="muted">{t("No saved changes.")}</p>}
      {sections.map((k) => {
        const diff = diffOf(k);
        return (
          <div>
            <div class="row" style={{ marginTop: "12px" }}>
              <h3 style={{ margin: 0 }}>{sectionLabel(k)}</h3>
              <span class="spacer" />
              {props.editable && <Button small onClick={() => void revertSection(k)}>{t("Revert")}</Button>}
            </div>
            {diff ? (
              <div class="diff" style={{ marginTop: "6px" }}>
                {diff.length === 0 && <div class="muted">{t("No node receives different settings.")}</div>}
                {diff.slice(0, 120).map((d) => (
                  <div class={`diff-${d.op}`}>
                    {d.op === "add" ? "+" : d.op === "remove" ? "−" : "~"} {d.path} {d.op !== "remove" ? JSON.stringify(d.after) : ""}
                  </div>
                ))}
                {diff.length > 120 && <div class="muted">{t("… {count} more", { count: diff.length - 120 })}</div>}
              </div>
            ) : (
              <p class="small muted">{t("Changes the settings every affected node receives (listed under the nodes).")}</p>
            )}
          </div>
        );
      })}
      {props.editable && (
        <Field label={t("Message")}>
          <TextInput value={message} onInput={setMessage} placeholder={t("what and why")} />
        </Field>
      )}
    </Modal>
  );
}

export function ProblemList(props: { problems: Problem[]; tone: "bad" | "warn"; title?: string }) {
  if (!props.problems.length) return null;
  return (
    <div class={`callout ${props.tone}`}>
      {props.title && <strong>{props.title}</strong>}
      <div class="problem-list">
        {props.problems.map((p) => <div>{p.node ? `${p.node}: ` : ""}{p.path.replace(/^\/nodes\/[^/]+/, "") || "/"} — {p.message}</div>)}
      </div>
    </div>
  );
}
