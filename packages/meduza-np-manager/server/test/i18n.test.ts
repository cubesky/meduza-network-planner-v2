// Interface translations are complete: every message the web code passes to t()/msg() has a zh-Hans and a zh-Hant
// entry with the same {placeholders}, every package descriptor text is translated, and the account language setting
// round-trips through the API.

import { test } from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { BOOTSTRAP_TOKEN, ORIGIN, REPO_ROOT, SoftAuthenticator, cookieFrom, startManager } from "./helpers.js";

const LOCALES = ["zh-Hans", "zh-Hant"] as const;

function keysOf(dir: string): Set<string> {
  const keys = new Set<string>();
  const walk = (d: string) => {
    for (const e of readdirSync(d, { withFileTypes: true })) {
      const p = join(d, e.name);
      if (e.isDirectory()) walk(p);
      else if (/\.tsx?$/.test(e.name)) {
        for (const m of readFileSync(p, "utf8").matchAll(/\b(?:t|msg)\(\s*"((?:[^"\\]|\\.)*)"/g)) keys.add(JSON.parse(`"${m[1]}"`) as string);
      }
    }
  };
  walk(dir);
  return keys;
}

const dict = (dir: string, l: string) => JSON.parse(readFileSync(join(dir, "locales", `${l}.json`), "utf8")) as Record<string, string>;
const placeholders = (s: string) => [...s.matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort().join(",");

test("i18n: every web message has zh-Hans and zh-Hant translations with the same placeholders", () => {
  const uiKit = join(REPO_ROOT, "web/ui-kit/src");
  const renderer = join(REPO_ROOT, "web/descriptor-renderer/src");
  const manager = join(REPO_ROOT, "packages/meduza-np-manager/web/src");
  // A library's messages must be in its own dictionaries (the node dashboard loads them without the manager's).
  const trees: Array<[string, string[]]> = [
    [uiKit, [uiKit]],
    [renderer, [renderer, uiKit]],
    [manager, [manager, renderer, uiKit]],
  ];
  const missing: string[] = [];
  for (const [src, dicts] of trees) {
    for (const l of LOCALES) {
      const merged = Object.assign({}, ...dicts.map((d) => dict(d, l))) as Record<string, string>;
      for (const k of keysOf(src)) {
        const v = merged[k];
        if (v === undefined) missing.push(`${l}: ${JSON.stringify(k)} (${src.slice(REPO_ROOT.length)})`);
        else if (placeholders(v) !== placeholders(k)) missing.push(`${l}: placeholders differ for ${JSON.stringify(k)}`);
      }
    }
  }
  assert.deepEqual(missing, []);
});

test("i18n: package descriptors translate their texts and field labels", () => {
  const problems: string[] = [];
  for (const pkg of readdirSync(join(REPO_ROOT, "packages"))) {
    const f = join(REPO_ROOT, "packages", pkg, "ui", "descriptor.json");
    if (!existsSync(f)) continue;
    const d = JSON.parse(readFileSync(f, "utf8")) as Record<string, unknown> & { i18n?: Record<string, { text?: Record<string, string>; fields?: Record<string, string> }> };
    const texts = new Set<string>();
    const fields = new Set<string>();
    const walk = (o: unknown, k?: string) => {
      if (Array.isArray(o)) for (const x of o) k === "fields" || k === "columns" ? fields.add(String(x)) : walk(x);
      else if (o && typeof o === "object") for (const [kk, v] of Object.entries(o)) kk !== "i18n" && walk(v, kk);
      else if (typeof o === "string" && ["title", "description", "label", "confirm"].includes(k ?? "")) texts.add(o);
    };
    walk(d);
    const field = (tr: { fields?: Record<string, string> } | undefined, name: string) => tr?.fields?.[name] ?? tr?.fields?.[name.split(".").pop()!];
    for (const l of LOCALES) {
      for (const s of texts) if (!d.i18n?.[l]?.text?.[s]) problems.push(`${pkg} ${l} text ${JSON.stringify(s)}`);
      for (const n of fields) if (!field(d.i18n?.[l], n)) problems.push(`${pkg} ${l} field ${n}`);
    }
    // Texts written in Chinese need an English rendering.
    for (const s of texts) if (/[㐀-鿿]/.test(s) && !d.i18n?.en?.text?.[s]) problems.push(`${pkg} en text ${JSON.stringify(s)}`);
  }
  assert.deepEqual(problems, []);
});

test("i18n: the account language is stored per account and validated", async () => {
  const { app } = await startManager();
  const base = { origin: ORIGIN, "content-type": "application/json" };
  const authn = new SoftAuthenticator();
  const opt = await app.inject({ method: "POST", url: "/api/v1/auth/passkeys/register/options", headers: base, payload: { bootstrapToken: BOOTSTRAP_TOKEN, username: "alice" } });
  const { challengeId, options } = opt.json();
  const ver = await app.inject({ method: "POST", url: "/api/v1/auth/passkeys/register/verify", headers: base, payload: { challengeId, response: authn.register(options), name: "laptop" } });
  const cookie = cookieFrom(ver);
  const csrf = (await app.inject({ method: "GET", url: "/api/v1/auth/session", headers: { cookie } })).json().csrfToken as string;
  const headers = { ...base, cookie, "x-csrf-token": csrf };
  const before = (await app.inject({ method: "GET", url: "/api/v1/auth/session", headers })).json();
  assert.deepEqual(before.user.settings, {}, "follows the browser until chosen");
  const set = await app.inject({ method: "PUT", url: "/api/v1/account/settings", headers, payload: { locale: "zh-Hant" } });
  assert.equal(set.statusCode, 200, set.body);
  assert.equal((await app.inject({ method: "GET", url: "/api/v1/auth/session", headers })).json().user.settings.locale, "zh-Hant");
  const bad = await app.inject({ method: "PUT", url: "/api/v1/account/settings", headers, payload: { locale: "fr" } });
  assert.equal(bad.statusCode, 400);
  await app.inject({ method: "PUT", url: "/api/v1/account/settings", headers, payload: { locale: null } });
  assert.deepEqual((await app.inject({ method: "GET", url: "/api/v1/auth/session", headers })).json().user.settings, {}, "back to the browser language");
  const anonymous = await app.inject({ method: "PUT", url: "/api/v1/account/settings", headers: base, payload: { locale: "en" } });
  assert.equal(anonymous.statusCode, 401);
  await app.close();
});
