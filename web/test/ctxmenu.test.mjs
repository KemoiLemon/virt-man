// The right-click menu closes when what is behind it scrolls (it would point
// at the wrong row), but NOT when the menu itself scrolls: the scroll
// listener captures, so it heard the menu's own list and a long menu vanished
// the moment you scrolled down it.
//
// Run: pnpm test

import assert from "node:assert/strict";
import test, { before } from "node:test";
import { JSDOM } from "jsdom";

let m;
let doc;

before(async () => {
  const dom = new JSDOM("<!doctype html><body><div id='pane'></div></body>", {
    url: "http://localhost/", pretendToBeVisual: true,
  });
  globalThis.window = dom.window;
  globalThis.document = dom.window.document;
  globalThis.getComputedStyle = dom.window.getComputedStyle.bind(dom.window);
  doc = dom.window.document;
  m = await import("./_build/ctxmenu.js");
});

const items = Array.from({ length: 40 }, (_, i) => ({ label: `item ${i}`, run() {} }));
const scroll = (el) => el.dispatchEvent(new window.Event("scroll"));

test("scrolling the menu itself keeps it open", () => {
  m.openContextMenu(10, 10, items);
  const menu = doc.querySelector(".ctxmenu");
  assert.ok(menu, "menu open");
  scroll(menu);
  assert.ok(m.contextMenuOpen(), "still open after the menu scrolled");
  // A scroll event from inside it (an item) counts as the menu too.
  scroll(menu.querySelector(".ctxmenu-item"));
  assert.ok(m.contextMenuOpen(), "still open after a scroll from inside it");
  m.closeContextMenu();
});

test("scrolling what is behind the menu closes it", () => {
  m.openContextMenu(10, 10, items);
  scroll(doc.getElementById("pane"));
  assert.ok(!m.contextMenuOpen(), "closed when the page behind scrolled");
});

test("a wheel at the menu's end does not reach the page behind", () => {
  m.openContextMenu(10, 10, items);
  const menu = doc.querySelector(".ctxmenu");
  // jsdom does no layout: make it a short list already at its end.
  Object.defineProperty(menu, "scrollHeight", { value: 100, configurable: true });
  Object.defineProperty(menu, "clientHeight", { value: 100, configurable: true });
  const e = new window.WheelEvent("wheel", { deltaY: 40, bubbles: true, cancelable: true });
  menu.dispatchEvent(e);
  assert.ok(e.defaultPrevented, "the wheel is kept on the menu");
  assert.ok(m.contextMenuOpen(), "and the menu stays");
  m.closeContextMenu();
});
