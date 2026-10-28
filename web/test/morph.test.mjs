// morph: a repaint changes what differs and keeps what does not.
//
// The poll used to replace a pane's nodes every second, which dropped hover,
// transitions, scroll and focus. These pin the properties the app now relies
// on, including the two that keep older render code correct: a kept node must
// not carry its old handlers into the next bind, and a form control must read
// as the template says (except the one being typed in).
//
// Run: pnpm test

import assert from "node:assert/strict";
import test, { after } from "node:test";
import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><body></body>", { url: "http://localhost/" });
after(() => dom.window.close());
for (const k of ["window", "document", "Element", "EventTarget", "HTMLInputElement",
  "HTMLTextAreaElement", "HTMLSelectElement", "Node"]) {
  globalThis[k] = dom.window[k];
}
const { morphChildren, addPermanentListener } = await import("./_build/morph.js");

const mount = (html) => {
  const el = document.createElement("div");
  el.innerHTML = html;
  document.body.append(el);
  return el;
};
const next = (el, html) => {
  const n = el.cloneNode(false);
  n.innerHTML = html;
  morphChildren(el, n);
};

test("a node that is still there is still the same node", () => {
  const el = mount(`<ul><li id="a">one</li><li id="b">two</li></ul>`);
  const a = el.querySelector("#a");
  next(el, `<ul><li id="a">ONE</li><li id="b">two</li></ul>`);
  assert.equal(el.querySelector("#a"), a, "identity kept");
  assert.equal(a.textContent, "ONE", "text updated");
});

test("attributes and classes are brought in line", () => {
  const el = mount(`<span class="on" title="x">s</span>`);
  const s = el.firstChild;
  next(el, `<span class="off">s</span>`);
  assert.equal(el.firstChild, s);
  assert.equal(s.className, "off");
  assert.ok(!s.hasAttribute("title"), "removed attribute is gone");
});

test("extra and missing children are added and dropped", () => {
  const el = mount(`<p>a</p><p>b</p><p>c</p>`);
  next(el, `<p>a</p><p>b</p>`);
  assert.equal(el.children.length, 2);
  next(el, `<p>a</p><p>b</p><p>z</p><p>y</p>`);
  assert.deepEqual([...el.children].map((c) => c.textContent), ["a", "b", "z", "y"]);
});

test("a different element at the same place is replaced", () => {
  const el = mount(`<div id="x">text</div>`);
  next(el, `<table id="x"></table>`);
  assert.equal(el.firstChild.nodeName, "TABLE");
});

test("a kept node loses its old handlers so the next bind does not double them", () => {
  const el = mount(`<button>go</button>`);
  const b = el.firstChild;
  let calls = 0;
  b.addEventListener("click", () => calls++);
  b.onclick = () => calls++;
  next(el, `<button>go</button>`);
  b.click();
  assert.equal(calls, 0, "both the property and the listener were cleared");
  b.addEventListener("click", () => calls++);
  b.click();
  assert.equal(calls, 1, "a fresh bind fires once");
});

test("a permanent listener survives a repaint", () => {
  const el = mount(`<select><option>a</option></select>`);
  const s = el.firstChild;
  let calls = 0;
  addPermanentListener(s, "change", () => calls++);
  next(el, `<select><option>a</option></select>`);
  s.dispatchEvent(new dom.window.Event("change"));
  assert.equal(calls, 1);
});

test("a control reads as the template says, unless it is being typed in", () => {
  const el = mount(`<input id="i" value="a"><input id="j" value="a">`);
  const i = el.querySelector("#i");
  const j = el.querySelector("#j");
  i.value = "typed";
  j.value = "typed";
  j.focus();
  next(el, `<input id="i" value="a"><input id="j" value="a">`);
  assert.equal(i.value, "a", "reset to the template, as a replaced node would be");
  assert.equal(j.value, "typed", "the focused field keeps what the user wrote");
  assert.equal(document.activeElement, j, "and keeps focus");
});

test("a themed select's button and the selection bar are left alone", () => {
  const el = mount(`<select id="s"><option>a</option></select><i>x</i>`);
  const sel = el.querySelector("select");
  const trig = document.createElement("button");
  trig.className = "select-trigger";
  sel.after(trig);
  const bar = document.createElement("div");
  bar.className = "sel-bar";
  el.append(bar);
  next(el, `<select id="s"><option>a</option><option>b</option></select><i>y</i>`);
  assert.ok(trig.isConnected && bar.isConnected, "app-owned nodes survive");
  assert.equal(el.querySelector("i").textContent, "y", "the template still lines up around them");
  assert.equal(sel.options.length, 2);
});

test("removing a select takes its button with it", () => {
  const el = mount(`<select id="s"></select>`);
  const sel = el.firstChild;
  const trig = document.createElement("button");
  trig.className = "select-trigger";
  sel.after(trig);
  sel._trigger = trig;
  next(el, `<p>none</p>`);
  assert.ok(!trig.isConnected, "no orphaned button");
});

test("svg children are patched in their own namespace", () => {
  const el = mount(`<svg viewBox="0 0 10 10"><path d="M0 0"/></svg>`);
  const path = el.querySelector("path");
  next(el, `<svg viewBox="0 0 10 10"><path d="M1 1"/></svg>`);
  assert.equal(el.querySelector("path"), path);
  assert.equal(path.getAttribute("d"), "M1 1");
});
