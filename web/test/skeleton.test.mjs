// Skeleton placeholders: they must sit in the real layout classes (so the page
// does not jump when content replaces them) and announce themselves once.
//
// Run: pnpm test

import assert from "node:assert/strict";
import test, { before } from "node:test";

let sk;
before(async () => {
  sk = await import("./_build/skeleton.js");
});

test("every shape is one labelled, busy status region", () => {
  for (const shape of ["dashboard", "cards", "table", "page"]) {
    const html = sk.skeleton(shape);
    assert.match(html, /^<div class="skeleton" role="status" aria-label="Loading" aria-busy="true">/, shape);
    assert.equal(html.match(/role="status"/g).length, 1, `${shape}: one live region`);
  }
});

test("shapes reuse the layout classes of what they stand in for", () => {
  assert.match(sk.skeleton("dashboard"), /class="cards"/);
  assert.match(sk.skeleton("dashboard"), /class="sumgrid"/);
  assert.match(sk.skeleton("cards"), /class="storage-card"/);
  assert.match(sk.skeleton("page"), /class="dhead"/);
});

test("placeholders carry no text a screen reader would read", () => {
  for (const shape of ["dashboard", "cards", "table", "page"]) {
    const text = sk.skeleton(shape).replace(/<[^>]+>/g, "").trim();
    assert.equal(text, "", `${shape} has no text content`);
  }
});
