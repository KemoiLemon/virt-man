// Icon-handling checks (task-3 / criterion 3).
//
// Asserts against the SHIPPED stylesheet and the real icon renderer:
//  - icons carry no inline paint attributes (one .ico > svg rule owns them)
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

test("icon colour is defined once in CSS, not per icon", () => {
  const rule = flat().match(/\.ico\s*>\s*svg\s*\{([^}]*)\}/);
  assert.ok(rule, ".ico > svg rule exists");
  assert.match(rule[1], /fill:\s*currentColor/, "filled glyphs take the text colour");
  assert.match(rule[1], /stroke:\s*none/, "no outline on a filled glyph");
});

test("rendered icons are bare Phosphor fill paths", () => {
  for (const name of ["close", "theme", "trash", "admin", "logout"]) {
    const html = icons.ic[name]();
    assert.match(html, /class="ico"/, `${name} is wrapped in .ico`);
    assert.match(html, /<svg viewBox="0 0 256 256"><path d="M[^"]{20,}"><\/path><\/svg>/,
      `${name} is one attribute-free path on Phosphor's 256 grid`);
    assert.doesNotMatch(html, /stroke|fill=/, `${name} carries no inline paint`);
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

// Create cards highlight on hover. Their icon wrappers pin a colour of their
// own, so without an explicit hover rule the icon stayed grey while the card
// turned red; and an icon that is red while idle stands out from every other
// icon, which idle at the dim colour.
test("create-card icons idle neutral and follow the hover highlight", () => {
  const css = flat();
  assert.doesNotMatch(css, /\.create-resource-card \.metric-ic\s*\{[^}]*color/,
    "create-resource icon is not coloured while idle");
  assert.match(css, /\.create-resource-card:hover \.metric-ic\s*\{[^}]*color:\s*var\(--accent\)/);
});


// Networks and pools are siblings of the guests under a server, so they share
// the guests' indent. The last rule wins (density.css overrides views.css), so
// compare the shipped values rather than one source file.
test("sidebar networks and pools align with the guests", () => {
  // The minifier merges selectors (".tree-vm,.tree-res{...}"), so split each
  // rule's selector list rather than matching one selector literally.
  const last = (sel) => {
    let v;
    for (const m of flat().matchAll(/([^{}]+)\{([^}]*)\}/g)) {
      const sels = m[1].split(",").map((s) => s.trim());
      const pad = m[2].match(/padding-left:\s*(\d+)px/);
      if (pad && sels.includes(sel)) v = Number(pad[1]);
    }
    return v;
  };
  const vm = last(".tree-vm");
  assert.ok(vm > 0, "guest indent found");
  assert.equal(last(".tree-res"), vm, "network/pool rows");
  assert.equal(last(".tree-res-empty"), vm, "'No networks.' / 'No pools.' placeholders");
});

test("keyboard focus is visible on buttons, rows and cards", () => {
  assert.match(flat(), /button:focus-visible[^{]*\{[^}]*outline:\s*1px solid var\(--accent\)/);
  assert.match(flat(), /\[tabindex\]:focus-visible/);
});

// The access-logs button sits on the far side of Add rule / Apply: right of
// the rules table, not among the editing actions.
test("the access-logs button is pushed to the right of its action row", () => {
  const css = flat();
  assert.match(css, /\.actions\s*>\s*\.al-open\s*\{[^}]*margin-left:\s*auto/);
});
