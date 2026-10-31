import { useState } from "preact/hooks";
import { Button, Card, Loadable, StateBadge, getLocale, t, toast, useAsync } from "@meduza-np/ui-kit";
import { errorText, get, post } from "../api.ts";

interface PendingLogin {
  id: string;
  nodeId: string;
  nodeName: string;
  returnUrl: string;
  expiresAt: string;
  state: "pending" | "approved" | "used" | "expired";
}

/** Approval page for a node dashboard sign-in (dashboard → manager identity → back to the node). */
export function DashboardLoginPage(props: { id: string; role: string }) {
  const state = useAsync(() => get<PendingLogin>(`/api/v1/dashboard-logins/${encodeURIComponent(props.id)}`), [props.id]);
  const [busy, setBusy] = useState(false);

  const decide = async (approve: boolean) => {
    setBusy(true);
    try {
      if (approve) {
        const r = await post<{ redirect: string }>(`/api/v1/dashboard-logins/${encodeURIComponent(props.id)}/approve`);
        location.assign(r.redirect);
        return;
      }
      await post(`/api/v1/dashboard-logins/${encodeURIComponent(props.id)}/deny`);
      toast(t("Denied"), "info");
      location.hash = "/overview";
    } catch (e) {
      toast(errorText(e), "bad");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card title={t("Node dashboard sign-in")}>
      <Loadable state={state}>
        {(l) => (
          <div>
            <p>
              {t("The local dashboard of node {name} ({id}) asks to sign in as you, with your current role {role}.", { name: l.nodeName, id: l.nodeId, role: t(props.role) })}
            </p>
            <p class="small">
              {t("After signing in you return to:")} <code>{l.returnUrl}</code>
              <br />
              {t("Only allow this if that address really is the node dashboard you just opened.")}
            </p>
            <p>
              {t("State")} <StateBadge state={l.state === "pending" ? "ok" : "failed"} label={t(l.state)} />{" "}
              <span class="small muted">{t("valid until {time}", { time: new Date(l.expiresAt).toLocaleString(getLocale()) })}</span>
            </p>
            {l.state === "pending" ? (
              <div style={{ display: "flex", gap: "8px" }}>
                <Button variant="primary" busy={busy} onClick={() => decide(true)}>
                  {t("Allow and return to the node")}
                </Button>
                <Button busy={busy} onClick={() => decide(false)}>
                  {t("Deny")}
                </Button>
              </div>
            ) : (
              <p class="muted">
                {l.state === "expired"
                  ? t("This sign-in request has expired; start again from the node dashboard.")
                  : t("This sign-in request was already handled; start again from the node dashboard.")}
              </p>
            )}
          </div>
        )}
      </Loadable>
    </Card>
  );
}
