// The profile's Resource Limits section, and how the base role is named.
//
// An administrator has no limits, so the whole section is removed for them --
// heading included. The server is the other half of that (it no longer enforces
// an admin's stored quota, `guard::quota_applies`), so a section that says
// "Administrators have no resource limits" was describing a rule the server had
// stopped applying.
//
// Driven by running the real app in jsdom with `fetch` stubbed, because
// `openProfile`/`loadProfileLimits` are closures in the app entry and cannot be
// imported. Answers come from a stub rather than a backend, so this runs
// anywhere.
//
// Run: pnpm test

import assert from "node:assert/strict";
import test, { before } from "node:test";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";

const BASE = "https://127.0.0.1:8006";

let appCode;

before(() => {
  // The app is only reachable as a bundle; build the IIFE the live suites use.
  execFileSync("npx", ["vite", "build", "--logLevel", "error", "--outDir", "test/_live",
    "--assetsDir", ".", "--target", "es2020"],
    { cwd: process.cwd(), stdio: "ignore", env: { ...process.env, VM_IIFE: "1" } });
  appCode = readFileSync("test/_live/app.iife.js", "utf8");
});

/// Boot the app with `me` as the signed-in account, open the profile, and
/// return the profile element plus its Resource Limits container.
async function openProfileWith(me, usage) {
  const { JSDOM, VirtualConsole } = await import("jsdom");
  const vc = new VirtualConsole();
  const dom = new JSDOM(
    '<!doctype html><html data-theme="dark"><body><div id="app"></div></body></html>',
    { url: `${BASE}/`, runScripts: "dangerously", pretendToBeVisual: true, virtualConsole: vc },
  );
  const { window } = dom;
  const json = (body) => Promise.resolve({
    ok: true, status: 200,
    headers: { get: () => null },
    json: () => Promise.resolve(body),
  });
  window.fetch = (input) => {
    const url = typeof input === "string" ? input : input.url;
    if (url.includes("/auth/status")) return json({ needs_setup: false, auth_disabled: false });
    if (url.includes("/auth/me")) return json(me);
    if (url.includes("/me/usage")) return json(usage);
    // Everything else a boot touches: an empty list is fine.
    return json([]);
  };
  window.eval(appCode);
  // Let boot settle, then open the profile from the sidebar button.
  await new Promise((r) => window.setTimeout(r, 60));
  const btn = window.document.querySelector("#profile-btn");
  assert.ok(btn, "the profile button renders after boot");
  btn.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await new Promise((r) => window.setTimeout(r, 60));
  const modal = window.document.querySelector(".modal-back.open") ?? window.document.body;
  return { window, modal };
}

const ADMIN = { id: "a1", username: "root", nickname: "Root", avatar: null, admin: true, privs: [] };
const USER = { id: "u1", username: "jane", nickname: "Jane", avatar: null, admin: false, privs: [] };
const NO_QUOTA = { admin: false, quota: { shared: false, default: {}, daemons: {} }, total: {}, daemons: [] };

test("an administrator sees no Resource Limits section at all", async () => {
  const { window, modal } = await openProfileWith(ADMIN, { ...NO_QUOTA, admin: true });
  const limits = modal.querySelector("#pf-limits");
  assert.ok(limits, "the container exists, so 'absent' is the container being empty");
  assert.equal(
    limits.innerHTML.trim(), "",
    `the section must be removed for an admin, not explained: ${limits.innerHTML}`,
  );
  assert.ok(
    !modal.textContent.includes("Resource Limits"),
    "the heading must go with it -- a heading plus an explanation is still a section",
  );
  assert.ok(
    !modal.textContent.includes("no resource limits"),
    "and so must the explanation",
  );
  window.close();
});

test("a non-admin still gets the Resource Limits section", async () => {
  // The removal must be about being an administrator, not about the profile
  // page having changed for everyone.
  const { window, modal } = await openProfileWith(USER, NO_QUOTA);
  const limits = modal.querySelector("#pf-limits");
  assert.ok(limits, "the container exists for a non-admin");
  assert.equal(limits.textContent.trim(), "Resource LimitsYou have no resource limits.",
    "a user with no limits is told so under the heading");
  window.close();
});

test("the base role is named as the server names it", async () => {
  // `perm::ADMIN_ROLE`/`DEFAULT_ROLE` are "Administrator"/"User"; the profile
  // header said "administrator"/"user". Both render spots must agree.
  const admin = await openProfileWith(ADMIN, { ...NO_QUOTA, admin: true });
  const adminWho = admin.modal.querySelector("#pf-who-user");
  assert.ok(adminWho, "the identity line renders");
  assert.match(adminWho.textContent, /·\s*Administrator$/, `got: ${adminWho.textContent}`);
  admin.window.close();

  const user = await openProfileWith(USER, NO_QUOTA);
  const userWho = user.modal.querySelector("#pf-who-user");
  assert.match(userWho.textContent, /·\s*User$/, `got: ${userWho.textContent}`);
  user.window.close();
});
