import { useState } from "preact/hooks";
import { Card, Loadable, Table, TextInput, relativeTime, t, useAsync } from "@meduza-np/ui-kit";
import { get } from "../api.ts";

export function AuditPage() {
  const state = useAsync(() => get<Array<Record<string, unknown>>>("/api/v1/audit"), [], 30_000);
  const [q, setQ] = useState("");
  return (
    <div>
      <div class="topbar">
        <h1>{t("Audit")}</h1>
        <TextInput value={q} onInput={setQ} placeholder={t("filter")} />
      </div>
      <Loadable state={state}>
        {(rows) => (
          <Card>
            <Table
              rows={rows.filter((r) => !q || JSON.stringify(r).toLowerCase().includes(q.toLowerCase()))}
              rowKey={(r) => String(r.id)}
              columns={[
                { key: "at", label: t("When"), render: (r) => <span title={String(r.at)}>{relativeTime(String(r.at))}</span> },
                { key: "actor", label: t("Actor") },
                { key: "action", label: t("Action") },
                { key: "target", label: t("Target") },
                { key: "detail", label: t("Detail (redacted)"), render: (r) => <code class="small">{JSON.stringify(r.detail)}</code> },
              ]}
            />
          </Card>
        )}
      </Loadable>
    </div>
  );
}
