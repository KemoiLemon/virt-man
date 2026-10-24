// Console viewer: the pure parts around noVNC.
//
// noVNC does the RFB, the rendering and the input; what is left here is what
// silently breaks a console when wrong: the stream URL, and the keysyms sent
// for typed text and the phone's on-screen keys. The live console suite drives
// the real thing.
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

test("printable keys map to their character code", () => {
  assert.equal(con.keysymFor("a"), 0x61);
  assert.equal(con.keysymFor("Z"), 0x5a);
  assert.equal(con.keysymFor(" "), 0x20, "space has an explicit entry");
});

test("named keys map to X11 keysyms", () => {
  assert.equal(con.keysymFor("Enter"), 0xff0d);
  assert.equal(con.keysymFor("Escape"), 0xff1b);
  assert.equal(con.keysymFor("Backspace"), 0xff08);
  assert.equal(con.keysymFor("ArrowUp"), 0xff52);
  assert.equal(con.keysymFor("F1"), 0xffbe);
  assert.equal(con.keysymFor("Control"), 0xffe3);
});

test("unknown multi-character keys map to nothing", () => {
  assert.equal(con.keysymFor("Unidentified"), 0);
  assert.equal(con.keysymFor("MediaPlayPause"), 0);
});

test("typed characters: Latin-1 as itself, Unicode in the X11 range, newline as Return", () => {
  assert.equal(con.charKeysym("a"), 0x61);
  assert.equal(con.charKeysym("é"), 0xe9);
  assert.equal(con.charKeysym("€"), 0x01000000 + 0x20ac);
  assert.equal(con.charKeysym("😀"), 0x01000000 + 0x1f600, "astral characters are one key");
  assert.equal(con.charKeysym("\n"), 0xff0d);
  assert.equal(con.charKeysym("\t"), 0xff09);
});

test("the stream URL follows the page's scheme and host", () => {
  const base = "/proxy/c1/domains/web/console";
  assert.equal(con.streamUrl(base, { protocol: "https:", host: "h:8006" }),
    "wss://h:8006/proxy/c1/domains/web/console/stream");
  assert.equal(con.streamUrl(base, { protocol: "http:", host: "127.0.0.1:8799" }),
    "ws://127.0.0.1:8799/proxy/c1/domains/web/console/stream");
});

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
  const stage = document.createElement("div");
  const seen = [];
  const handle = await con.attachConsole(stage, "spice", "/proxy/x/domains/y/console",
    (m) => seen.push(m));
  assert.equal(handle, null, "no viewer for SPICE");
  assert.match(seen.join(" "), /virt-viewer/i, `status mentions virt-viewer: ${seen}`);
});
