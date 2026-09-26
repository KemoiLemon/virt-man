// Icon-handling checks (task-3 / criterion 3).
//
// Asserts against the SHIPPED stylesheet and the real icon renderer:
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

test("stroke weight is defined once in CSS, not per icon", () => {
  const rule = flat().match(/\.ico\s*>\s*svg\s*\{([^}]*)\}/);
  assert.ok(rule, ".ico > svg rule exists");
  assert.match(rule[1], /stroke:\s*currentColor/);
  assert.match(rule[1], /stroke-width:\s*2/);
  assert.match(rule[1], /stroke-linecap:\s*round/);
  assert.match(rule[1], /stroke-linejoin:\s*round/);
});

test("rendered icons carry no inline stroke attributes", () => {
  for (const name of ["close", "theme", "trash", "admin", "logout"]) {
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

// The listbox popover is display:flex, which overrides the UA's [hidden]
// display:none (author styles beat UA styles). Without this rule a parked
// popover stays laid out and shows as a stray box — most visibly on screens
// with no selects at all, like the login page.
test("the hidden attribute beats layout display", () => {
  assert.match(flat(), /\[hidden\]\s*\{\s*display:\s*none\s*!important/,
    "[hidden] { display: none !important } exists in the shipped CSS");
});

// The daemon dashboard groups networking and storage pools into sections that
// need clear separation (and cards inside a section need a real gap).
test("dashboard sections are spaced apart", () => {
  assert.match(flat(), /\.dash-section\s*\{\s*margin-top:\s*22px/,
    ".dash-section has a top gap in the shipped CSS");
  assert.match(flat(), /\.dash-section\s+\.storage-card\s*\{[^}]*margin-bottom:\s*18px/,
    "dashboard storage cards are separated by 18px");
});
