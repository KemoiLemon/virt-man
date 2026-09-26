// Functional test for the themed listbox (task-2 contract).
//
// Exercises the real module against a jsdom DOM: a native <select> must be
// presented by a .select-trigger, open a .select-popover of .select-option
// buttons, and write the chosen value BACK to the native select (which is
// what every existing `.value` read in the app depends on).
//
// The module under test is the Vite-bundled artifact (see package.json
// "test"), so this exercises the exact code that ships, not a re-compile.
//
// Run: pnpm test

import assert from "node:assert/strict";
import test, { before } from "node:test";
import { JSDOM } from "jsdom";

let listbox;

before(async () => {
  const dom = new JSDOM(`<!doctype html><html><body></body></html>`, {
    url: "http://localhost/",
    pretendToBeVisual: true,
  });
  // The module registers document-level listeners at import time.
  globalThis.window = dom.window;
  globalThis.document = dom.window.document;
  globalThis.Element = dom.window.Element;
  globalThis.HTMLElement = dom.window.HTMLElement;
  globalThis.Event = dom.window.Event;
  globalThis.KeyboardEvent = dom.window.KeyboardEvent;
  listbox = await import("./_build/listbox.js");
});

function makeSelect(options, selected) {
  document.body.innerHTML = "";
  const sel = document.createElement("select");
  for (const [value, label] of options) {
    const o = document.createElement("option");
    o.value = value;
    o.textContent = label;
    sel.append(o);
  }
  sel.value = selected;
  document.body.append(sel);
  return sel;
}

test("builds a trigger showing the selected option's label", () => {
  const sel = makeSelect([["qcow2", "qcow2"], ["raw", "raw"]], "raw");
  listbox.enhanceSelects(document);

  const trigger = document.querySelector(".select-trigger");
  assert.ok(trigger, "a .select-trigger is created next to the select");
  assert.equal(trigger.querySelector(".select-value").textContent, "raw");
  assert.ok(trigger.querySelector(".select-caret svg"), "caret renders an inline svg");
  assert.equal(sel.nextElementSibling, trigger, "trigger sits directly after the select");
});

test("is idempotent — re-enhancing does not duplicate triggers", () => {
  makeSelect([["a", "A"], ["b", "B"]], "a");
  listbox.enhanceSelects(document);
  listbox.enhanceSelects(document);
  listbox.enhanceSelects(document);
  assert.equal(document.querySelectorAll(".select-trigger").length, 1);
});

test("opening renders one .select-option per option, marking the active one", () => {
  const sel = makeSelect([["nat", "NAT"], ["bridge", "Bridge"], ["none", "Isolated"]], "bridge");
  listbox.enhanceSelects(document);
  document.querySelector(".select-trigger").click();

  const pop = document.querySelector(".select-popover");
  assert.equal(pop.hidden, false, "popover is shown");
  const opts = pop.querySelectorAll(".select-option");
  assert.equal(opts.length, 3);
  assert.deepEqual([...opts].map((o) => o.textContent), ["NAT", "Bridge", "Isolated"]);
  assert.equal(pop.querySelectorAll(".select-option.active").length, 1);
  assert.equal(pop.querySelector(".select-option.active").textContent, "Bridge");
  assert.equal(sel.value, "bridge", "opening alone does not change the value");
});

test("choosing an option writes the value back and fires change", () => {
  const sel = makeSelect([["nat", "NAT"], ["bridge", "Bridge"]], "nat");
  let changes = 0;
  sel.addEventListener("change", () => changes++);

  listbox.enhanceSelects(document);
  document.querySelector(".select-trigger").click();
  [...document.querySelectorAll(".select-option")].find((o) => o.textContent === "Bridge").click();

  assert.equal(sel.value, "bridge", "native select owns the new value");
  assert.equal(changes, 1, "a bubbling change event fired exactly once");
  assert.equal(
    document.querySelector(".select-value").textContent,
    "Bridge",
    "trigger label follows the selection",
  );
  assert.equal(document.querySelector(".select-popover").hidden, true, "popover closed");
});

test("replacing options re-syncs the trigger label", () => {
  const sel = makeSelect([["old", "Old pool"]], "old");
  listbox.enhanceSelects(document);
  assert.equal(document.querySelector(".select-value").textContent, "Old pool");

  // Mirrors main.ts replacing a select's options after loading form data.
  sel.innerHTML = `<option value="default">default</option><option value="ISOs">ISOs</option>`;
  sel.value = "ISOs";
  listbox.enhanceSelects(sel);

  assert.equal(
    document.querySelector(".select-value").textContent,
    "ISOs",
    "stale label is refreshed when the select itself is the root",
  );
  assert.equal(document.querySelectorAll(".select-trigger").length, 1, "no duplicate trigger");
});

test("a disabled select disables its trigger", () => {
  const sel = makeSelect([["a", "A"]], "a");
  sel.disabled = true;
  listbox.enhanceSelects(document);
  assert.equal(document.querySelector(".select-trigger").disabled, true);
});

test("Escape closes an open popover", () => {
  makeSelect([["a", "A"], ["b", "B"]], "a");
  listbox.enhanceSelects(document);
  document.querySelector(".select-trigger").click();
  assert.equal(document.querySelector(".select-popover").hidden, false);

  document.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  assert.equal(document.querySelector(".select-popover").hidden, true);
});

test("keyboard opens the popover from the trigger", () => {
  makeSelect([["a", "A"], ["b", "B"]], "a");
  listbox.enhanceSelects(document);
  const trigger = document.querySelector(".select-trigger");

  trigger.dispatchEvent(new window.KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
  assert.equal(document.querySelector(".select-popover").hidden, false, "ArrowDown opens");
  listbox.closeSelect();

  trigger.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  assert.equal(document.querySelector(".select-popover").hidden, false, "Enter opens");
});
