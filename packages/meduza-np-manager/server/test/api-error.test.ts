import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { ModuleKind, ScriptTarget, transpileModule } from "typescript";
import { REPO_ROOT } from "./helpers.js";

const source = readFileSync(join(REPO_ROOT, "packages/meduza-np-manager/web/src/api-error.ts"), "utf8");
const compiled = transpileModule(source, { compilerOptions: { target: ScriptTarget.ES2023, module: ModuleKind.ESNext } }).outputText;
const { ApiError, errorText } = await import(`data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`);

test("API errors show the LDAP 422 textual problem instead of undefined fields", () => {
  const problem = "plain ldap:// requires StartTLS; passwords are never sent in clear text";
  assert.equal(errorText(new ApiError(422, "invalid-settings", "Unprocessable Entity", {
    error: "invalid-settings", problems: [problem],
  })), `Unprocessable Entity: ${problem}`);
});

test("API errors retain field paths and accept mixed text or pathless problems", () => {
  assert.equal(errorText(new ApiError(422, "invalid-settings", "Validation failed", { problems: [
    "text problem", { path: "/nodes/a", message: "invalid node" }, { message: "missing setting" }, "fourth problem",
  ] })), "Validation failed: text problem; /nodes/a invalid node; missing setting");
});

test("API errors ignore malformed problems without hiding valid details or the HTTP fallback", () => {
  assert.equal(errorText(new ApiError(422, "invalid-settings", "", { problems: [null, {}, 3, "", { path: "/x" }, "actual error"] })), "invalid-settings: actual error");
  for (const body of [undefined, null, "html", { problems: "bad" }, { problems: [null, {}] }]) {
    assert.equal(errorText(new ApiError(502, "", "", body)), "HTTP 502");
  }
  assert.equal(errorText(new Error("network failure")), "network failure");
});
