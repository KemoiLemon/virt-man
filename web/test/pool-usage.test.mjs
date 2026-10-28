// The pool usage figure shown beside total/free.
//
// The daemon reports two different scopes: `allocation` is the blocks of the
// files in the pool directory, while `capacity`/`available` are the filesystem
// holding it. The UI must show one scope, or the three figures do not add up.

import assert from "node:assert/strict";
import test from "node:test";
import { poolUsed } from "./_build/pool-usage.js";

test("used is the filesystem's, not the pool directory's block count", () => {
  // 930.5 GiB total, 705.0 GiB free: 225.5 GiB is used by everything on the
  // filesystem, including data outside the pool root.
  const capacity = 999_129_350_144;
  const available = 756_940_177_408;
  assert.equal(poolUsed({ capacity, available, allocation: 370_548_736 }), capacity - available);
});

test("a definition without filesystem figures falls back to pool blocks", () => {
  assert.equal(poolUsed({ allocation: 1234 }), 1234);
  assert.equal(poolUsed({ capacity: 1000, allocation: 250 }), 250);
  assert.equal(poolUsed({}), 0);
});

test("used never goes negative", () => {
  assert.equal(poolUsed({ capacity: 100, available: 200, allocation: 5 }), 0);
});
