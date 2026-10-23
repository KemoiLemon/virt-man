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

test("closing before anything opened creates no popover", () => {
  // A click or Escape on a page with no selects (the login page) used to mint
  // an empty .select-popover node via the global dismiss handlers.
  document.body.innerHTML = "";
  listbox.closeSelect();
  document.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  document.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  assert.equal(document.querySelector(".select-popover"), null, "no popover node is created");
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

test("scrolling the popover itself does not close it", () => {
  // The capture scroll listener used to close on ANY scroll, including the
  // popover's own overflow-y:auto, so trying to scroll a long dropdown
  // dismissed it. A scroll event from inside .select-popover must be ignored.
  makeSelect([["a", "A"], ["b", "B"]], "a");
  listbox.enhanceSelects(document);
  document.querySelector(".select-trigger").click();
  const pop = document.querySelector(".select-popover");
  assert.equal(pop.hidden, false, "popover open");

  pop.dispatchEvent(new window.Event("scroll", { bubbles: true }));
  assert.equal(pop.hidden, false, "popover stays open when it scrolls");

  // A scroll elsewhere (a background pane) still closes it.
  document.body.dispatchEvent(new window.Event("scroll", { bubbles: true }));
  assert.equal(pop.hidden, true, "background scroll closes the popover");
});

test("a narrow trigger near the right edge stays aligned with its popover", () => {
  // The popover reserved a fixed 200px of viewport whatever its own width, so
  // a 72px size-unit picker at the right edge of the hardware form was drawn
  // ~90px left of the trigger it belongs to (the MiB/GiB misalignment). The
  // reserve must be the measured popover width. jsdom has no layout, so pin
  // the trigger's rect and the viewport; the popover reports its inline
  // min-width (the trigger's width) as its measured width.
  makeSelect([["MiB", "MiB"], ["GiB", "GiB"]], "MiB");
  listbox.enhanceSelects(document);
  const trigger = document.querySelector(".select-trigger");
  const beforeWidth = window.innerWidth;
  const original = window.Element.prototype.getBoundingClientRect;
  let rect = { left: 174, right: 246, top: 31, bottom: 65, width: 72, height: 34, x: 174, y: 31 };
  window.innerWidth = 300;
  window.Element.prototype.getBoundingClientRect = function () {
    if (this.classList?.contains("select-popover")) {
      const width = Number.parseFloat(this.style.minWidth) || 0;
      return { ...rect, left: 0, right: width, width, x: 0 };
    }
    return this.classList?.contains("select-trigger")
      ? rect
      : { left: 0, right: 0, top: 0, bottom: 0, width: 0, height: 0, x: 0, y: 0 };
  };
  try {
    trigger.click();
    const pop = document.querySelector(".select-popover");
    // 174 + 72 = 246, inside the 300px viewport, so it sits under the trigger.
    assert.equal(pop.style.left, "174px", "popover aligns with the trigger while it fits");

    listbox.closeSelect();
    // Pushed to the right edge: it may shift, but only by what overflows.
    rect = { ...rect, left: 260, right: 332, x: 260 };
    trigger.click();
    assert.equal(pop.style.left, "220px", "popover shifts only as far as needed to fit");
  } finally {
    window.Element.prototype.getBoundingClientRect = original;
    window.innerWidth = beforeWidth;
  }
});
