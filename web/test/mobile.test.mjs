// The phone layout's contract, asserted against the SHIPPED stylesheet and the
// real index.html.
//
// These are the rules jsdom cannot check: it does no layout and evaluates no
// media queries, so nothing else in the suite would notice the CSS that makes a
// 375px screen usable quietly disappearing. Each one below is a specific way the
// phone build has already been broken or would break silently.
//
// Run: pnpm build && pnpm test

import assert from "node:assert/strict";
import test, { before } from "node:test";
import { readFileSync, readdirSync } from "node:fs";

let css;
let html;

before(() => {
  const file = readdirSync("dist/assets").find((f) => f.endsWith(".css"));
  assert.ok(file, "dist/assets has a built stylesheet (run: pnpm build)");
  css = readFileSync(`dist/assets/${file}`, "utf8");
  html = readFileSync("index.html", "utf8");
});

/// The width query, however the minifier spells it (`max-width: 820px` becomes
/// `width<=820px` in the shipped bundle).
const WIDTH_QUERY = /@media\s*\(\s*(?:max-width\s*:\s*|width\s*<=\s*)820px\s*\)/;

test("the viewport opts into the notch", () => {
  // Without viewport-fit=cover, env(safe-area-inset-*) reports 0 and every
  // rule below it silently does nothing.
  assert.match(html, /name="viewport"/);
  assert.match(html, /viewport-fit=cover/, "viewport-fit=cover is required for safe-area insets");
});

test("the phone layer loads last", () => {
  // Sizing overrides belong in the last layer: a rule added to an earlier file
  // loses the cascade to density.css and the tweak silently does nothing.
  const entry = readFileSync("src/styles.css", "utf8");
  const density = entry.indexOf("./styles/density.css");
  const mobile = entry.indexOf("./styles/mobile.css");
  assert.ok(density >= 0 && mobile >= 0, "both layers are imported");
  assert.ok(mobile > density, "mobile.css is imported after density.css");
});

test("the phone layer really is last in the shipped bundle", () => {
  // The import order above is not proof: a builder may reorder. density.css's
  // own distinctive rule must precede the mobile media query in the output.
  const densityRule = css.indexOf(".al-filters>.btn");
  const mobileQuery = css.search(WIDTH_QUERY);
  assert.ok(densityRule >= 0, "density.css's marker rule is in the bundle");
  assert.ok(mobileQuery >= 0, "the mobile media query is in the bundle");
  assert.ok(densityRule < mobileQuery, "mobile rules come after density rules");
});

test("the sidebar leaves the flow instead of squeezing the content", () => {
  // .layout was minmax(300px, 21vw) 1fr: below 375px the 300px minimum won and
  // the content pane was ~35px wide.
  const q = css.search(WIDTH_QUERY);
  const block = css.slice(q, q + 900);
  assert.match(block, /\.layout\{grid-template-columns:1fr\}/,
    "one column, so the content pane gets the whole width");
  assert.match(block, /\.list-pane\{[^}]*position:fixed/,
    "the sidebar is an overlay, not a grid column");
  assert.match(block, /transform:translate\(-100%\)/, "and it is parked off-screen");
  assert.match(block, /body\.nav-open \.list-pane\{[^}]*transform:none/,
    "opening it brings it back");
});

test("the drawer has a scrim to close it", () => {
  assert.match(css, /\.nav-scrim\{/, ".nav-scrim exists");
  assert.match(css, /body\.nav-open \.nav-scrim\{display:block/, "and shows only while open");
});

test("phone-only chrome is hidden by default and revealed in the query", () => {
  // The gate is one class: if a new phone-only control is added without it, the
  // control shows up on the desktop instead of failing loudly.
  const hidden = css.indexOf(".mobile-only{display:none}");
  const revealed = css.indexOf(".mobile-only{display:inline-flex}");
  assert.ok(hidden >= 0, "hidden by default");
  assert.ok(revealed >= 0, "revealed inside the width query");
  assert.ok(hidden < revealed, "the reveal comes after the hide");
});

test("the shell is sized to the dynamic viewport, not 100vh", () => {
  // 100vh on iOS is the LARGE viewport (toolbars retracted), so a full-height
  // shell is clipped whenever the toolbar is showing.
  const q = css.search(WIDTH_QUERY);
  assert.match(css.slice(q, q + 900), /height:100dvh/, "dvh tracks what is visible");
});

test("safe-area insets are declared once and used by the layout", () => {
  // The variables are also read by touch.ts, which is why they are properties
  // rather than inline env() calls: a popover clamped to innerHeight lands
  // behind the keyboard and the home bar.
  assert.match(css, /--safe-top:env\(safe-area-inset-top/, "the top inset is declared");
  assert.match(css, /--safe-bottom:env\(safe-area-inset-bottom/, "and the bottom");
  assert.match(css, /padding:var\(--safe-top\) var\(--safe-right\)/, "the shell pads by them");
});

test("text fields reach 16px on a coarse pointer", () => {
  // Below 16px iOS zooms the page when a field is focused and does not zoom
  // back, leaving the whole app stuck at 1.3x after one tap. It is the single
  // rule most likely to be "tidied" away, and nothing else would catch it.
  const at = css.indexOf("@media (pointer:coarse)");
  assert.ok(at >= 0, "a coarse-pointer block exists");
  const block = css.slice(at, at + 400);
  // The visible dropdown is the themed .select-trigger; the native <select> is
  // display:none, so sizing `select` alone would do nothing.
  assert.match(block, /input,textarea,\.select-trigger\{font-size:16px\}/,
    "the fields ACTUALLY rendered are the ones sized");
});

test("the coarse block lowers the long-press callout only where we own it", () => {
  // The callout must survive in the XML and log panes, where it is the only way
  // to copy. Suppressing it globally would quietly remove copy/paste there.
  const at = css.indexOf("@media (pointer:coarse)");
  const block = css.slice(at, at + 1600);
  assert.match(block, /-webkit-touch-callout:none/, "resource surfaces suppress it");
  assert.match(block, /\.tree-row,.dhead,\.tab/, "and they are named individually");
  // The opt-out surfaces (.cmd, .log, .spec span) must not appear in this block.
  assert.doesNotMatch(block, /\.log\b/, "the log pane keeps the native callout");
});

test("the console claims every gesture on the screen", () => {
  // Without touch-action:none, WebKit takes a pinch for page zoom and a drag for
  // scrolling: neither reaches the guest, and a page zoom cannot be undone by
  // the page.
  assert.match(css, /\.console-stage\{[^}]*touch-action:none/,
    "the stage owns its gestures");
});

test("the console has a keyboard field and an on-screen key row", () => {
  // 1px and transparent, never display:none: iOS declines to show a keyboard
  // for an element that is not rendered.
  assert.match(css, /\.console-kbd\{/, ".console-kbd exists");
  assert.match(css, /\.console-kbd\{[^}]*opacity:0/);
  assert.doesNotMatch(css, /\.console-kbd\{[^}]*display:none/,
    "display:none would stop the keyboard from ever appearing");
  assert.match(css, /\.console-kbd\{[^}]*font-size:16px/,
    "and it must not trigger the focus zoom it exists to avoid");
  assert.match(css, /\.console-keys\{/, "the key row exists");
  assert.match(css, /\.console-key\.on\{/, "and shows which modifiers are held");
});

test("dialogs stay inside the safe area", () => {
  // .modal-back is position:fixed, so the body's padding does not inset it and
  // a dialog would otherwise land under the notch.
  const q = css.search(WIDTH_QUERY);
  const block = css.slice(q, q + 2400);
  assert.match(block, /\.modal-back\{[^}]*padding:var\(--safe-top\)/,
    "the overlay pads by the insets, which insets the dialog it centres");
});
