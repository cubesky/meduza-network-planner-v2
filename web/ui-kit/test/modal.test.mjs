import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import { test } from "node:test";
import assert from "node:assert/strict";
import ts from "typescript";

const runtime = ts.transpileModule(readFileSync(new URL("../src/index.tsx", import.meta.url), "utf8"), {
  compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
}).outputText;

function dialog() {
  const exports = {};
  const stub = {
    jsx: (type, props) => ({ type, props }), jsxs: (type, props) => ({ type, props }),
    useRef: current => ({ current }), useEffect() {}, initializeTheme() {}, t: value => value,
  };
  runInNewContext(runtime, { exports, require: () => stub });
  let closed = 0;
  const { props } = exports.Modal({ title: "Edit", onClose: () => closed++, children: null });
  const backdrop = {}, input = {};
  const event = (target, button = 0) => ({ target, currentTarget: backdrop, button });
  return { props, backdrop, input, event, closed: () => closed };
}

test("modal keeps edits while a text-selection drag starts or ends inside", () => {
  const d = dialog();
  for (const [start, end] of [[d.input, d.backdrop], [d.backdrop, d.input], [d.input, d.input]]) {
    d.props.onPointerDown(d.event(start));
    d.props.onPointerUp(d.event(end));
    // Browser may dispatch the click to the common ancestor, the backdrop.
    d.props.onClick(d.event(d.backdrop));
    assert.equal(d.closed(), 0);
  }
  d.props.onPointerDown(d.event(d.backdrop));
  d.props.onPointerUp(d.event(d.backdrop));
  d.props.onClick(d.event(d.backdrop));
  assert.equal(d.closed(), 1, "a normal backdrop click still dismisses");
});

test("cancelled gestures and secondary-button presses cannot dismiss a modal", () => {
  const d = dialog();
  d.props.onPointerDown(d.event(d.backdrop));
  d.props.onPointerCancel();
  d.props.onClick(d.event(d.backdrop));
  d.props.onPointerDown(d.event(d.backdrop, 2));
  d.props.onPointerUp(d.event(d.backdrop, 2));
  d.props.onClick(d.event(d.backdrop));
  assert.equal(d.closed(), 0);
});
