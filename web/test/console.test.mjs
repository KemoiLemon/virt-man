// Console viewer: pixel decode, fit scaling, and keysym mapping.
//
// The RFB client lives in the daemon; the browser only decodes rectangles of
// base64 BGRA and paints them. These are the parts that silently corrupt the
// picture when wrong: a swapped channel tints the whole screen, and a fit
// scale that upscales past 1:1 blurs a small guest on a big display.
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

// ---- pixel decode ----------------------------------------------------------

test("BGRA from the guest becomes RGBA with opaque alpha", () => {
  // One pixel, little-endian BGRX: blue=0x10, green=0x20, red=0x30.
  const bin = String.fromCharCode(0x10, 0x20, 0x30, 0x00);
  const px = con.bgraToRgba(bin, 1, 1);
  assert.deepEqual([...px], [0x30, 0x20, 0x10, 255], "channels swap, alpha forced");
});

test("every decoded pixel is fully opaque", () => {
  // The guest sends BGRX, where the 4th byte is padding, not alpha. Trusting
  // it would paint a fully transparent screen.
  const bin = String.fromCharCode(...Array.from({ length: 16 }, () => 0));
  const px = con.bgraToRgba(bin, 2, 2);
  for (let i = 3; i < px.length; i += 4) {
    assert.equal(px[i], 255, `pixel ${(i - 3) / 4} opaque`);
  }
});

test("a short payload decodes to null instead of a torn image", () => {
  // A truncated frame must be dropped, not painted as garbage.
  assert.equal(con.bgraToRgba("abc", 4, 4), null);
});

test("decoded buffers are ImageData-compatible", () => {
  // ImageData rejects a SharedArrayBuffer-backed array; the decode must
  // always hand back a plain ArrayBuffer.
  const px = con.bgraToRgba(String.fromCharCode(0, 0, 0, 0), 1, 1);
  assert.ok(px.buffer instanceof ArrayBuffer, "backed by a plain ArrayBuffer");
  assert.equal(px.length, 4, "exactly one pixel");
});

// ---- fit scaling -----------------------------------------------------------

test("fit never upscales past 1:1", () => {
  // A small guest on a big stage must stay crisp rather than be blown up.
  assert.equal(con.fitScale(800, 600, 1920, 1080), 1);
});

test("fit shrinks to the tighter axis", () => {
  // 1600x1200 into 800x900: width is the constraint, so 0.5.
  assert.equal(con.fitScale(1600, 1200, 800, 900), 0.5);
  // 1600x1200 into 1600x600: height constrains instead.
  assert.equal(con.fitScale(1600, 1200, 1600, 600), 0.5);
});

test("fit is defensive about a stage with no size", () => {
  // Called before layout, or on a hidden tab: must not divide by zero.
  for (const args of [[0, 0, 100, 100], [800, 600, 0, 0], [800, 600, -5, 10]]) {
    const s = con.fitScale(...args);
    assert.ok(Number.isFinite(s) && s > 0, `finite scale for ${args}: ${s}`);
  }
});

test("aspect ratio is preserved by the fit scale", () => {
  const [gw, gh] = [1024, 768];
  const s = con.fitScale(gw, gh, 500, 500);
  assert.ok(Math.abs((gw * s) / (gh * s) - gw / gh) < 1e-9, "ratio unchanged");
  assert.ok(gw * s <= 500 && gh * s <= 500, "fits inside the stage");
});

// ---- keysyms ---------------------------------------------------------------

test("printable keys map to their character code", () => {
  assert.equal(con.keysymFor("a"), 0x61);
  assert.equal(con.keysymFor("Z"), 0x5a);
  assert.equal(con.keysymFor(" "), 0x20, "space has an explicit entry");
});

test("named keys map to X11 keysyms", () => {
  // These are the values the RFB server expects; ASCII would be wrong.
  assert.equal(con.keysymFor("Enter"), 0xff0d);
  assert.equal(con.keysymFor("Escape"), 0xff1b);
  assert.equal(con.keysymFor("Backspace"), 0xff08);
  assert.equal(con.keysymFor("ArrowUp"), 0xff52);
  assert.equal(con.keysymFor("F1"), 0xffbe);
  assert.equal(con.keysymFor("Control"), 0xffe3);
});

test("unknown multi-character keys map to nothing", () => {
  // 0 means "do not send", rather than a bogus keysym the guest misreads.
  assert.equal(con.keysymFor("Unidentified"), 0);
  assert.equal(con.keysymFor("MediaPlayPause"), 0);
});

// ---- handle contract -------------------------------------------------------

test("module exposes the console contract the UI depends on", () => {
  assert.equal(typeof con.attachConsole, "function");
  assert.equal(typeof con.detachConsole, "function");
  assert.equal(typeof con.currentConsole, "function");
});

test("detachConsole is safe with nothing attached", () => {
  con.detachConsole();
  assert.equal(con.currentConsole(), null, "no console after detaching");
});

test("a non-VNC guest is refused with a virt-viewer hint", async () => {
  // SPICE has no browser client: the tab must say so instead of half-attaching.
  const stage = document.createElement("div");
  const seen = [];
  const handle = await con.attachConsole(stage, "spice", "/proxy/x/domains/y/console",
    (m) => seen.push(m));
  assert.equal(handle, null, "no viewer for SPICE");
  assert.match(seen.join(" "), /virt-viewer/i, `status mentions virt-viewer: ${seen}`);
});

test("attachConsole reports failure instead of throwing", async () => {
  // The connect call cannot succeed here (no backend), so this must degrade to
  // a status message rather than crashing the tab.
  const stage = document.createElement("div");
  const seen = [];
  globalThis.fetch = () => Promise.reject(new Error("no backend"));
  const handle = await con.attachConsole(stage, "vnc", "/proxy/x/domains/y/console",
    (m) => seen.push(m));
  assert.equal(handle, null, "no handle when the daemon refuses");
  assert.ok(seen.length > 0, `status reported: ${JSON.stringify(seen)}`);
});

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
