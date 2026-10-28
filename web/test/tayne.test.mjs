// Tayne command palette: filtering, selection and execution.
//
// Run: pnpm test

import assert from "node:assert/strict";
import test, { before } from "node:test";
import { JSDOM } from "jsdom";

let tayne;

before(async () => {
  const dom = new JSDOM("<!doctype html><body></body>", { url: "http://localhost/" });
  globalThis.window = dom.window;
  globalThis.document = dom.window.document;
  globalThis.DOMParser = dom.window.DOMParser;
  tayne = await import("./_build/tayne.js");
});

const cmds = [
  { id: "go-dc", label: "Go to Datacenter", hint: "Servers" },
  { id: "vm-start", label: "Start — alpine", hint: "Selected VM" },
  { id: "toggle-theme", label: "Toggle Theme", hint: "dark → light" },
];

test("opening shows every command, first one selected", () => {
  tayne.openTayne(cmds);
  assert.ok(tayne.tayneOpen(), "palette open");
  const rows = [...document.querySelectorAll(".tayne-row")];
  assert.equal(rows.length, 3, "all commands listed");
  assert.ok(rows[0].classList.contains("sel"), "first row selected");
  tayne.closeTayne();
});

test("typing filters across label, hint and id", () => {
  tayne.openTayne(cmds);
  const input = document.querySelector(".tayne-input");
  input.value = "alpine";
  input.dispatchEvent(new window.Event("input", { bubbles: true }));
  let rows = [...document.querySelectorAll(".tayne-row")];
  assert.equal(rows.length, 1, "only the alpine command matches");
  assert.equal(rows[0].textContent, "Start — alpineSelected VM");

  // The hint is searched too: "dark" appears only in toggle-theme's hint.
  input.value = "dark";
  input.dispatchEvent(new window.Event("input", { bubbles: true }));
  rows = [...document.querySelectorAll(".tayne-row")];
  assert.equal(rows.length, 1, "hint text is searchable");
  assert.equal(rows[0].textContent, "Toggle Themedark → light");
  tayne.closeTayne();
});

test("no match shows the empty state instead of a stale row", () => {
  tayne.openTayne(cmds);
  const input = document.querySelector(".tayne-input");
  input.value = "zzz-nothing";
  input.dispatchEvent(new window.Event("input", { bubbles: true }));
  assert.equal(document.querySelectorAll(".tayne-row").length, 0);
  assert.ok(document.querySelector(".tayne-empty"), "empty message shown");
  tayne.closeTayne();
});

test("arrow keys move the selection, wrapping at the ends", () => {
  tayne.openTayne(cmds);
  const input = document.querySelector(".tayne-input");
  const sel = () => [...document.querySelectorAll(".tayne-row")].findIndex((r) => r.classList.contains("sel"));

  input.dispatchEvent(new window.KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
  assert.equal(sel(), 1, "down moves to the second row");
  input.dispatchEvent(new window.KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
  input.dispatchEvent(new window.KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
  assert.equal(sel(), 0, "down past the end wraps to the first row");
  input.dispatchEvent(new window.KeyboardEvent("keydown", { key: "ArrowUp", bubbles: true }));
  assert.equal(sel(), 2, "up wraps to the last row");
  tayne.closeTayne();
});

test("Enter actually invokes the selected command", () => {
  let invoked = "";
  tayne.openTayne([{ id: "a", label: "Alpha" }, { id: "b", label: "Beta" }]);
  // Re-open with commands that record their invocation.
  tayne.closeTayne();
  tayne.openTayne([
    { id: "a", label: "Alpha", run: () => { invoked = "alpha"; } },
    { id: "b", label: "Beta", run: () => { invoked = "beta"; } },
  ]);
  const input = document.querySelector(".tayne-input");
  input.dispatchEvent(new window.KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
  input.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  assert.equal(invoked, "beta", "the highlighted (second) command ran");
  assert.ok(!tayne.tayneOpen());
});

test("Escape closes the palette", () => {
  tayne.openTayne(cmds);
  const input = document.querySelector(".tayne-input");
  input.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  assert.ok(!tayne.tayneOpen(), "closed on Escape");
});

test("clicking a row runs it and closes", () => {
  let invoked = "";
  tayne.openTayne([{ id: "x", label: "Do the thing", run: () => { invoked = "done"; } }]);
  document.querySelector(".tayne-row").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  assert.equal(invoked, "done");
  assert.ok(!tayne.tayneOpen());
});

// Icons are optional per command and built as real elements (no innerHTML),
// with the <svg> a direct child of .ico so the shared `.ico > svg` rule applies.
test("a command's icon renders as a direct svg child of .ico", () => {
  const icon = '<span class="ico"><svg viewBox="0 0 256 256"><path d="M0 0h256v256H0z"></path></svg></span>';
  tayne.openTayne([
    { id: "a", label: "With icon", icon, run() {} },
    { id: "b", label: "Without", run() {} },
  ]);
  const rows = [...document.querySelectorAll(".tayne-row")];
  assert.ok(rows[0].querySelector(".tayne-ic > svg"), "icon present");
  assert.equal(rows[0].querySelector(".tayne-ic").nextElementSibling.className, "tayne-label", "icon sits before the label");
  assert.equal(rows[1].querySelector(".tayne-ic"), null, "no icon when none given");
  tayne.closeTayne();
});
