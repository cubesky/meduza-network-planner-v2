import { useState } from "preact/hooks";
import { Button, Card, Field, TextInput, t, toast } from "@meduza-np/ui-kit";
import { errorText, login, registerPasskey, type Session } from "../api.ts";
import { LanguageSelect } from "../language.tsx";

export function LoginPage(props: { session: Session; hash: string; onSignedIn: () => void }) {
  const invite = props.hash.startsWith("/invite/") ? props.hash.slice("/invite/".length) : null;
  const [busy, setBusy] = useState(false);
  const [token, setToken] = useState("");
  const [username, setUsername] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [keyName, setKeyName] = useState("");

  const run = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await fn();
      location.hash = "/overview";
      props.onSignedIn();
    } catch (e) {
      toast(errorText(e), "bad");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div style={{ maxWidth: "460px", margin: "8vh auto", padding: "0 16px" }}>
      <div class="row">
        <h1>Meduza NP Manager</h1>
        <span class="spacer" />
        {/* no account yet: the choice is kept in this browser */}
        <LanguageSelect />
      </div>
      {invite ? (
        <Card title={t("Accept invitation")}>
          <p class="muted">{t("Register a passkey for your new account. Your device will ask for biometric or PIN verification.")}</p>
          <Field label={t("Passkey name")} help={t("e.g. work laptop")}>
            <TextInput value={keyName} onInput={setKeyName} />
          </Field>
          <Button variant="primary" busy={busy} onClick={() => run(() => registerPasskey({ inviteToken: invite, name: keyName }))}>
            {t("Create passkey")}
          </Button>
        </Card>
      ) : (
        <Card title={t("Sign in")}>
          <p class="muted">{t("Sign in with a passkey registered for your manager account. There is no password login.")}</p>
          <Button variant="primary" busy={busy} onClick={() => run(login)}>
            {t("Sign in with passkey")}
          </Button>
        </Card>
      )}
      {props.session.bootstrap === "open" && !invite && (
        <Card title={t("First administrator")}>
          <p class="muted">
            {t("Enter the one-time bootstrap token from the server ({file}). This entry closes after the first administrator registers.", { file: "BOOTSTRAP_TOKEN_FILE" })}
          </p>
          <Field label={t("Bootstrap token")}>
            <TextInput type="password" value={token} onInput={setToken} />
          </Field>
          <Field label={t("Username")}>
            <TextInput value={username} onInput={setUsername} placeholder="admin" />
          </Field>
          <Field label={t("Display name")}>
            <TextInput value={displayName} onInput={setDisplayName} />
          </Field>
          <Field label={t("Passkey name")}>
            <TextInput value={keyName} onInput={setKeyName} />
          </Field>
          <Button busy={busy} disabled={!token || !username} onClick={() => run(() => registerPasskey({ bootstrapToken: token, username, displayName, name: keyName }))}>
            {t("Register administrator passkey")}
          </Button>
        </Card>
      )}
      {props.session.bootstrap === "expired" && (
        <p class="small muted">{t("The bootstrap window has expired. Use the host recovery command to issue a recovery invite.")}</p>
      )}
    </div>
  );
}
