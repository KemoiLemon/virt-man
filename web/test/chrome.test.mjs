// Window-control + icon-handling checks (task-3 / criterion 1 + 3).
//
// Asserts against the SHIPPED stylesheet and the real icon renderer:
//  - .win-btn is full-height, fixed-width, and hovers to --bad on close
//  - icons carry no inline stroke attributes (one .ico > svg rule owns them)
//
// Run: pnpm test

import assert from "node:assert/strict";
import test, { before } from "node:test";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM } from "jsdom";

let css;
let icons;

before(async () => {
  const file = readdirSync("dist/assets").find((f) => f.endsWith(".css"));
  assert.ok(file, "dist/assets has a built stylesheet (run: pnpm build)");
  css = readFileSync(`dist/assets/${file}`, "utf8");

  const dom = new JSDOM("<!doctype html><body></body>", { url: "http://localhost/" });
  globalThis.window = dom.window;
  globalThis.document = dom.window.document;
  icons = await import("./_build/icons.js");
});

/** Collapse whitespace so rule matching is layout-insensitive. */
const flat = () => css.replace(/\s+/g, " ");

test("window controls use dedicated .win-btn styling, not .btn.icon", () => {
  const rule = flat().match(/\.win-btn\s*\{([^}]*)\}/);
  assert.ok(rule, ".win-btn rule exists in the shipped CSS");
  const body = rule[1];
  assert.match(body, /width:\s*44px/, "fixed width");
  assert.match(body, /height:\s*100%/, "full height");
  assert.match(body, /border:\s*0/, "no generic button border");
});

test("close control hovers to the --bad colour", () => {
  const hover = flat().match(/\.win-btn\.close:hover\s*\{([^}]*)\}/);
  assert.ok(hover, ".win-btn.close:hover rule exists");
  assert.match(hover[1], /background:\s*var\(--bad\)/, "hover background is --bad");
});

test("win-btn hover uses theme variables only", () => {
  const hover = flat().match(/\.win-btn:hover\s*\{([^}]*)\}/);
  assert.ok(hover, ".win-btn:hover rule exists");
  assert.match(hover[1], /var\(--fg\)/);
  assert.match(hover[1], /var\(--hover\)/);
});

test("stroke weight is defined once in CSS, not per icon", () => {
  const rule = flat().match(/\.ico\s*>\s*svg\s*\{([^}]*)\}/);
  assert.ok(rule, ".ico > svg rule exists");
  assert.match(rule[1], /stroke:\s*currentColor/);
  assert.match(rule[1], /stroke-width:\s*2/);
  assert.match(rule[1], /stroke-linecap:\s*round/);
  assert.match(rule[1], /stroke-linejoin:\s*round/);
});

test("rendered icons carry no inline stroke attributes", () => {
  for (const name of ["min", "max", "close", "theme", "webui", "trash"]) {
    const html = icons.ic[name]();
    assert.match(html, /class="ico"/, `${name} is wrapped in .ico`);
    assert.match(html, /<svg viewBox="0 0 24 24">/, `${name} svg is attribute-free`);
    assert.doesNotMatch(html, /stroke-width=/, `${name} has no inline stroke-width`);
    assert.doesNotMatch(html, /stroke="currentColor"/, `${name} has no inline stroke`);
  }
});

test("icons honour a requested size without restyling the svg", () => {
  assert.match(icons.ic.close(20), /width:20px;height:20px/);
  assert.match(icons.ic.close(), /width:16px;height:16px/, "defaults to 16px");
});
