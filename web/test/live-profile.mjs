// Unified profile card, against the real auth-enabled backend. Also proves a
// username change actually lands server-side and repaints the UI.
//
// The system service serves HTTPS with a self-signed cert, so this one runs
// with NODE_TLS_REJECT_UNAUTHORIZED=0 (Node's undici accepts the cert):
//   NODE_TLS_REJECT_UNAUTHORIZED=0 VM_USER=admin VM_PASS=... \
//     node test/live-profile.mjs [baseUrl]
//
// Credentials come from VM_USER/VM_PASS because the account can be renamed
// through the very feature under test — hardcoding them made this suite fail
// the moment someone changed their own username.
//
// jsdom cannot do file input or canvas, so the crop dialog itself is not
// exercised here — crop math is covered by crop.test.mjs.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";

const base = process.argv[2] ?? "https://127.0.0.1:8006";
const USER = process.env.VM_USER ?? "admin";
const PASS = process.env.VM_PASS ?? "password123";
// Optional non-admin account. When supplied, the suite also proves the admin
// panel is hidden from it and that the API refuses it regardless of the UI.
const LOW_USER = process.env.VM_LOW_USER;
const LOW_PASS = process.env.VM_LOW_PASS;

execFileSync(
  "npx",
  ["vite", "build", "--logLevel", "error", "--outDir", "test/_live",
   "--assetsDir", ".", "--target", "es2020"],
  { cwd: process.cwd(), stdio: "inherit", env: { ...process.env, VM_IIFE: "1" } },
);

const code = readFileSync("test/_live/app.iife.js", "utf8");
const cssFile = readdirSync("dist/assets").find((f) => f.endsWith(".css"));
assert.ok(cssFile, "dist/assets contains a built stylesheet (run: pnpm build)");
const css = readFileSync(`dist/assets/${cssFile}`, "utf8");

const virtualConsole = new VirtualConsole();
virtualConsole.on("jsdomError", (e) => {
  if (!/not implemented|canvas|WebSocket/i.test(String(e.message))) {
    console.error("jsdom error:", e.message);
  }
});

const dom = new JSDOM(
  `<!doctype html><html data-theme="dark"><head><style>${css}</style></head>` +
  `<body><div id="app"></div></body></html>`,
  { url: `${base}/`, runScripts: "dangerously", pretendToBeVisual: true, virtualConsole },
);
const { window } = dom;
const { document } = window;

// undici has no cookie jar, so carry the session cookie from login responses
// back into subsequent requests (the app's API relies on the session cookie).
let sessionCookie = "";
window.fetch = (input, init) => {
  const url = typeof input === "string" ? new URL(input, base).toString() : input;
  init = init ?? {};
  if (sessionCookie) {
    init.headers = { ...(init.headers ?? {}), Cookie: sessionCookie };
  }
  return fetch(url, init).then((res) => {
    const sc = res.headers.get("set-cookie");
    const m = sc?.match(/virt-man-session=[^;]+/);
    if (m) sessionCookie = m[0];
    return res;
  });
};
window.eval(code);

const wait = (ms) => new Promise((r) => window.setTimeout(r, ms));
const px = (el, prop) => parseFloat(window.getComputedStyle(el)[prop]) || 0;
const results = [];
const check = (name, fn) => {
  try {
    fn();
    results.push(["pass", name]);
  } catch (e) {
    results.push(["FAIL", `${name} — ${e.message}`]);
  }
};

await wait(2000);

check("login screen appears", () => {
  assert.ok(document.querySelector("#lu"), "username field");
  assert.ok(document.querySelector("#lp"), "password field");
});

document.querySelector("#lu").value = USER;
document.querySelector("#lp").value = PASS;
document.querySelector("#login-btn").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1600);

if (!document.querySelector("#profile-btn")) {
  console.error(`✖ could not sign in as ${USER} — set VM_USER/VM_PASS for this instance`);
  process.exit(1);
}

check("profile button renders in the sidebar after login", () => {
  assert.ok(document.querySelector("#profile-btn"), "profile button present");
  assert.ok(document.querySelector("#profile-btn .avatar"), "avatar shown in the button");
});

// One click opens the unified card — no popover step.
document.querySelector("#profile-btn").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(600);

check("one card holds identity, both names, password and API keys", () => {
  assert.ok(!document.querySelector(".pop-menu"), "no popover layer any more");
  const modal = document.querySelector(".modal-back.open");
  assert.ok(modal, "profile card open");
  assert.ok(modal.querySelector(".profile-id"), "identity header present");
  assert.ok(modal.querySelector("#pf-avatar-btn .avatar"), "avatar is the crop trigger");
  assert.ok(modal.querySelector("#pf-nick"), "display name field");
  assert.ok(modal.querySelector("#pf-user"), "username field");
  assert.ok(modal.querySelector("#pf-pass"), "current-password field");
  assert.ok(modal.querySelector("#pf-pass2"), "new-password field");
  assert.ok(modal.querySelector("#pf-keys"), "API key list");
  assert.ok(modal.querySelector("#pf-newkey"), "new API key button");
});

check("card padding follows the theme, not ad-hoc values", () => {
  const id = document.querySelector(".profile-id");
  const cs = window.getComputedStyle(id);
  // Same inset as .spec, the theme's standard bordered-block padding.
  assert.equal(cs.padding, "9px 11px", `identity header uses the theme inset (got ${cs.padding})`);
  // .actions inside a flex-gap modal body must not double up its .detail margin.
  const actions = document.querySelector(".modal-body > .actions");
  assert.equal(window.getComputedStyle(actions).marginBottom, "0px",
    "actions rows do not stack their detail-pane margin on the modal gap");
});

// The real bug: saving a username must change it server-side AND repaint.
const before = await window.fetch("/auth/me").then((r) => r.json());
const origUser = before.username;
const origNick = before.nickname;
const probeUser = `${origUser}-probe`;
document.querySelector("#pf-user").value = probeUser;
document.querySelector("#pf-nick").value = "Renamed";
document.querySelector("#pf-save").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1200);

const afterSave = await window.fetch("/auth/me").then((r) => r.json());

check("saving changes the username on the server", () => {
  assert.equal(afterSave.username, probeUser, "server reports the new username");
  assert.equal(afterSave.nickname, "Renamed", "server reports the new display name");
});

check("the card and sidebar both repaint after saving", () => {
  assert.equal(document.querySelector("#pf-who-user").textContent.split(" · ")[0], `@${probeUser}`,
    "card header shows the new username");
  assert.equal(document.querySelector("#pf-who-name").textContent, "Renamed",
    "card header shows the new display name");
  assert.equal(document.querySelector("#profile-btn .who b").textContent, "Renamed",
    "sidebar shows the new display name");
  assert.match(document.querySelector(".topbar .host").textContent, /Renamed/,
    "topbar shows the new display name");
});

// Restore the account exactly as it was, so the suite is repeatable.
await window.fetch("/me", {
  method: "PUT",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify({ username: origUser, nickname: origNick }),
});
const restored = await window.fetch("/auth/me").then((r) => r.json());
check("account restored for repeatability", () => {
  assert.equal(restored.username, origUser, `username back to ${origUser}`);
  assert.equal(restored.nickname, origNick, `nickname back to ${origNick}`);
});

// ------------------------------------------------------- topbar + admin panel

document.querySelector(".modal-back.open .x")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(300);

check("topbar order is theme, admin, sign out — all icon-only", () => {
  const quick = document.querySelector(".topbar-quick");
  assert.ok(quick, ".topbar-quick group exists");
  // Browser build must not offer Connect to WebUI at all.
  assert.ok(!document.querySelector("#webui-btn"), "no WebUI button in the browser build");
  const ids = [...quick.querySelectorAll("button")].map((b) => b.id);
  assert.deepEqual(ids, ["theme-btn", "admin-btn", "logout-btn"], `order is theme, admin, sign out (got ${ids})`);
  for (const id of ids) {
    const b = document.querySelector(`#${id}`);
    assert.ok(b.classList.contains("icon"), `${id} is an icon button`);
    assert.ok(b.querySelector("svg"), `${id} renders an icon`);
    assert.equal(b.textContent.trim(), "", `${id} has no text label`);
    assert.ok(b.title, `${id} has a tooltip`);
  }
});

document.querySelector("#admin-btn").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(900);

check("admin panel opens with Users, Roles and API keys", () => {
  const modal = document.querySelector(".modal-back.open");
  assert.ok(modal, "admin modal open");
  const tabs = [...modal.querySelectorAll("[data-admintab]")];
  assert.deepEqual(tabs.map((t) => t.dataset.admintab), ["users", "roles", "keys"]);
  // "Users & permissions" was renamed to just "Users".
  assert.equal(tabs[0].textContent.trim(), "Users", "first tab is titled Users");
  assert.ok(modal.querySelector("#admin-area table.grid"), "user table rendered from the live API");
  assert.ok(modal.querySelector("#add-user"), "add user button");
  assert.ok(modal.querySelector("[data-edituser]"), "per-user edit (role + permissions)");
});

check("the add-user button is spaced away from the table above it", () => {
  const actions = document.querySelector("#admin-area table.grid + .actions");
  assert.ok(actions, "actions row follows the table");
  const mt = px(actions, "marginTop");
  assert.ok(mt >= 8, `table and its action row are separated (margin-top ${mt}px)`);
});

check("role choices are spaced apart in the picker", () => {
  // The themed listbox popover renders the role options; a small gap keeps
  // adjacent role names from reading as one block.
  const pop = document.querySelector(".select-popover");
  assert.ok(pop, "select popover exists in the DOM");
  const gap = px(pop, "gap");
  assert.ok(gap > 0 && gap <= 4, `options have a tiny gap (${gap}px)`);
});

// ---- Roles tab: create a custom role, then clean it up.

document.querySelector('[data-admintab="roles"]').dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(900);

check("roles tab lists built-ins as a scope matrix", () => {
  const rows = [...document.querySelectorAll("#admin-area table.grid tr")];
  const names = rows.map((r) => r.querySelector(".tname")?.textContent?.trim()).filter(Boolean);
  for (const want of ["admin", "operator", "viewer"]) {
    assert.ok(names.some((n) => n.startsWith(want)), `${want} listed (${names})`);
  }
  // Header carries one column per enforced scope, from the backend.
  const heads = [...document.querySelectorAll("#admin-area table.grid th")].map((h) => h.textContent.trim());
  for (const scope of ["users", "vms", "networks", "storage", "settings", "connections"]) {
    assert.ok(heads.includes(scope), `${scope} column present (${heads})`);
  }
  // The scope list must come from the server, so it cannot offer a scope that
  // nothing enforces ("domains" used to be offered and silently ignored).
  assert.ok(!heads.includes("domains"), "no unenforced 'domains' scope");
  assert.ok(document.querySelector("#add-role"), "new role button");
  // Built-ins are editable but not deletable.
  const adminRow = [...document.querySelectorAll("#admin-area table.grid tr")]
    .find((r) => r.querySelector(".tname")?.textContent?.trim().startsWith("admin"));
  assert.ok(adminRow.querySelector("[data-editrole]"), "built-in role is editable");
  assert.ok(!adminRow.querySelector("[data-delrole]"), "built-in role has no delete");
});

document.querySelector("#add-role").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(400);

check("the role editor offers every enforced scope", () => {
  const modal = [...document.querySelectorAll(".modal-back.open")].pop();
  assert.ok(modal.querySelector("#er-name"), "name field");
  const scopes = [...modal.querySelectorAll("[data-rolescope]")].map((s) => s.dataset.rolescope);
  assert.deepEqual(scopes.sort(), ["connections", "networks", "settings", "storage", "users", "vms"].sort());
});

document.querySelector("#er-name").value = "probe-role";
document.querySelector("#er-save").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1200);

const rolesAfter = await window.fetch("/roles").then((r) => r.json());
check("creating a custom role persists it", () => {
  assert.ok(rolesAfter.roles.some((r) => r.name === "probe-role"), "probe-role created");
  const probe = rolesAfter.roles.find((r) => r.name === "probe-role");
  assert.equal(probe.builtin, false, "custom role is not builtin");
});

const cleanup = await window.fetch("/roles/probe-role", { method: "DELETE" });
check("a custom role can be deleted again", () => {
  assert.equal(cleanup.status, 200, "probe-role removed");
});

// ---- API keys tab: create one with its own permissions.

document.querySelector('[data-admintab="keys"]').dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(900);

check("keys tab can mint a key with its own permissions", () => {
  assert.ok(document.querySelector("#add-key"), "new API key button in the admin panel");
  document.querySelector("#add-key").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  const modal = [...document.querySelectorAll(".modal-back.open")].pop();
  assert.ok(modal.querySelector("#nk-name"), "key name field");
  assert.ok(modal.querySelector("#nk-owner"), "owner picker");
  const scopes = [...modal.querySelectorAll("[data-keyscope]")].map((s) => s.dataset.keyscope);
  assert.deepEqual(scopes.sort(), ["connections", "networks", "settings", "storage", "users", "vms"].sort());
  // Each scope must offer "inherit owner" plus the three levels.
  const first = modal.querySelector("[data-keyscope]");
  const vals = [...first.querySelectorAll("option")].map((o) => o.value);
  assert.deepEqual(vals, ["", "write", "read", "none"], "inherit + write/read/none");
});

const fails = results.filter(([s]) => s === "FAIL");

check("users no longer duplicated as a Datacenter tab", () => {
  assert.ok(!document.querySelector('[data-dctab="users"]'), "Datacenter has no Users tab");
});

// --------------------------------------------- non-admin sees no admin panel

if (LOW_USER && LOW_PASS) {
  // Sign out and back in as the low-privilege account in the same document.
  document.querySelector(".modal-back.open .x")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(200);
  sessionCookie = "";
  document.querySelector("#logout-btn").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(1000);
  document.querySelector("#lu").value = LOW_USER;
  document.querySelector("#lp").value = LOW_PASS;
  document.querySelector("#login-btn").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(1600);

  const who = await window.fetch("/auth/me").then((r) => r.json());
  const usersStatus = (await window.fetch("/users")).status;
  const keysStatus = (await window.fetch("/keys")).status;

  check("a non-admin gets no admin button, and the API refuses them anyway", () => {
    assert.notEqual(who.role, "admin", `signed in as a non-admin (${who.username}/${who.role})`);
    assert.ok(!document.querySelector("#admin-btn"), "admin button hidden");
    assert.ok(document.querySelector("#theme-btn"), "theme button still shown");
    assert.ok(document.querySelector("#logout-btn"), "sign out still shown");
    // Hiding the button is cosmetic; the server is the actual gate.
    assert.equal(usersStatus, 403, "GET /users forbidden");
    assert.equal(keysStatus, 403, "GET /keys forbidden");
  });

  check("the command palette hides the admin command from a non-admin", () => {
    document.dispatchEvent(new window.KeyboardEvent("keydown", { key: "k", ctrlKey: true, bubbles: true }));
    const labels = [...document.querySelectorAll(".tayne-row .tayne-label")].map((e) => e.textContent);
    assert.ok(labels.length, "palette rendered commands");
    assert.ok(!labels.some((l) => /Admin panel/.test(l)), "no Admin panel command");
    document.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  });
} else {
  results.push(["info", "set VM_LOW_USER/VM_LOW_PASS to also check the non-admin case"]);
}

for (const [s, msg] of results) {
  console.log(`${s === "pass" ? "✔" : s === "info" ? "·" : "✖"} ${msg}`);
}
window.close();
if (fails.length) {
  console.log(`\n${fails.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall profile checks passed");
