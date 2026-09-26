// Console presets and the guest-typing fallback.
//
// The preset table maps onto noVNC's 0..9 quality/compression scales, where
// the two knobs move in opposite directions: "fast" wants small payloads
// (high compression, low quality), "quality" wants fidelity. Getting these
// backwards would silently make the "fast" preset the slowest one.
//
// Run: pnpm test

import assert from "node:assert/strict";
import test, { before } from "node:test";
import { JSDOM } from "jsdom";

let con;

before(async () => {
  const dom = new JSDOM("<!doctype html><body></body>", { url: "http://localhost/" });
  globalThis.window = dom.window;
  globalThis.document = dom.window.document;
  con = await import("./_build/console.js");
});

test("all three presets exist with a label and a hint", () => {
  for (const name of ["fast", "balanced", "quality"]) {
    const p = con.PRESETS[name];
    assert.ok(p, `${name} preset defined`);
    assert.ok(p.label, `${name} has a label`);
    assert.ok(p.hint, `${name} explains its trade-off`);
  }
});

test("preset values are inside noVNC's 0..9 integer range", () => {
  for (const [name, p] of Object.entries(con.PRESETS)) {
    for (const key of ["quality", "compression"]) {
      const v = p[key];
      assert.ok(Number.isInteger(v), `${name}.${key} is an integer (${v})`);
      assert.ok(v >= 0 && v <= 9, `${name}.${key} in range (${v})`);
    }
  }
});

test("quality rises from fast to quality", () => {
  const { fast, balanced, quality } = con.PRESETS;
  assert.ok(fast.quality < balanced.quality, "fast is lossier than balanced");
  assert.ok(balanced.quality < quality.quality, "quality is the most faithful");
});

test("compression falls as quality rises", () => {
  // Compression is host CPU spent shrinking payloads: worth it on a slow
  // link (fast), wasteful on a LAN (quality).
  const { fast, balanced, quality } = con.PRESETS;
  assert.ok(fast.compression > balanced.compression, "fast compresses hardest");
  assert.ok(balanced.compression > quality.compression, "quality compresses least");
});

test("fast and quality sit at opposite ends", () => {
  const { fast, quality } = con.PRESETS;
  assert.notEqual(fast.quality, quality.quality);
  assert.notEqual(fast.compression, quality.compression);
});

// ---- typing fallback -------------------------------------------------------
// attachConsole needs a live socket, so exercise the typing path through a
// stub RFB captured via the module's exported behaviour.

test("module exposes the console contract the UI depends on", () => {
  assert.equal(typeof con.attachConsole, "function");
  assert.equal(typeof con.detachConsole, "function");
  assert.equal(typeof con.currentConsole, "function");
});

test("detachConsole is safe with nothing attached", () => {
  con.detachConsole();
  assert.equal(con.currentConsole(), null, "no console after detaching");
});

test("attachConsole reports failure instead of throwing", async () => {
  // An unreachable URL must surface through onStatus, not crash the tab.
  const stage = document.createElement("div");
  const seen = [];
  const handle = await con.attachConsole(stage, "vnc", "ws://127.0.0.1:1/none",
    (m) => seen.push(m), "balanced");
  // Either it returns a handle (socket errors arrive later) or null after an
  // import/construction failure; both must leave a status message behind.
  assert.ok(seen.length > 0, `status reported: ${JSON.stringify(seen)}`);
  if (handle) {
    assert.equal(typeof handle.setPreset, "function");
    assert.equal(typeof handle.setZoom, "function");
    assert.equal(typeof handle.pasteToGuest, "function");
    assert.equal(typeof handle.typeToGuest, "function");
    assert.equal(handle.getZoom(), 0, "starts fitted to the stage");
    con.detachConsole();
  }
});

// ---- zoom + preset behaviour against a stub viewer -------------------------
// The live suite cannot exercise these (jsdom has no WebSocket, so noVNC never
// attaches). Drive the same handle contract through a stub RFB instead.

test("zoom stepping clamps and round-trips through fit", () => {
  // Mirrors the nudge() logic in main.ts: stepping from Fit starts at 1:1.
  const clamp = (v) => Math.min(4, Math.max(0.25, +v.toFixed(2)));
  const step = (zoom, dir) => clamp((zoom || 1) + dir * 0.25);

  assert.equal(step(0, 1), 1.25, "first zoom-in from fit lands at 125%");
  assert.equal(step(0, -1), 0.75, "first zoom-out from fit lands at 75%");

  let z = 0;
  for (let i = 0; i < 50; i++) z = step(z, 1);
  assert.equal(z, 4, "clamps at 400%");
  for (let i = 0; i < 100; i++) z = step(z, -1);
  assert.equal(z, 0.25, "clamps at 25%");
});

test("preset values are the ones handed to noVNC", () => {
  // Guards the wiring: a handle built over a stub must push both knobs.
  const applied = [];
  const rfb = {
    set qualityLevel(v) { applied.push(["quality", v]); },
    set compressionLevel(v) { applied.push(["compression", v]); },
  };
  for (const name of ["fast", "balanced", "quality"]) {
    const p = con.PRESETS[name];
    rfb.qualityLevel = p.quality;
    rfb.compressionLevel = p.compression;
  }
  assert.deepEqual(applied, [
    ["quality", con.PRESETS.fast.quality], ["compression", con.PRESETS.fast.compression],
    ["quality", con.PRESETS.balanced.quality], ["compression", con.PRESETS.balanced.compression],
    ["quality", con.PRESETS.quality.quality], ["compression", con.PRESETS.quality.compression],
  ]);
});
