// Button alignment regression check.
//
// The Datacenter server row places a text button ("test") next to an
// icon-only button (delete) in one table cell. Inline-level boxes align on
// their baseline by default, and an icon-only button's baseline (the svg's
// bottom edge) sits lower than a text button's — which offset the two
// vertically. `vertical-align: middle` on .btn/.ico pins them instead.
//
// Run: pnpm test

import assert from "node:assert/strict";
import test, { before } from "node:test";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM } from "jsdom";

let css;
let raw;

before(() => {
  const file = readdirSync("dist/assets").find((f) => f.endsWith(".css"));
  assert.ok(file, "dist/assets has a built stylesheet (run: pnpm build)");
  raw = readFileSync(`dist/assets/${file}`, "utf8");
  css = raw.replace(/\s+/g, " ");
});

/** Bodies of every rule whose selector list contains `sel`, concatenated.
 *  A class is usually split across several rules (base, override, density),
 *  so matching only the first would miss later declarations. Splitting on
 *  `}` avoids the overlapping-delimiter problem a single regex would hit. */
function ruleFor(sel) {
  const bodies = [];
  for (const chunk of css.split("}")) {
    const i = chunk.lastIndexOf("{");
    if (i === -1) continue;
    const selectors = chunk.slice(0, i);
    const body = chunk.slice(i + 1);
    // Match the class as a whole token, so `.ico` never matches `.icon-label`.
    const re = new RegExp(`\\${sel}(?![a-zA-Z0-9_-])`);
    if (re.test(selectors)) bodies.push(body);
  }
  return bodies.length ? bodies.join(";") : null;
}

test(".btn is vertically centred, not baseline-aligned", () => {
  const body = ruleFor(".btn");
  assert.ok(body, ".btn rule exists in the shipped CSS");
  assert.match(
    body,
    /vertical-align:\s*middle/,
    "text and icon-only buttons must share a vertical anchor",
  );
});

test(".ico is vertically centred for bare icons outside buttons", () => {
  const body = ruleFor(".ico");
  assert.ok(body, ".ico rule exists");
  assert.match(body, /vertical-align:\s*middle/);
});

test("buttons still centre their own contents", () => {
  const body = ruleFor(".btn");
  assert.match(body, /display:\s*inline-flex/, "inline-flex layout retained");
  assert.match(body, /align-items:\s*center/, "contents stay centred");
});

/**
 * The access-log filter bar, as the two dialogs build it: fields, a themed
 * select, and the Live / Packet capture buttons.
 *
 * Measured through getComputedStyle, not by reading declarations: the bug this
 * guards against twice was a rule in views.css out-specifying the one in
 * density.css that sizes the fields, so the buttons rendered ~3 px shorter
 * than the selects beside them. A declaration-level check reads both rules and
 * sees nothing wrong; only the cascade does.
 *
 * jsdom resolves the cascade and computed lengths, but does no layout, so the
 * assertions are on the values that decide a control's height (font, vertical
 * padding, line-height) rather than on measured box geometry.
 */
function filterBar() {
  const dom = new JSDOM(
    `<!doctype html><html data-theme="dark"><head><style>${raw}</style></head><body>` +
      `<div class="modal wide al-modal"><div class="modal-body"><div class="al-filters">` +
      `<label class="field"><span>VM</span><select id="al-vm"></select>` +
      `<button class="select-trigger"><span class="select-value">All VMs</span></button></label>` +
      `<label class="field"><span>Port</span><input id="al-port"></label>` +
      `<label class="field al-grow"><span>Domain or IP</span><input id="al-q"></label>` +
      `<button class="btn" id="al-live">Live</button>` +
      `<button class="btn" id="al-capture">Packet capture</button>` +
      `</div></div></div></body></html>`,
    { url: "http://localhost/", pretendToBeVisual: true },
  );
  const { window } = dom;
  const computed = (sel) => window.getComputedStyle(window.document.querySelector(sel));
  /** Just the values that sum to a control's height. */
  const vertical = (sel) => {
    const s = computed(sel);
    return {
      fontSize: s.fontSize,
      paddingTop: s.paddingTop,
      paddingBottom: s.paddingBottom,
      lineHeight: s.lineHeight,
    };
  };
  return { vertical, computed, document: window.document };
}

test("a filter-bar button is exactly as tall as the fields beside it", () => {
  const bar = filterBar();
  const field = bar.vertical("#al-port");
  const select = bar.vertical(".select-trigger");
  assert.deepEqual(field, select, "the fields agree with each other first");
  assert.deepEqual(bar.vertical("#al-live"), field, "Live follows the fields");
  assert.deepEqual(bar.vertical("#al-capture"), field, "Packet capture follows the fields");
});

test("a control's border is declared once for buttons and fields alike", () => {
  // getComputedStyle does not resolve a `border:` shorthand into widths in
  // jsdom, so the 1 px edge that also contributes to height is checked at the
  // declaration instead — the same value for every control in the bar.
  assert.match(ruleFor(".btn"), /border:\s*1px solid/, ".btn has a 1 px border");
  // The bare token: ruleFor's trailing lookahead rejects a class-name
  // character, and `input,` in the minified selector is followed by `select`.
  assert.match(ruleFor("input"), /border:\s*1px solid/, "fields do too");
});

test("the filter bar is a grid, so its columns hold from one row to the next", () => {
  const bar = filterBar();
  const barRule = bar.computed(".al-filters");
  assert.equal(barRule.display, "grid", "a wrapping flex row packed each row to its own widths");
  assert.match(barRule.gridTemplateColumns, /auto-fit/, "columns are laid out to fit the dialog");
  assert.match(barRule.gridTemplateColumns, /minmax\(\d+px, ?1fr\)/, "…at an even width");
  assert.equal(
    bar.computed(".al-grow").gridColumn,
    "span 2",
    "the search box earns two columns",
  );
});
