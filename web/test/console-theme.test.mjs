// The container text console's surface, asserted against the SHIPPED
// stylesheet. The VNC stage is a guest screen with a black letterbox, but the
// text console reuses the same class and inherited that black -- a frame around
// the terminal that clashed with the theme. This pins the override, because a
// stylesheet change is invisible to jsdom (no layout, no `var()` substitution)
// and only shows up as a rendering bug.
//
// Run: pnpm build && pnpm test

import assert from "node:assert/strict";
import test, { before } from "node:test";
import { readFileSync, readdirSync } from "node:fs";

let css;

before(() => {
  // `index-*.css` is the app stylesheet; the terminal chunk also emits a `.css`
  // (xterm's own) and `readdirSync` order is not guaranteed.
  const files = readdirSync("dist/assets");
  const file = files.find((f) => f.startsWith("index-") && f.endsWith(".css"))
    ?? files.find((f) => f.endsWith(".css"));
  assert.ok(file, "dist/assets has a built stylesheet (run: pnpm build)");
  css = readFileSync(`dist/assets/${file}`, "utf8");
});

test("the text console stage takes the theme background, not the VNC black", () => {
  // `.console-stage` keeps its `#000` letterbox for the guest screen, so the
  // fix must be a more specific rule on `.term-stage`; the minifier keeps the
  // selector list, only rewriting the declaration.
  const rule = css.match(/\.console-stage\.term-stage[^{]*\{([^}]*)\}/);
  assert.ok(rule, "the text console stage has its own background rule");
  assert.match(rule[1], /background:var\(--code-bg\)/,
    "the stage uses the terminal's own background");
});

test("xterm's hardcoded viewport black is overridden", () => {
  // xterm paints its canvas but its own stylesheet sets `#000` on the viewport
  // around it. The override must reach that element with enough specificity to
  // win whatever order the lazily loaded stylesheets land in.
  const rule = css.match(/\.console-stage\.term-stage \.xterm-viewport[^{]*\{([^}]*)\}/);
  assert.ok(rule, "the terminal viewport is overridden");
  assert.match(rule[1], /background:var\(--code-bg\)/,
    "the viewport follows the theme instead of xterm's black");
});
