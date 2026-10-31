// Published revisions, per-node results and rollback (a rollback publishes an earlier document as a new revision).

import { useState } from "preact/hooks";
import { Badge, Button, Card, Loadable, PageHead, StateBadge, Table, ask, relativeTime, t, toast, useAsync } from "@meduza-np/ui-kit";
import { errorText, get, post } from "../api.ts";
import { reload, stagedChanges, unsaved, useConfig } from "../store.ts";

type Role = "admin" | "operator" | "viewer";

export function HistoryPage(props: { role: Role }) {
  const revisions = useAsync(() => get<Array<Record<string, unknown>>>("/api/v1/revisions"), [], 15_000);
  const [selected, setSelected] = useState<string | null>(null);
  const results = useAsync(() => (selected ? get<Array<Record<string, unknown>>>(`/api/v1/revisions/${selected}/results`) : Promise.resolve([])), [selected], selected ? 5000 : undefined);
  const cfg = useConfig();
  return (
    <div>
      <PageHead title={t("History")} sub={t("Every applied change is a revision; rolling back publishes an earlier one again.")} />
      <div class="split">
        <Card title={t("Revisions")}>
          <Loadable state={revisions} empty={(r) => !r.length} emptyText={t("Nothing published yet")}>
            {(rows) => (
              <Table
                rows={rows}
                rowKey={(r) => String(r.revision)}
                selected={selected}
                onSelect={(r) => setSelected(String(r.revision))}
                columns={[
                  { key: "revision", label: t("Revision"), render: (r) => <span class="mono">{String(r.revision)}</span> },
                  { key: "message", label: t("Message"), render: (r) => (r.kind === "rollback" ? <Badge tone="warn">{t("rollback of {revision}", { revision: String(r.rollback_of) })}</Badge> : String(r.message ?? "")) },
                  { key: "created_by", label: t("By") },
                  { key: "created_at", label: t("When"), render: (r) => <span class="nowrap">{relativeTime(String(r.created_at))}</span> },
                ]}
              />
            )}
          </Loadable>
        </Card>
        <Card
          title={selected ? t("Results of {revision}", { revision: selected }) : t("Results")}
          actions={selected && props.role !== "viewer" && (
            <Button variant="danger" small onClick={async () => {
              const notes = [
                unsaved(cfg) && t("You have unsaved changes; rolling back discards them."),
                stagedChanges(cfg) && t("Your saved changes stay staged and are merged onto the rolled-back revision when you apply them."),
              ].filter(Boolean);
              const question = (
                <>
                  {t("Publish the configuration of {revision} again as a new revision?", { revision: selected })}
                  {notes.map((n) => <span class="callout warn" style={{ display: "block", marginTop: "10px" }}>{n}</span>)}
                </>
              );
              if (!(await ask(question, { title: t("Roll back"), ok: t("Roll back"), danger: true }))) return;
              try {
                const r = await post<{ revision: string }>(`/api/v1/revisions/${selected}/rollback`, {});
                toast(t("Rolled back as {revision}", { revision: r.revision }), "ok");
                revisions.reload();
                await reload();
              } catch (e) {
                toast(errorText(e), "bad");
              }
            }}>{t("Roll back to this")}</Button>
          )}
        >
          {selected ? (
            <Loadable state={results}>
              {(rows) => (
                <Table
                  rows={rows}
                  rowKey={(r) => String(r.node_id)}
                  columns={[
                    { key: "node_id", label: t("Node") },
                    { key: "stage", label: t("Stage"), render: (r) => <StateBadge state={String(r.stage)} /> },
                    { key: "applied_revision", label: t("Applied now"), render: (r) => <span class="mono small">{String(r.applied_revision ?? "—")}</span> },
                    { key: "message", label: t("Message"), render: (r) => <span class="small">{String(r.message ?? r.last_error ?? "")}</span> },
                  ]}
                />
              )}
            </Loadable>
          ) : (
            <div class="empty">{t("Select a revision")}</div>
          )}
        </Card>
      </div>
    </div>
  );
}
