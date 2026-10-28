// Behavioural test for the themed autocomplete used by the firewall's
// source/dest IP fields.
//
// The control has to stay a free-text input (any address is valid) while
// offering what the daemon knows about, so the contract is: suggest without
// trapping — commit on click or Enter, filter prefix-first, and let one Escape
// close only the list.
//
// The module under test is the Vite-bundled artifact (see package.json
// "test"), so this exercises the exact code that ships.
//
// Run: pnpm test

import assert from "node:assert/strict";
import test, { before, beforeEach } from "node:test";
import { JSDOM } from "jsdom";

let suggest;
let win;

before(async () => {
  win = new JSDOM(`<!doctype html><html><body></body></html>`, {
    url: "http://localhost/",
    pretendToBeVisual: true,
  }).window;
  // The module registers document-level listeners at import time.
  globalThis.window = win;
  globalThis.document = win.document;
  globalThis.Element = win.Element;
  globalThis.HTMLElement = win.HTMLElement;
  globalThis.Event = win.Event;
  globalThis.KeyboardEvent = win.KeyboardEvent;
  suggest = await import("./_build/suggest.js");
});

beforeEach(() => {
  suggest.closeSuggest();
  win.document.body.textContent = "";
});

/** A mounted input wired to a fixed candidate list. */
function field(candidates) {
  const input = win.document.createElement("input");
  win.document.body.append(input);
  suggest.attachSuggest(input, () => candidates);
  return input;
}

const optionLabels = () =>
  [...win.document.querySelectorAll(".select-popover .select-option")]
    .map((b) => b.textContent);

const popoverVisible = () =>
  !!win.document.querySelector(".select-popover:not([hidden])");

const type = (input, value) => {
  input.value = value;
  input.dispatchEvent(new win.Event("input", { bubbles: true }));
};

test("focusing the field offers every known address", () => {
  const input = field(["192.168.167.1", "10.0.0.5"]);
  input.dispatchEvent(new win.Event("focus"));
  assert.ok(popoverVisible(), "popover shown");
  assert.deepEqual(optionLabels(), ["192.168.167.1", "10.0.0.5"]);
});

test("typing filters, ranking prefix matches above substring matches", () => {
  // For query "10.0": "10.0.16.5" is a prefix match, while "192.10.0.5" only
  // contains it. Prefix hits must be ranked first even though the substring
  // match was listed earlier.
  const input = field(["192.10.0.5", "10.0.16.5"]);
  input.dispatchEvent(new win.Event("focus"));
  type(input, "10.0");
  assert.deepEqual(optionLabels(), ["10.0.16.5", "192.10.0.5"]);
});

test("a query that matches nothing closes the list instead of showing it empty", () => {
  const input = field(["192.168.167.1"]);
  input.dispatchEvent(new win.Event("focus"));
  type(input, "10.9.9");
  assert.equal(popoverVisible(), false, "no empty popover left behind");
});

test("Enter commits the highlighted address back into the field", () => {
  const input = field(["192.168.167.1", "192.168.167.2"]);
  input.dispatchEvent(new win.Event("focus"));
  const changes = [];
  input.addEventListener("change", () => changes.push(input.value));

  input.dispatchEvent(new win.KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
  input.dispatchEvent(new win.KeyboardEvent("keydown", { key: "Enter", bubbles: true }));

  assert.equal(input.value, "192.168.167.2", "second entry selected");
  assert.deepEqual(changes, ["192.168.167.2"], "existing change listeners stay in step");
  assert.equal(popoverVisible(), false, "list closes after committing");
});

test("clicking an option commits it", () => {
  const input = field(["10.0.0.5"]);
  input.dispatchEvent(new win.Event("focus"));
  win.document.querySelector(".select-option").dispatchEvent(
    new win.MouseEvent("click", { bubbles: true }),
  );
  assert.equal(input.value, "10.0.0.5");
});

test("Escape closes only the list and does not reach the page", () => {
  const input = field(["192.168.167.1"]);
  input.dispatchEvent(new win.Event("focus"));
  type(input, "192.168.167.1");
  assert.ok(popoverVisible(), "list open before Escape");

  // Stands in for the app's document-level Escape handler, which would
  // otherwise also dismiss the enclosing modal on the same press.
  let reachedDocument = false;
  const spy = () => { reachedDocument = true; };
  win.document.addEventListener("keydown", spy);
  input.dispatchEvent(new win.KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  win.document.removeEventListener("keydown", spy);

  assert.equal(popoverVisible(), false, "list dismissed");
  assert.equal(reachedDocument, false, "one press dismisses one layer");
  assert.equal(input.value, "192.168.167.1", "the typed value is untouched");
});

test("committing does not immediately reopen the list", () => {
  // Refocusing the input on commit would re-fire the focus listener and pop
  // the dropdown back open over the value the user just chose.
  const input = field(["10.0.0.5", "10.0.0.6"]);
  input.dispatchEvent(new win.Event("focus"));
  win.document.querySelector(".select-option").dispatchEvent(
    new win.MouseEvent("click", { bubbles: true }),
  );
  assert.equal(input.value, "10.0.0.5");
  assert.equal(popoverVisible(), false, "list stays closed after a pick");
});
