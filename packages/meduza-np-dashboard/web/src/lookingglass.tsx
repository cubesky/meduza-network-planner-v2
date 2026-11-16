import { useState } from "preact/hooks";
import { Badge, Button, Card, Field, Loadable, Select, TextInput, ThemeSelect, t, useAsync, useLocale } from "@meduza-np/ui-kit";
import { get, post, errorText } from "./api.ts";
import { LanguageSelect } from "./language.tsx";

interface Result { command: string; arguments: string[]; exitCode: number | null; timedOut: boolean; truncated: boolean; stdout: string; stderr: string }

export function LookingGlassPage() {
  useLocale();
  const status = useAsync(() => get<{ available: boolean }>("/api/ilg/status"), []);
  const [operation, setOperation] = useState("ping");
  const [target, setTarget] = useState("");
  const [busy, setBusy] = useState(false);
  const [results, setResults] = useState<Result[]>([]);
  const [error, setError] = useState("");
  const run = async (event: Event) => {
    event.preventDefault();
    if (busy || !target.trim()) return;
    setBusy(true); setError(""); setResults([]);
    try { setResults((await post<{ results: Result[] }>("/api/ilg/query", { operation, target: target.trim() })).results); }
    catch (e) { setError(errorText(e)); }
    finally { setBusy(false); }
  };
  return <main style={{ maxWidth: "1000px", margin: "0 auto", padding: "24px" }}>
    <div class="row" style={{ justifyContent: "space-between", marginBottom: "20px" }}>
      <h1>LookingGlass</h1><div class="row"><LanguageSelect /><ThemeSelect /></div>
    </div>
    <Loadable state={status}>{s => s.available ? <Card title={t("Network diagnostics")}>
      <form onSubmit={run}>
        <Field label={t("Diagnostic tool")}><Select value={operation} onChange={setOperation} options={[
          { value: "ping", label: "Ping" }, { value: "mtr", label: "MTR" },
          { value: "iperf", label: "iPerf3" }, { value: "bgp", label: t("BGP routes") },
        ]} /></Field>
        <Field label={t("IP address or ASN")} help={operation === "iperf" ? t("iPerf3 requires a server on TCP port 5201; the test lasts 5 seconds.") : undefined}>
          <TextInput value={target} onInput={setTarget} placeholder={operation === "bgp" ? "AS65000 / 10.0.0.1 / 2001:db8::1" : "10.0.0.1 / 2001:db8::1"} />
        </Field>
        <Button type="submit" variant="primary" busy={busy} disabled={!target.trim()}>{t("Run query")}</Button>
      </form>
    </Card> : <div class="callout info">{t("LookingGlass is not installed.")}</div>}</Loadable>
    {busy && <p role="status">{t("Running network diagnostic…")}</p>}
    {error && <div class="callout bad" role="alert">{error}</div>}
    {results.map((r, i) => <Card key={i} title={`${r.command} ${r.arguments.join(" ")}`}>
      <div class="row"><Badge tone={r.exitCode === 0 ? "ok" : "warn"}>exit {r.exitCode ?? "—"}</Badge>
        {r.timedOut && <Badge tone="warn">{t("Diagnostic timed out")}</Badge>}
        {r.truncated && <Badge tone="warn">{t("Diagnostic output truncated")}</Badge>}
      </div>
      <pre style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>{r.stdout}{r.stderr ? `\n${r.stderr}` : ""}</pre>
    </Card>)}
  </main>;
}
