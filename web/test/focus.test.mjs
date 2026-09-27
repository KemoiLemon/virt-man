// Tab / Shift+Tab stay inside the top layer (a modal or the context menu),
// wrap at both ends, and pull focus back in if it has escaped.
//
// Run: pnpm test

import assert from "node:assert/strict";
import test, { before } from "node:test";
import { JSDOM } from "jsdom";

let f;
let doc;
let win;
before(async () => {
  const dom = new JSDOM("<!doctype html><body></body>", { url: "http://localhost/", pretendToBeVisual: true });
  win = dom.window;
  doc = win.document;
  globalThis.window = win;
  globalThis.document = doc;
  globalThis.getComputedStyle = win.getComputedStyle.bind(win);
  globalThis.MutationObserver = win.MutationObserver;
  f = await import("./_build/focus.js");
});

const setup = () => {
  doc.body.innerHTML = `
    <button id="behind">page</button>
    <div class="modal-back open">
      <div class="modal">
        <div class="modal-head"><span>T</span><button class="x">x</button></div>
        <div class="modal-body">
          <input id="a">
          <select id="hidden-select" style="display:none"><option>1</option></select>
          <button id="disabled" disabled>no</button>
          <div hidden><button id="in-hidden">no</button></div>
          <button id="b" tabindex="-1">skip</button>
          <textarea id="c"></textarea>
        </div>
        <div class="modal-foot"><button id="cancel">Cancel</button><button id="ok">OK</button></div>
      </div>
    </div>`;
  return doc.querySelector(".modal-back");
};
const tab = (shiftKey = false) => {
  const e = new win.KeyboardEvent("keydown", { key: "Tab", shiftKey, bubbles: true, cancelable: true });
  (doc.activeElement ?? doc.body).dispatchEvent(e);
  return e.defaultPrevented;
};

test("only reachable controls are in the tab order", () => {
  const layer = setup();
  const ids = f.focusables(layer).map((el) => el.id || el.className);
  assert.deepEqual(ids, ["x", "a", "c", "cancel", "ok"]);
});

test("Tab wraps from the last control to the first, Shift+Tab the other way", () => {
  setup();
  doc.getElementById("ok").focus();
  assert.ok(tab(), "the browser's own move is replaced");
  assert.equal(doc.activeElement.className, "x", "wrapped to the first");
  assert.ok(tab(true));
  assert.equal(doc.activeElement.id, "ok", "Shift+Tab wrapped to the last");
});

test("Tab in the middle of a dialog is left to the browser", () => {
  setup();
  doc.getElementById("a").focus();
  assert.equal(tab(), false, "no interference between first and last");
});

test("focus that escaped the dialog is pulled back in", () => {
  setup();
  doc.getElementById("behind").focus();
  assert.ok(tab());
  assert.equal(doc.activeElement.className, "x", "Tab enters at the start");
  doc.getElementById("behind").focus();
  assert.ok(tab(true));
  assert.equal(doc.activeElement.id, "ok", "Shift+Tab enters at the end");
});

test("with no layer open, Tab belongs to the page", () => {
  doc.body.innerHTML = `<button id="p1">1</button><button id="p2">2</button>`;
  doc.getElementById("p2").focus();
  assert.equal(tab(), false);
});

test("the context menu is the top layer while it is open", () => {
  setup();
  const menu = doc.createElement("div");
  menu.className = "ctxmenu";
  menu.innerHTML = `<button id="m1">1</button><button id="m2">2</button>`;
  doc.body.append(menu);
  assert.equal(f.topLayer(), menu);
  doc.getElementById("m2").focus();
  tab();
  assert.equal(doc.activeElement.id, "m1", "wraps within the menu, not the modal below");
});

test("a Tab another widget already handled is left alone", () => {
  setup();
  doc.getElementById("ok").focus();
  const e = new win.KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true });
  doc.getElementById("ok").addEventListener("keydown", (ev) => ev.preventDefault(), { once: true });
  doc.getElementById("ok").dispatchEvent(e);
  assert.equal(doc.activeElement.id, "ok", "focus not moved a second time");
});

test("a new dialog focuses its first field, and gives focus back on close", async () => {
  const layer = setup();
  const opener = doc.getElementById("behind");
  opener.focus();
  f.enterLayer(layer, opener);
  assert.equal(doc.activeElement.id, "a", "first field, not the close button");
  layer.remove();
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(doc.activeElement.id, "behind", "focus returned to what opened it");
});

test("a dialog that placed focus itself keeps it", () => {
  const layer = setup();
  doc.getElementById("c").focus();
  f.enterLayer(layer, null);
  assert.equal(doc.activeElement.id, "c");
});

test("Enter opens a focused card like a click", () => {
  doc.body.innerHTML = `<div data-open tabindex="0" id="card"></div>`;
  let clicked = 0;
  const card = doc.getElementById("card");
  card.addEventListener("click", () => clicked++);
  card.focus();
  card.dispatchEvent(new win.KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
  card.dispatchEvent(new win.KeyboardEvent("keydown", { key: " ", bubbles: true, cancelable: true }));
  assert.equal(clicked, 2);
});
