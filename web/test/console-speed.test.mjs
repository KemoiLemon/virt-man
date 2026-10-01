// Console speed preset: the mapping from a preset to the daemon's coalescing
// window, and the fact that the choice survives a reload.
//
// The window is the whole feature. If `fast` ever stopped being 0 the viewer
// would add latency nobody asked for; if `quality` stopped being the largest the
// preset would be a label with nothing behind it — which is exactly what the
// live suite warns about ("no dead preset picker"). Balanced is the default, so
// a missing preference must land there rather than on whichever preset happens
// to be first in the list.

import assert from "node:assert/strict";
import test, { beforeEach } from "node:test";
import { JSDOM } from "jsdom";

// localStorage is provided by the DOM; install it before importing the module,
// since the module reads the stored value at call time but the tests need a
// clean store per case.
const dom = new JSDOM("<!doctype html><body></body>", { url: "http://localhost/" });
globalThis.window = dom.window;
globalThis.localStorage = dom.window.localStorage;

const cs = await import("./_build/console-speed.js");

beforeEach(() => dom.window.localStorage.clear());

test("the presets are ordered from least to most coalescing", () => {
  const ms = cs.SPEEDS.map((s) => s.settleMs);
  assert.equal(ms[0], 0, "Fast must add no delay at all");
  assert.ok(ms[1] > 0, "Balanced must actually coalesce something");
  assert.ok(ms[2] > ms[1], "Quality must wait longer than Balanced");
});

test("every preset has a distinct key and a label", () => {
  const keys = cs.SPEEDS.map((s) => s.key);
  assert.equal(new Set(keys).size, keys.length, "keys are unique");
  for (const s of cs.SPEEDS) {
    assert.ok(s.label, `${s.key} has a label`);
    assert.ok(s.hint, `${s.key} explains itself`);
  }
});

test("balanced is the default before anything is stored", () => {
  assert.equal(cs.speed().key, "balanced");
  assert.ok(cs.speed().settleMs > 0, "the default is not Fast");
});

test("a choice survives a reload", () => {
  cs.setSpeed("quality");
  assert.equal(cs.speed().key, "quality", "read back in the same session");
  // A reload re-reads from storage, so clearing the module cache is not needed:
  // the value is read on every call rather than cached at import.
  assert.equal(dom.window.localStorage.getItem("virt-man.console-speed"), "quality");
});

test("setSpeed returns the preset it applied", () => {
  assert.equal(cs.setSpeed("fast").settleMs, 0);
  assert.equal(cs.speed().settleMs, 0);
});

test("a corrupt or unknown stored value falls back to balanced", () => {
  dom.window.localStorage.setItem("virt-man.console-speed", "turbo");
  assert.equal(cs.speed().key, "balanced", "an unknown key is not left selected");
  // And setting one does not persist the bogus key.
  assert.equal(cs.setSpeed("turbo").key, "balanced");
  assert.equal(dom.window.localStorage.getItem("virt-man.console-speed"), "balanced");
});

test("the options markup reflects the stored choice", () => {
  cs.setSpeed("quality");
  const html = cs.speedOptions();
  assert.match(html, /value="quality" selected/, "the stored preset is selected");
  assert.equal((html.match(/<option/g) ?? []).length, cs.SPEEDS.length);
  // Every preset must be reachable from the picker, or a stored value could
  // not be changed back.
  for (const s of cs.SPEEDS) {
    assert.ok(html.includes(`value="${s.key}"`), `${s.key} is offered`);
  }
});
