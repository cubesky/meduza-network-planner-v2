// Manager accounts and passkeys (AGENT.md §6.4).

import { useState } from "preact/hooks";
import { Badge, Button, Card, CopyBlock, Field, Loadable, Modal, Select, Table, TextInput, ask, askText, relativeTime, t, toast, useAsync } from "@meduza-np/ui-kit";
import { del, errorText, get, patch, post, registerPasskey, type Session } from "../api.ts";

export function AccountsPage(props: { session: Session }) {
  const admin = props.session.user?.role === "admin";
  return (
    <div>
      <div class="topbar">
        <h1>{t("Accounts & passkeys")}</h1>
      </div>
      <MyPasskeys />
      {admin && <Accounts />}
    </div>
  );
}

function MyPasskeys() {
  const state = useAsync(() => get<Array<Record<string, unknown>>>("/api/v1/auth/passkeys"), []);
  const [name, setName] = useState("");
  return (
    <Card
      title={t("My passkeys")}
      actions={
        <div class="row">
          <TextInput value={name} onInput={setName} placeholder={t("new passkey name")} />
          <Button variant="primary" onClick={async () => {
            try {
              await registerPasskey({ name });
              toast(t("Passkey added"), "ok");
              setName("");
              state.reload();
            } catch (e) {
              toast(errorText(e), "bad");
            }
          }}>{t("Add passkey")}</Button>
        </div>
      }
    >
      <Loadable state={state}>
        {(rows) => (
          <Table
            rows={rows}
            rowKey={(r) => String(r.id)}
            columns={[
              { key: "name", label: t("Name") },
              { key: "device_type", label: t("Type"), render: (r) => (r.backed_up ? <Badge tone="info">{t("synced")}</Badge> : <Badge tone="idle">{String(r.device_type)}</Badge>) },
              { key: "created_at", label: t("Added"), render: (r) => relativeTime(String(r.created_at)) },
              { key: "last_used_at", label: t("Last used"), render: (r) => relativeTime(r.last_used_at as string | null) },
              { key: "revoked_at", label: t("State"), render: (r) => (r.revoked_at ? <Badge tone="bad">{t("revoked")}</Badge> : <Badge tone="ok">{t("active")}</Badge>) },
              {
                key: "actions",
                label: "",
                render: (r) =>
                  !r.revoked_at && (
                    <div class="row">
                      <Button small onClick={async () => {
                        const n = await askText(t("New name"), { title: t("Rename"), value: String(r.name), ok: t("Rename") });
                        if (!n) return;
                        await patch(`/api/v1/auth/passkeys/${encodeURIComponent(String(r.id))}`, { name: n }).catch((e) => toast(errorText(e), "bad"));
                        state.reload();
                      }}>{t("Rename")}</Button>
                      <Button small variant="danger" onClick={async () => {
                        if (!(await ask(t("Revoke this passkey? Sessions created with it end immediately."), { ok: t("Revoke"), danger: true }))) return;
                        try {
                          await del(`/api/v1/auth/passkeys/${encodeURIComponent(String(r.id))}`);
                          state.reload();
                        } catch (e) {
                          toast(errorText(e), "bad");
                        }
                      }}>{t("Revoke")}</Button>
                    </div>
                  ),
              },
            ]}
          />
        )}
      </Loadable>
    </Card>
  );
}

function Accounts() {
  const state = useAsync(() => get<Array<Record<string, unknown>>>("/api/v1/accounts"), []);
  const [inviting, setInviting] = useState(false);
  const [username, setUsername] = useState("");
  const [role, setRole] = useState("operator");
  const [invite, setInvite] = useState<{ inviteUrl: string; expiresAt: string } | null>(null);
  return (
    <Card title={t("Accounts")} actions={<Button onClick={() => setInviting(true)}>{t("Invite account")}</Button>}>
      <Loadable state={state}>
        {(rows) => (
          <Table
            rows={rows}
            rowKey={(r) => String(r.id)}
            columns={[
              { key: "username", label: t("User") },
              { key: "display_name", label: t("Name") },
              {
                key: "role",
                label: t("Role"),
                render: (r) => (
                  <Select value={String(r.role)} options={["admin", "operator", "viewer"].map((v) => ({ value: v, label: t(v) }))} onChange={async (v) => {
                    try {
                      await patch(`/api/v1/accounts/${r.id}`, { role: v });
                      state.reload();
                    } catch (e) {
                      toast(errorText(e), "bad");
                    }
                  }} />
                ),
              },
              { key: "passkeys", label: t("Passkeys") },
              { key: "disabled", label: t("State"), render: (r) => (r.disabled ? <Badge tone="bad">{t("disabled")}</Badge> : <Badge tone="ok">{t("active")}</Badge>) },
              {
                key: "actions",
                label: "",
                render: (r) => (
                  <div class="row">
                    <Button small onClick={async () => {
                      try {
                        setInvite(await post(`/api/v1/accounts/${r.id}/invite`));
                      } catch (e) {
                        toast(errorText(e), "bad");
                      }
                    }}>{t("New invite")}</Button>
                    <Button small onClick={async () => {
                      try {
                        const x = await post<{ revoked: number }>(`/api/v1/accounts/${r.id}/revoke-sessions`);
                        toast(t("{count} session(s) revoked", { count: x.revoked }), "ok");
                      } catch (e) {
                        toast(errorText(e), "bad");
                      }
                    }}>{t("End sessions")}</Button>
                    <Button small variant={r.disabled ? undefined : "danger"} onClick={async () => {
                      try {
                        await patch(`/api/v1/accounts/${r.id}`, { disabled: !r.disabled });
                        state.reload();
                      } catch (e) {
                        toast(errorText(e), "bad");
                      }
                    }}>{r.disabled ? t("Enable") : t("Disable")}</Button>
                  </div>
                ),
              },
            ]}
          />
        )}
      </Loadable>
      {inviting && (
        <Modal
          title={t("Invite account")}
          onClose={() => setInviting(false)}
          actions={
            <Button variant="primary" disabled={!username} onClick={async () => {
              try {
                setInvite(await post("/api/v1/accounts", { username, role }));
                setInviting(false);
                state.reload();
              } catch (e) {
                toast(errorText(e), "bad");
              }
            }}>{t("Create invite")}</Button>
          }
        >
          <Field label={t("Username")}><TextInput value={username} onInput={setUsername} /></Field>
          <Field label={t("Role")} help={t("Manager roles are managed here only; LDAP groups never grant them.")}>
            <Select value={role} options={["viewer", "operator", "admin"].map((v) => ({ value: v, label: t(v) }))} onChange={setRole} />
          </Field>
        </Modal>
      )}
      {invite && (
        <Modal title={t("Invitation link")} onClose={() => setInvite(null)}>
          <p>{t("Send this single-use link to the user (expires {time}):", { time: invite.expiresAt })}</p>
          <CopyBlock text={invite.inviteUrl} />
        </Modal>
      )}
    </Card>
  );
}
