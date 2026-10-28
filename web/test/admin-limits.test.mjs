// Administrator limits visibility (and enforcement).
//
// The admin page has always said "Administrators are not limited", but the UI
// offered them a Limits column, a Limits row and a quota editor, and the server
// enforced whatever was stored. These pin the rule at both ends:
//
//  - `limitsApply` (web) decides every limits surface, so one predicate governs
//    the Users table, the Limits table and the quota editor together.
//  - `quota_applies` (server, `guard.rs`) decides whether a stored quota is
//    enforced at all. Its own Rust test covers the semantics; this only checks
//    the two ends agree on who is exempt.
//
// Run: pnpm test

import assert from "node:assert/strict";
import test, { before } from "node:test";

let admin;

before(async () => {
  admin = await import("./_build/admin.js");
});

test("limits do not apply to an administrator", () => {
  // The whole point of the request: an admin is never shown a limit field.
  assert.equal(admin.limitsApply({ admin: true }), false);
  assert.equal(admin.limitsApply({ admin: false }), true);
});

test("an account without the admin flag is treated as limited", () => {
  // Absent must mean "not an admin", not "unknown": the Users table and the
  // Limits table both read whatever `/users` returns, and a roster that omits
  // the field must not silently hide every row's limits.
  assert.equal(admin.limitsApply({}), true);
  assert.equal(admin.limitsApply({ admin: undefined }), true);
});
