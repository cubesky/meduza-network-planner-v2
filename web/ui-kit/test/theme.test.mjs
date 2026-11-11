import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import { test } from "node:test";
import assert from "node:assert/strict";
import ts from "typescript";

const bootstrap = readFileSync(new URL("../src/theme-init.js", import.meta.url), "utf8");
const runtime = ts.transpileModule(readFileSync(new URL("../src/theme.tsx", import.meta.url), "utf8"), {
  compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
}).outputText;

function setup(value, blocked = false) {
  const stored = new Map(value === undefined ? [] : [["mnp.theme", value]]);
  const document = { documentElement: { dataset: {} } };
  const localStorage = {
    getItem(key) { if (blocked) throw new Error("storage disabled"); return stored.get(key) ?? null; },
    setItem(key, value) { if (blocked) throw new Error("storage disabled"); stored.set(key, value); },
    removeItem(key) { if (blocked) throw new Error("storage disabled"); stored.delete(key); },
  };
  const exports = {};
  const context = { document, localStorage, exports, require: () => ({}) };
  runInNewContext(runtime, context);
  return { stored, document, context, api: exports };
}

test("theme bootstrap restores explicit choices before the app and agrees with the runtime storage key", () => {
  for (const theme of ["dark", "light"]) {
    const { stored, document, context, api } = setup(theme);
    runInNewContext(bootstrap, context);
    assert.equal(document.documentElement.dataset.theme, theme);
    assert.equal(api.getTheme(), theme);
    api.setTheme(theme === "dark" ? "light" : "dark");
    delete document.documentElement.dataset.theme;
    runInNewContext(bootstrap, context); // full document reload with the same preference
    assert.equal(document.documentElement.dataset.theme, stored.get("mnp.theme"));
  }
});

test("system, missing and invalid preferences leave CSS in charge of the OS theme", () => {
  for (const value of [undefined, "system", "invalid"]) {
    const { document, context, api } = setup(value);
    document.documentElement.dataset.theme = "light";
    runInNewContext(bootstrap, context);
    assert.equal(document.documentElement.dataset.theme, undefined);
    assert.equal(api.getTheme(), "system");
    api.setTheme("dark");
    api.setTheme("system");
    assert.equal(document.documentElement.dataset.theme, undefined);
    runInNewContext(bootstrap, context);
    assert.equal(document.documentElement.dataset.theme, undefined);
  }
});

test("disabled storage does not prevent startup or changing the current page's theme", () => {
  const { document, context, api } = setup(undefined, true);
  assert.doesNotThrow(() => runInNewContext(bootstrap, context));
  assert.equal(api.getTheme(), "system");
  api.setTheme("dark");
  assert.equal(document.documentElement.dataset.theme, "dark");
});
