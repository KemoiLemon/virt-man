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

let css;

before(() => {
  const file = readdirSync("dist/assets").find((f) => f.endsWith(".css"));
  assert.ok(file, "dist/assets has a built stylesheet (run: pnpm build)");
  css = readFileSync(`dist/assets/${file}`, "utf8").replace(/\s+/g, " ");
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
