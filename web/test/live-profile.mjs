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
// Print whatever was collected if the script dies part-way, so a crash does
// not hide the checks that explain it.
process.on("exit", (code) => {
  if (code && !printed) for (const [s, m] of results) console.log(`${s === "pass" ? "✔" : "✖"} ${m}`);
});
let printed = false;
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

check("Save sits next to Cancel in the footer, hidden until something changes", () => {
  const foot = document.querySelector(".modal-back.open .modal-foot");
  assert.ok(foot, "footer present");
  const ids = [...foot.querySelectorAll("button")].map((b) => b.id);
  assert.deepEqual(ids, ["pf-close", "pf-save"], "Close then Save, both in the footer");
  // No edits yet, so Save must not be offered at all.
  const save = document.querySelector("#pf-save");
  assert.ok(save.classList.contains("hidden"), "Save hidden while the form is pristine");
  assert.equal(window.getComputedStyle(save).display, "none", "and actually not rendered");
  assert.equal(document.querySelector("#pf-close").textContent.trim(), "Close",
    "the other button reads Close when there is nothing to discard");
  // The old inline Save row above the Password section is gone.
  assert.ok(!document.querySelector(".modal-body #pf-save"), "Save is not in the body any more");
});

check("editing a name reveals Save and turns Close into Cancel", () => {
  const nick = document.querySelector("#pf-nick");
  const original = nick.value;
  nick.value = `${original}-edited`;
  nick.dispatchEvent(new window.Event("input", { bubbles: true }));
  const save = document.querySelector("#pf-save");
  assert.ok(!save.classList.contains("hidden"), "Save appears once a field differs");
  assert.equal(document.querySelector("#pf-close").textContent.trim(), "Cancel",
    "Close becomes Cancel while edits are pending");
  // Typing the original value back must retire the button again.
  nick.value = original;
  nick.dispatchEvent(new window.Event("input", { bubbles: true }));
  assert.ok(save.classList.contains("hidden"), "reverting hides Save again");
  assert.equal(document.querySelector("#pf-close").textContent.trim(), "Close", "and Cancel reverts to Close");
});

check("the empty API key list is bordered and compact, like a populated one", () => {
  const box = document.querySelector("#pf-keys");
  assert.ok(box.classList.contains("key-list"), "list carries the key-list class");
  // jsdom drops a `border` shorthand whose colour is var(), so the computed
  // style cannot prove the border. Assert the stylesheet rule instead, the
  // same way the theme audit does.
  assert.match(css.replace(/\s+/g, " "), /\.key-list\{border:1px solid var\(--line\)\}/,
    "key-list is bordered in the shipped stylesheet");
  const empty = box.querySelector(".empty");
  if (empty) {
    // .empty's default 24px is sized for a whole pane; inside a row list it
    // must use the same inset as a key row.
    const cs = window.getComputedStyle(empty);
    assert.equal(cs.padding, "7px 9px", `empty state uses the row inset (got ${cs.padding})`);
    assert.match(empty.textContent, /No API keys/, "names what is missing");
  } else {
    results.push(["info", "account has API keys, so the empty state was not rendered"]);
  }
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
const userField = document.querySelector("#pf-user");
const nickField = document.querySelector("#pf-nick");
userField.value = probeUser;
nickField.value = "Renamed";
// Save only exists once the form is dirty, so announce the edits.
userField.dispatchEvent(new window.Event("input", { bubbles: true }));
nickField.dispatchEvent(new window.Event("input", { bubbles: true }));
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

check("Save retires itself once the edits are persisted", () => {
  const save = document.querySelector("#pf-save");
  assert.ok(save.classList.contains("hidden"), "saved values are the new baseline");
  assert.equal(document.querySelector("#pf-close").textContent.trim(), "Close",
    "nothing left to discard, so Cancel goes back to Close");
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
  // The phone-only palette button is hidden on desktop; it is not part of this order.
  const ids = [...quick.querySelectorAll("button:not(.mobile-only)")].map((b) => b.id);
  assert.deepEqual(ids, ["theme-btn", "admin-btn", "logout-btn"], `order is theme, admin, sign out (got ${ids})`);
  for (const id of ids) {
    const b = document.querySelector(`#${id}`);
    assert.ok(b.classList.contains("icon"), `${id} is an icon button`);
    assert.ok(b.querySelector("svg"), `${id} renders an icon`);
    assert.equal(b.textContent.trim(), "", `${id} has no text label`);
    assert.ok(b.title, `${id} has a tooltip`);
  }
});

// A missing element is a failed check, not a crash that hides every result.
const click = (el) => el
  ? el.dispatchEvent(new window.MouseEvent("click", { bubbles: true }))
  : results.push(["FAIL", `clicked a missing element (after: ${results.at(-1)?.[1]})`]);
const topModal = () => [...document.querySelectorAll(".modal-back.open")].pop();
const beforeAdmin = { html: document.querySelector("#admin-btn").innerHTML, path: window.location.pathname };

click(document.querySelector("#admin-btn"));
await wait(3000);

check("the admin button opens a page, not a modal, on its dashboard", () => {
  assert.ok(!document.querySelector(".modal-back.open"), "no modal opened");
  assert.equal(window.location.pathname, "/admin/dashboard", "URL is the admin dashboard");
  assert.match(document.querySelector("#content .dtitle").textContent, /Dashboard/, "content pane shows the dashboard");
});

check("the admin dashboard is tabbed, in the datacenter dashboard's parts", () => {
  const area = document.querySelector("#admin-area");
  const tabs = [...area.querySelectorAll(".tabbar .tab")].map((t) => t.dataset.dashTab);
  assert.deepEqual(tabs, ["overview", "usage", "ownership", "activity"]);
  assert.equal(area.querySelector(".tab.active")?.dataset.dashTab, "overview");
  const labels = [...area.querySelectorAll(".cards .card i")].map((i) => i.textContent.trim());
  for (const want of ["USERS", "ADMINISTRATORS", "GROUPS", "PERMISSIONS", "API KEYS"]) {
    assert.ok(labels.includes(want), `${want} card (${labels})`);
  }
  // Metric panels, as on the datacenter dashboard, not a wall of number cards.
  const panels = [...area.querySelectorAll(".sumgrid .metric .metric-head b")].map((b) => b.textContent.trim());
  assert.deepEqual(panels, ["Daemons", "VMs", "Networks", "Activity today"]);
  assert.ok(area.querySelectorAll(".sumgrid .meter").length === 4, "each panel has a meter");
  assert.ok(!/Tightest/i.test(area.textContent), "no combined 'tightest limit' figure");
});

check("the admin sidebar groups its sections under the dashboard root", () => {
  const tree = document.querySelector("#tree");
  const root = tree.querySelector(".tree-root");
  assert.equal(root.dataset.adminSection, "dashboard", "the root row is the dashboard");
  assert.ok(root.classList.contains("sel"), "and selected");
  const groups = [...tree.querySelectorAll(".admin-group")].map((g) => g.textContent.trim());
  assert.deepEqual(groups, ["Access", "Infrastructure", "Limits", "Audit"]);
  // Every section, once, each after its group label.
  const order = [...tree.children].map((el) => el.classList.contains("admin-group") ? `#${el.textContent.trim()}` : el.dataset.adminSection);
  assert.deepEqual(order, ["dashboard", "#Access", "users", "groups", "roles", "permissions", "keys",
    "#Infrastructure", "daemons", "vms", "networks", "pools", "ips", "#Limits", "quotas", "#Audit", "activity"]);
  assert.ok(!tree.querySelector("#dc-row"), "the resource tree is replaced, not appended to");
});

click(document.querySelector('#admin-area [data-dash-tab="usage"]'));
await wait(3000);
check("the Usage tab shows every limit as its own meter", () => {
  assert.equal(window.location.pathname, "/admin/dashboard/usage", "the tab is in the URL");
  const area = document.querySelector("#admin-area");
  assert.equal(area.querySelector(".tab.active")?.dataset.dashTab, "usage");
  const card = area.querySelector("[data-quota-user]");
  assert.ok(card, `a limited user's card (${area.textContent.slice(0, 160)})`);
  const specs = [...card.querySelectorAll(".spec b")].map((b) => b.textContent.trim());
  assert.deepEqual(specs, ["VMs", "vCPUs", "Memory", "Storage", "Networks"], "five separate limits");
  // bob is limited on four of the five, so four meters; networks is unlimited.
  assert.equal(card.querySelectorAll(".limit-spec .meter").length, 4, "one meter per set limit");
  assert.match(card.textContent, /no limit/, "an unset limit says so instead of a meter");
});
// A dashboard tab survives a reload: Back to it re-renders the same tab.
window.history.pushState(null, "", "/admin/dashboard/ownership");
window.dispatchEvent(new window.PopStateEvent("popstate"));
await wait(3000);
check("a dashboard tab URL opens that tab", () => {
  assert.equal(document.querySelector("#admin-area .tab.active")?.dataset.dashTab, "ownership");
  assert.equal(document.querySelector("#tree .tree-row.sel")?.dataset.adminSection, "dashboard");
});
click(document.querySelector('#admin-area [data-dash-tab="overview"]'));
await wait(3000);
check("the first tab is the bare section path", () => {
  assert.equal(window.location.pathname, "/admin/dashboard");
});


click(document.querySelector('#admin-area .card[data-go="users"]'));
await wait(900);
check("a dashboard card opens its section", () => {
  assert.equal(window.location.pathname, "/admin/users");
  assert.ok(document.querySelector("#admin-area table.grid"), "user table rendered from the live API");
});

// jsdom evaluates no media queries, so this is the desktop's view of the
// shipped stylesheet: the phone-only buttons must not be displayed.
check("phone-only chrome is not shown on a desktop", () => {
  for (const id of ["nav-btn", "palette-btn"]) {
    const el = document.querySelector(`#${id}`);
    assert.ok(el, `#${id} exists (for phones)`);
    assert.equal(window.getComputedStyle(el).display, "none", `#${id} hidden`);
  }
});

check("the admin button stays put and becomes a return button", () => {
  const quick = [...document.querySelectorAll(".topbar-quick button:not(.mobile-only)")].map((b) => b.id);
  assert.deepEqual(quick, ["theme-btn", "admin-btn", "logout-btn"], "same slot");
  const b = document.querySelector("#admin-btn");
  assert.equal(b.title, "Return", "tooltip says Return");
  assert.notEqual(b.innerHTML, beforeAdmin.html, "icon changed");
  assert.ok(b.classList.contains("on"), "highlighted while the admin page is open");
});

// Every section renders from the live API without an error panel.
for (const sec of ["users", "groups", "roles", "permissions", "daemons", "vms", "networks", "pools", "quotas", "ips", "keys", "activity"]) {
  click(document.querySelector(`#tree [data-admin-section="${sec}"]`));
  await wait(["vms", "networks", "pools", "ips", "permissions"].includes(sec) ? 2500 : 900);
  check(`admin section ${sec} renders and has its own URL`, () => {
    assert.equal(window.location.pathname, `/admin/${sec}`);
    assert.equal(document.querySelector("#tree .tree-row.sel")?.dataset.adminSection, sec, "sidebar follows");
    const area = document.querySelector("#admin-area");
    assert.ok(area.querySelector(sec === "pools" ? ".storage-card, .empty" : "table.grid"),
      `${sec}: rendered (got ${area.textContent.slice(0, 120)})`);
    if (["vms", "networks", "pools"].includes(sec)) {
      const tabs = [...area.querySelectorAll("[data-res-tab]")];
      assert.ok(tabs.length >= 1, `${sec}: one tab per daemon`);
      assert.equal(area.querySelector(".tab.active")?.dataset.resTab, tabs[0].dataset.resTab, `${sec}: first daemon open`);
    }
    assert.ok(!area.querySelector(":scope > .empty"), `${sec}: no error panel`);
    // Add / create buttons sit at the right of their row. jsdom does no
    // layout, so check the computed rule the layout comes from.
    const add = area.querySelector('button.btn[id^="add-"], #act-clear');
    if (add) {
      const row = add.closest(".actions");
      assert.equal(window.getComputedStyle(row).justifyContent, "flex-end", `${sec}: ${add.id} is right-aligned`);
    }
  });
}

// ---- Roles: privilege-based; a custom role round-trips through the editor.
click(document.querySelector('#tree [data-admin-section="roles"]'));
await wait(900);
check("roles list the built-ins with their privileges, fixed", () => {
  const names = [...document.querySelectorAll("#admin-area .tname")].map((t) => t.textContent.trim().split(" ")[0]);
  for (const want of ["Administrator", "User", "VMUser", "NoAccess"]) assert.ok(names.includes(want), `${want} in ${names}`);
  assert.ok(document.querySelector('#admin-area [data-view-role="User"]'), "built-in opens read-only");
  assert.ok(!document.querySelector('#admin-area [data-del-role="User"]'), "built-in has no delete");
});
click(document.querySelector("#add-role"));
await wait(300);
check("the role editor offers a switch for every enforced privilege", () => {
  const privs = [...topModal().querySelectorAll("[data-priv]")].map((c) => c.dataset.priv);
  for (const want of ["VM.Allocate", "VM.Audit", "VM.PowerMgmt", "Network.Use", "Bridge.Use", "Datastore.AllocateSpace"]) {
    assert.ok(privs.includes(want), `${want} offered`);
  }
});
topModal().querySelector("#r-name").value = "ProbeRole";
// A switch: click it only when it is not already in the wanted state.
const flip = (sel, on) => {
  const sw = topModal().querySelector(sel);
  if (sw.checked !== on) click(sw);
};
flip('[data-priv="VM.Audit"]', true);
flip('[data-priv="VM.Console"]', true);
check("subsystem master switch selects all its privileges", () => {
  flip('[data-master="VM"]', true);
  for (const b of topModal().querySelectorAll('[data-priv^="VM."]')) {
    assert.equal(b.checked, true);
  }
  flip('[data-master="VM"]', false);
  for (const b of topModal().querySelectorAll('[data-priv^="VM."]')) {
    assert.equal(b.checked, false);
  }
  flip('[data-priv="VM.Audit"]', true);
  flip('[data-priv="VM.Console"]', true);
});
click(topModal().querySelector("#r-save"));
await wait(1500);
const rolesAfter = await window.fetch("/roles").then((r) => r.json());
check("a custom role persists with exactly the ticked privileges", () => {
  const probe = rolesAfter.roles.find((r) => r.name === "ProbeRole");
  assert.ok(probe, "created");
  assert.deepEqual(probe.privs, ["VM.Audit", "VM.Console"]);
});

// ---- Groups and grants: a group, granted a role on a path.
await window.fetch("/groups", { method: "POST", headers: { "content-type": "application/json" },
  body: JSON.stringify({ name: "probe-group", comment: "live test", members: [] }) });
click(document.querySelector('#tree [data-admin-section="permissions"]'));
await wait(2500);
click(document.querySelector("#add-acl"));
await wait(2500);
check("adding a permission picks a path from the daemons' real resources", () => {
  const m = topModal();
  const paths = [...m.querySelectorAll("#a-path option")].map((o) => o.value);
  assert.ok(paths.includes("/") && paths.some((p) => /^\/nodes\/[^/]+\/vms\/.+/.test(p)), `paths: ${paths.slice(0, 8)}`);
  assert.ok([...m.querySelectorAll("#a-ugid option")].some((o) => o.value === "group:probe-group"), "groups offered");
});
{
  const m = topModal();
  m.querySelector("#a-path").value = "/nodes";
  m.querySelector("#a-ugid").value = "group:probe-group";
  m.querySelector("#a-role").value = "ProbeRole";
  click(m.querySelector("#a-save"));
}
await wait(2000);
const aclAfter = await window.fetch("/acl").then((r) => r.json());
check("the grant is stored on the chosen path", () => {
  assert.ok(aclAfter.some((e) => e.path === "/nodes" && e.ugid === "group:probe-group" && e.role === "ProbeRole"), JSON.stringify(aclAfter));
});
await window.fetch("/acl", { method: "DELETE", headers: { "content-type": "application/json" },
  body: JSON.stringify({ path: "/nodes", ugid: "group:probe-group", role: "ProbeRole" }) });
await window.fetch("/groups/probe-group", { method: "DELETE" });
const cleanup = await window.fetch("/roles/ProbeRole", { method: "DELETE" });
check("probe role, group and grant cleaned up", () => assert.equal(cleanup.status, 200));

// ---- Quotas editor: per-daemon vs pooled, and per-daemon overrides.
click(document.querySelector('#tree [data-admin-section="quotas"]'));
await wait(900);
click(document.querySelector("#admin-area [data-quota]"));
await wait(300);
check("the limits editor offers mode, five limits and per-daemon overrides", () => {
  const m = topModal();
  assert.deepEqual([...m.querySelectorAll("#q-mode option")].map((o) => o.value), ["per", "shared"]);
  assert.deepEqual([...m.querySelectorAll("[data-ql]")].map((i) => i.dataset.ql), ["vms", "vcpus", "memory_mb", "storage_gb", "networks"]);
  assert.ok(m.querySelector("[data-qd-on]"), "a daemon override toggle");
  click(m.querySelector("#q-cancel"));
});

// ---- API keys: create one limited to a privilege list, shown once.
click(document.querySelector('#tree [data-admin-section="keys"]'));
await wait(900);
click(document.querySelector("#add-key"));
await wait(300);
check("a key can be limited with Yes/No privilege controls; name may be blank", () => {
  const m = topModal();
  assert.ok(m.querySelector("#k-owner"), "owner picker");
  assert.ok(m.querySelector('[data-kpriv="VM.Audit"][data-value="yes"]'), "privilege Yes button");
  assert.ok(m.querySelector('[data-kmaster="Sys"][data-value="yes"]'), "subsystem master Yes button");
  assert.match(m.querySelector("#k-name").placeholder, /blank/i);
  click(m.querySelector('[data-kpriv="Sys.Audit"][data-value="yes"]'));
});
click(topModal().querySelector("#k-save"));
await wait(1500);
const mintedKeys = await window.fetch("/keys").then((r) => r.json());
check("the key is shown once, generated name, limited privileges", () => {
  const secret = document.querySelector(".secret-value");
  assert.ok(secret, "secret shown in a dialog");
  assert.match(secret.textContent, /^vm_[0-9a-f]+$/);
  const k = mintedKeys.find((x) => /^[a-z]+-[a-z]+-[0-9a-f]{4}$/.test(x.name));
  assert.ok(k, `generated name (${mintedKeys.map((x) => x.name)})`);
  assert.deepEqual(k.privs, ["Sys.Audit"]);
  click(topModal().querySelector("#sv-done"));
});
for (const k of mintedKeys) await window.fetch(`/keys/${k.id}`, { method: "DELETE" });

// ---- Admin context menus: rows offer their own actions; the sidebar offers
// admin actions, not the resource tree's.
{
  const rc = (el) => {
    const e = new window.MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 50, clientY: 50 });
    el.dispatchEvent(e);
    return e.defaultPrevented;
  };
  const labels = () => [...document.querySelectorAll(".ctxmenu .ctxmenu-label")].map((x) => x.textContent.trim());
  click(document.querySelector('#tree [data-admin-section="users"]'));
  await wait(1200);
  const row = [...document.querySelectorAll("#admin-area tr")].find((r) => r.querySelector("[data-edit-user]"));
  const prevented = rc(row.querySelector("td"));
  check("right-clicking an admin row offers that row's actions", () => {
    assert.ok(prevented, "native menu replaced");
    assert.deepEqual(labels(), ["Edit", "Permissions", "Limits", "Delete"]);
    const del = [...document.querySelectorAll(".ctxmenu-item")].find((b) => b.textContent.includes("Delete"));
    assert.ok(del.classList.contains("danger"), "Delete is marked destructive");
  });
  // An item runs the row's own button: Edit opens the same dialog.
  [...document.querySelectorAll(".ctxmenu-item")].find((b) => b.textContent.includes("Edit")).click();
  await wait(300);
  check("a menu item does what the row's button does", () => {
    assert.match(topModal()?.querySelector(".modal-head")?.textContent ?? "", /Edit/);
    topModal().remove();
  });
  rc(document.querySelector('#tree [data-admin-section="groups"]'));
  check("the admin sidebar offers admin actions, not the resource tree's", () => {
    const l = labels();
    assert.deepEqual(l, ["Open Groups", "Add Group", "Return"]);
    assert.ok(!l.some((x) => /Add server|Create VM|Add resource/i.test(x)), `no resource-tree items (${l})`);
  });
  [...document.querySelectorAll(".ctxmenu-item")].find((b) => b.textContent.includes("Add Group")).click();
  await wait(1500);
  check("a sidebar action opens the section and runs its button", () => {
    assert.equal(window.location.pathname, "/admin/groups");
    assert.match(topModal()?.querySelector(".modal-head")?.textContent ?? "", /Add group/);
    topModal().remove();
  });
  rc(document.querySelector("#admin-area .section-title, #admin-area .hint, #admin-area") ?? document.body);
  document.querySelector(".ctxmenu")?.remove();
}

// ---- A reload of an admin URL lands on that section.
// Self-contained: it used to assert whatever path the previous block happened
// to leave behind, which is not what the comment above says it checks.
window.history.pushState(null, "", "/admin/keys");
window.dispatchEvent(new window.PopStateEvent("popstate"));
await wait(1200);
check("an admin URL parses to its section", () => {
  assert.equal(window.location.pathname, "/admin/keys");
  assert.equal(document.querySelector("#tree .tree-row.sel")?.dataset.adminSection, "keys", "sidebar follows");
  assert.match(document.querySelector("#content .dtitle")?.textContent ?? "", /API Keys/, "the section painted");
});

// ---- Return.
click(document.querySelector("#admin-btn"));
await wait(900);
check("return goes back to where the admin page was opened from", () => {
  assert.equal(window.location.pathname, beforeAdmin.path, "back to the previous view");
  assert.ok(document.querySelector("#tree #dc-row"), "resource tree is back");
  const b = document.querySelector("#admin-btn");
  assert.equal(b.title, "Administration");
  assert.equal(b.innerHTML, beforeAdmin.html, "icon restored");
});

// ---- Phone: the admin sidebar is the drawer, and choosing a section closes it.
window.innerWidth = 390;
click(document.querySelector("#admin-btn"));
await wait(900);
click(document.querySelector("#nav-btn"));
check("on a phone the admin sections live in the drawer", () => {
  assert.ok(document.body.classList.contains("nav-open"), "drawer opens");
  assert.ok(document.querySelector("#tree [data-admin-section]"), "drawer holds the admin sections");
});
click(document.querySelector('#tree [data-admin-section="groups"]'));
await wait(900);
check("choosing a section closes the drawer", () => {
  assert.equal(window.location.pathname, "/admin/groups");
  assert.ok(!document.body.classList.contains("nav-open"), "drawer closed by the navigation");
});
click(document.querySelector("#admin-btn"));
await wait(600);

check("the Datacenter has only Dashboard and Guests tabs", () => {
  click(document.querySelector("#dc-row"));
  const tabs = [...document.querySelectorAll("[data-dctab]")].map((t) => t.dataset.dctab);
  assert.deepEqual(tabs, ["dashboard", "guests"], "no Servers (it is the admin Daemons page) and no Users");
});

window.history.pushState(null, "", "/servers");
window.dispatchEvent(new window.PopStateEvent("popstate"));
await wait(900);
check("an old /servers link opens the admin Daemons page", () => {
  assert.equal(window.location.pathname, "/admin/daemons");
  assert.equal(document.querySelector("#tree .tree-row.sel")?.dataset.adminSection, "daemons");
});
click(document.querySelector("#admin-btn"));
await wait(600);

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
    assert.equal(who.admin, false, `signed in as a non-admin (${who.username})`);
    assert.ok(!document.querySelector("#admin-btn"), "admin button hidden");
    assert.ok(document.querySelector("#theme-btn"), "theme button still shown");
    assert.ok(document.querySelector("#logout-btn"), "sign out still shown");
    // Hiding the button is cosmetic; the server is the actual gate.
    assert.equal(usersStatus, 403, "GET /users forbidden");
    assert.equal(keysStatus, 403, "GET /keys forbidden");
  });

  const adminPage = (await window.fetch("/acl")).status;
  check("an /admin link does not open the admin page for a non-admin", () => {
    assert.equal(adminPage, 403, "GET /acl forbidden");
    window.history.pushState(null, "", "/admin/users");
    window.dispatchEvent(new window.PopStateEvent("popstate"));
    assert.ok(!document.querySelector("#tree [data-admin-section]"), "no admin sidebar");
    assert.ok(document.querySelector("#tree #dc-row"), "resource tree shown instead");
  });

  window.history.pushState(null, "", "/servers");
  window.dispatchEvent(new window.PopStateEvent("popstate"));
  await wait(600);
  check("an old /servers link sends a non-admin to the datacenter", () => {
    assert.equal(window.location.pathname, "/", "normalised to the dashboard");
    assert.ok(!document.querySelector("#tree [data-admin-section]"), "no admin sidebar");
  });

  check("the command palette hides the admin command from a non-admin", () => {
    document.dispatchEvent(new window.KeyboardEvent("keydown", { key: "k", ctrlKey: true, bubbles: true }));
    const labels = [...document.querySelectorAll(".tayne-row .tayne-label")].map((e) => e.textContent);
    assert.ok(labels.length, "palette rendered commands");
    assert.ok(!labels.some((l) => /Administration/.test(l)), "no Administration command");
    document.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  });
} else {
  results.push(["info", "set VM_LOW_USER/VM_LOW_PASS to also check the non-admin case"]);
}

// Counted after every check has run: counting earlier let the non-admin
// checks below it fail without failing the suite.
const fails = results.filter(([s]) => s === "FAIL");
printed = true;
for (const [s, msg] of results) {
  console.log(`${s === "pass" ? "✔" : s === "info" ? "·" : "✖"} ${msg}`);
}
window.close();
if (fails.length) {
  console.log(`\n${fails.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall profile checks passed");
