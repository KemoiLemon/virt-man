// A VM must be selectable directly from the Datacenter view.
//
// Usage: node test/live-dc-vm.mjs [baseUrl]

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";
import { ensureTestPool, ensureTestVm, removeTestPool, removeTestVm } from "./_fixture.mjs";

const base = process.argv[2] ?? "http://127.0.0.1:8807";

execFileSync("npx", ["vite", "build", "--logLevel", "error", "--outDir", "test/_live"], {
  cwd: process.cwd(), stdio: "inherit", env: { ...process.env, VM_IIFE: "1" },
});
const code = readFileSync("test/_live/app.iife.js", "utf8");
const css = readFileSync(
  `dist/assets/${readdirSync("dist/assets").find((f) => f.endsWith(".css"))}`, "utf8");

const virtualConsole = new VirtualConsole();
virtualConsole.on("jsdomError", (e) => {
  if (!/not implemented|canvas|WebSocket/i.test(String(e.message))) {
    console.error("jsdom error:", e.message);
  }
});
// noVNC's failed WebSocket logs through console.error; it is expected here
// (jsdom has no real socket) and would otherwise drown the results.
virtualConsole.on("error", () => {});
const dom = new JSDOM(
  `<!doctype html><html data-theme="dark"><head><style>${css}</style></head>` +
  `<body><div id="app"></div></body></html>`,
  { url: `${base}/`, runScripts: "dangerously", pretendToBeVisual: true, virtualConsole },
);
const { window } = dom;
const { document } = window;
// The 1s poll rebuilds the open view when the tree it reads has changed. Set
// this to perturb one guest list, which is the same `changed` signal a pool's
// allocation ticking produces, without touching the host's real resources.
let nudgeTree = false;
window.fetch = (input, init) => {
  const url = typeof input === "string" ? new URL(input, base).toString() : input;
  const res = fetch(url, init);
  if (!nudgeTree || !String(typeof url === "string" ? url : url?.url).endsWith("/domains")) return res;
  nudgeTree = false;
  return res.then(async (r) => {
    const data = await r.json();
    if (Array.isArray(data) && data[0]) data[0].title = `${data[0].title ?? data[0].name} `;
    return new Response(JSON.stringify(data), { status: r.status, headers: { "content-type": "application/json" } });
  });
};
// jsdom has no WebSocket, but the viewer no longer needs one: the RFB client
// lives in the daemon and frames arrive over fetch. Publish the globals the
// bundle expects when it runs outside a module scope.
globalThis.window = window;
globalThis.document = document;
// jsdom's canvas has no 2D context, so painting a frame would throw. The
// viewer's putImageData sits behind an optional call, so it degrades to a
// no-op here; geometry and controls are still assertable.
window.ResizeObserver ??= class { observe() {} unobserve() {} disconnect() {} };
// Live suites own their fixture: reuse a guest if the host has one,
// otherwise define a disposable one so the run never depends on
// whatever happens to be on this machine. The Storage tab needs a pool, so one
// is provisioned first.
const testPool = await ensureTestPool(base);
const fixture = await ensureTestVm(base);
window.eval(code);
const wait = (ms) => new Promise((r) => window.setTimeout(r, ms));
await wait(2800);

const results = [];
const check = (name, fn) => {
  try { fn(); results.push(["pass", name]); }
  catch (e) { results.push(["FAIL", `${name} — ${e.message}`]); }
};

// ---- the reported bug: selecting a guest in the SIDEBAR --------------------

check("sidebar guest rows carry their owning connection", () => {
  const guest = document.querySelector(".tree-row.tree-vm");
  assert.ok(guest, "a guest row is rendered");
  assert.ok(guest.dataset.vm, "row knows its VM name");
  assert.ok(
    guest.dataset.vmConn,
    "row knows which server owns it — without this the VM view has no " +
    "connection and the click silently does nothing",
  );
});

await (async () => {
  // Starting state is the Datacenter view, where selectedConn is null. This
  // is exactly the case that used to fail.
  const guest = document.querySelector(".tree-row.tree-vm");
  const name = guest.dataset.vm;
  // A guest is named by its UUID; the page shows the title the user typed.
  const shown = guest.querySelector(".tree-label").textContent.trim();
  guest.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(1600);

  check("clicking a sidebar guest from Datacenter opens that VM", () => {
    const title = document.querySelector(".dtitle")?.textContent.trim();
    assert.equal(title, shown, `VM view opened for ${name} (title: ${title})`);
    const tabs = [...document.querySelectorAll(".tabbar .tab")].map((t) => t.textContent.trim());
    assert.deepEqual(tabs, ["Summary", "Console", "Hardware", "Options", "Network", "XML"], "VM tabs render");
  });

  check("the sidebar marks that guest selected", () => {
    const sel = document.querySelector(".tree-row.tree-vm.sel");
    assert.ok(sel, "a guest row carries .sel");
    assert.equal(sel.dataset.vm, name, "the clicked guest is the selected one");
  });
})();

// Back to the Datacenter for the tab checks.
document.querySelector("#dc-row").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1200);

// ---- Guests get their own tab ---------------------------------------------

check("Datacenter has Dashboard, Guests, Containers, Networks and Storage tabs, and no Servers tab", () => {
  const tabs = [...document.querySelectorAll(".tabbar .tab")].map((t) => t.textContent.trim());
  // Servers moved to the admin page's Daemons section.
  assert.deepEqual(tabs, ["Dashboard", "Guests", "Containers", "Networks", "Storage"], `tabs (${tabs})`);
  // The landing dashboard has no guest cards either; guests are their own tab.
  assert.equal(document.querySelectorAll("[data-dc-vm]").length, 0,
    "guests are not listed on the Dashboard tab");
});

check("each datacenter tab has its own URL, and reloading one lands on it", () => {
  // The path is written by syncUrl on every view render, so a tab that is a
  // view has a path; the RESERVED/DC_TABS list is what keeps it from being
  // read back as a node name.
  const paths = {};
  for (const name of ["Guests", "Networks", "Storage"]) {
    [...document.querySelectorAll(".tabbar .tab")]
      .find((t) => t.textContent.trim() === name)
      ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
    paths[name] = window.location.pathname;
  }
  assert.deepEqual(paths,
    { Guests: "/guests", Networks: "/networks", Storage: "/storage" },
    JSON.stringify(paths));
});

// ---- Networks and Storage list every server's, like Guests ------------------

const connOf = (el) => el.getAttribute("data-net-conn") ?? el.getAttribute("data-pool-conn");
for (const [tab, label, card] of [
  ["Networks", "networks", "[data-dc-net], [data-net-open]"],
  ["Storage", "storage pools", "[data-pool-open]"],
]) {
  [...document.querySelectorAll(".tabbar .tab")]
    .find((t) => t.textContent.trim() === tab)
    ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(600);
  const cards = [...document.querySelectorAll(card)];
  check(`${tab} tab lists every server's ${label}, each naming its server`, () => {
    const body = document.querySelector("#content")?.textContent ?? "";
    assert.match(body, /SERVERS/, "quick statistics head the tab");
    assert.match(body, tab === "Networks" ? /NETWORKS/ : /POOLS/, `counts ${label}`);
    assert.ok(cards.length > 0, `${label} listed (${cards.length})`);
    for (const c of cards) assert.ok(connOf(c), "the card carries its owning server");
  });
  results.push(["info", `${label}: ${cards.length} card(s)`]);
}

// Creating from the Datacenter must not select the daemon on the page behind
// the form: choosing a daemon there is a choice about the resource, not a
// navigation. This is the regression that kept coming back, so it drives the
// pick-a-daemon path itself -- with NO server selected, which is the only case
// that path is reached in.
document.querySelector("#dc-row").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(400);
check("creating from the Datacenter keeps the Datacenter", () => {
  [...document.querySelectorAll(".tabbar .tab")]
    .find((t) => t.textContent.trim() === "Storage")
    ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  document.querySelector("#dc-pool-create")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  document.querySelector("#dc-net-create")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  assert.equal(window.location.pathname, "/storage", `still on the Datacenter`);
  const tabs = [...document.querySelectorAll(".tabbar .tab")].map((t) => t.textContent.trim());
  assert.ok(tabs.includes("Guests"), `datacenter tabs still there (${tabs})`);
});

// The real case: no server selected at all, so the daemon picker opens. The
// tree must not gain a selection and the Datacenter must stay on screen.
check("the daemon picker does not select the daemon it picks", () => {
  // Clear any selection the checks above left, then open the picker.
  document.querySelector("#dc-row")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  // The tabs' own create buttons exist only in an empty state, and this host has
  // pools, so reach the picker the way a user does: Add Resource, then Storage Pool.
  document.querySelector("#dc-row")?.dispatchEvent(new window.MouseEvent("contextmenu", {
    bubbles: true, cancelable: true, clientX: 50, clientY: 50,
  }));
  [...document.querySelectorAll(".ctxmenu-item")]
    .find((b) => b.textContent.trim() === "Add Resource")
    ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  document.querySelector('.modal-back.open [data-create="pool"]')
    ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
});
await wait(500);
check("…the picker lists a daemon to choose", () => {
  const modal = document.querySelector(".modal-back.open");
  assert.ok(modal, "a daemon picker is open for the Datacenter create");
  const pick = modal.querySelector("[data-pick-conn]");
  assert.ok(pick, "the picker lists a daemon");
  pick.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
});
await wait(700);
check("…and picking one leaves the Datacenter on screen, unselected", () => {
  // The bug: picking a daemon made the app select it, so the sidebar gained a
  // selection and the pane switched to that server's page.
  const sel = document.querySelector(".tree-row.sel");
  assert.equal(sel?.id, "dc-row", `the Datacenter is still the selected row (got ${sel?.className})`);
  const tabs = [...document.querySelectorAll(".tabbar .tab")].map((t) => t.textContent.trim());
  assert.ok(tabs.includes("Guests"), `still the Datacenter's tabs (${tabs})`);
});

// The Dashboard tab is a separate view; the checks below read #dc-dash, which
// only exists while that tab is the current one. The tab-walk above left the
// Datacenter on Storage.
[...document.querySelectorAll(".tabbar .tab")]
  .find((t) => t.textContent.trim() === "Dashboard")
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1200);

check("the datacenter dashboard summarises every server", () => {
  const body = document.querySelector("#dc-dash")?.textContent ?? "";
  assert.match(body, /Total CPU/, "aggregate CPU metric");
  assert.match(body, /Total Memory/, "aggregate memory metric");
  assert.match(body, /Total Disk/, "aggregate disk metric");
  assert.match(body, /SERVERS/, "server count card");
  assert.match(body, /NETWORKS/, "network count card");
  assert.match(body, /Networking/, "networking summary");
  assert.match(body, /Storage Pools/, "storage pool summary");
  assert.ok(document.querySelector("[data-dc-dash-conn]"), "per-server row links to its dashboard");
});

// The dashboard is a long page users leave scrolled down. A poll that found
// the tree changed used to rebuild the whole pane (skeleton included), which
// flashed and clamped the scroll position back to the top. Repainting #dc-dash
// in place is the only thing a poll may do here.
{
  const head = document.querySelector("#content > .dhead");
  const scroller = document.querySelector("#content");
  scroller.scrollTop = 240;
  nudgeTree = true;
  await wait(1600);
  check("a tree change during a poll repaints the dashboard in place", () => {
    assert.ok(document.querySelector("#dc-dash"), "dashboard still rendered");
    assert.equal(document.querySelector("#content > .dhead"), head,
      "the pane was not rebuilt — a rebuild replaces it with the skeleton and clamps the scroll");
    assert.equal(document.querySelectorAll("#content .skeleton").length, 0,
      "no skeleton painted over the open dashboard");
    assert.equal(scroller.scrollTop, 240, "the scroll position survived the poll");
  });
}

// A network is named by its UUID and keeps its nickname as a `title`, so the
// dashboard's Networking table showed a UUID for anything made or migrated
// since. The row must read as the nickname, while `data-dash-net` keeps the
// libvirt name the action needs.
const conns = await (await fetch(`${base}/connections`)).json();
const allNets = [];
for (const c of conns) {
  const nets = await (await fetch(`${base}/proxy/${c.id}/networks`)).json().catch(() => []);
  for (const n of nets) allNets.push(n);
}
check("the dashboard network rows show the nickname, not the UUID", () => {
  const rows = [...document.querySelectorAll("#dc-dash [data-dash-net]")];
  if (!allNets.length) {
    assert.equal(rows.length, 0, "no networks, so no rows to check");
    return;
  }
  for (const row of rows) {
    const name = row.dataset.dashNet;
    const net = allNets.find((n) => n.name === name);
    if (!net) continue;
    const shown = row.querySelector("td")?.textContent.trim();
    assert.equal(shown, net.title || net.name,
      `network ${name} shows its nickname (${shown})`);
    assert.ok(!/^[0-9a-f-]{36}$/i.test(shown),
      `network ${name} is not shown as a bare UUID (${shown})`);
  }
});

[...document.querySelectorAll(".tabbar .tab")]
  .find((t) => t.textContent.trim() === "Guests")
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(900);

check("Guests tab lists every guest", () => {
  const active = document.querySelector(".tabbar .tab.active")?.textContent.trim();
  assert.equal(active, "Guests", "Guests tab is active");
  const cards = [...document.querySelectorAll("[data-dc-vm]")];
  assert.ok(cards.length > 0, `${cards.length} guest card(s) listed`);
  results.push(["info", `guests listed: ${cards.map((r) => r.querySelector(".storage-card-title")?.textContent.trim()).join(", ")}`]);
});

check("each guest card shows its server and state", () => {
  const card = document.querySelector("[data-dc-vm]");
  const badges = [...card.querySelectorAll(".badge")].map((b) => b.textContent.trim());
  assert.ok(badges.some((b) => /Active|Inactive/.test(b)), `state badge (${badges})`);
  assert.ok(badges.length >= 2, `server badge present (${badges})`);
});

check("guest cards carry quick actions", () => {
  const card = document.querySelector("[data-dc-vm]");
  for (const sel of ["[data-dc-toggle]", "[data-dc-console]", "[data-dc-hwedit]"]) {
    assert.ok(card.querySelector(sel), `${sel} present`);
  }
});

// Clicking a guest card must open that VM.
const name = document.querySelector("[data-dc-vm]").dataset.dcVm;
const cardTitle = document.querySelector("[data-dc-vm] .storage-card-title").textContent.trim();
document.querySelector("[data-dc-vm]")
  .dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1600);

check("clicking a guest in the Guests tab opens its VM view", () => {
  const title = document.querySelector(".dtitle")?.textContent.trim();
  assert.equal(title, cardTitle, `VM view for ${name} is open (title: ${title})`);
  const tabs = [...document.querySelectorAll(".tabbar .tab")].map((t) => t.textContent.trim());
  assert.deepEqual(tabs, ["Summary", "Console", "Hardware", "Options", "Network", "XML"], "VM tabs render");
  assert.ok(document.querySelector(".tree-row.tree-vm.sel"), "sidebar marks the guest selected");
});

// Rename lives in the Options tab as a field, not as a toolbar button.
[...document.querySelectorAll(".tabbar .tab")]
  .find((t) => t.textContent.trim() === "Options")
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1200);

check("Options has a nickname, not a resource name or toolbar rename", () => {
  assert.ok(!document.querySelector("#vm-rename"), "no rename button in the toolbar");
  assert.ok(!document.querySelector("#opt-name"), "no resource name field");
  assert.ok(document.querySelector("#opt-title"), "Options tab has a Nickname field");
});

// Back to Datacenter > Guests, then straight into a console.
document.querySelector("#dc-row").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1000);
[...document.querySelectorAll(".tabbar .tab")]
  .find((t) => t.textContent.trim() === "Guests")
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(800);
document.querySelector("[data-dc-console]")
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1600);

check("console action jumps straight to the console tab", () => {
  const active = document.querySelector(".tabbar .tab.active")?.textContent.trim();
  assert.equal(active, "Console", `console tab active (got ${active})`);
});

// Server rows on the dashboard open the server.
document.querySelector("#dc-row").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(2500);
check("server rows are selectable", () => {
  const card = document.querySelector("[data-dc-dash-conn]");
  assert.ok(card, "server row is clickable");
  card.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
});
await wait(1400);
check("clicking a server opens its view", () => {
  const tabs = [...document.querySelectorAll(".tabbar .tab")].map((t) => t.textContent.trim());
  assert.deepEqual(tabs, ["Dashboard", "VMs", "Containers", "Networks", "Storage", "Firewall", "Shell"], `server tabs render (${tabs})`);
});

for (const [state, name2] of results) {
  console.log(`${state === "pass" ? "✔" : state === "info" ? "·" : "✖"} ${name2}`);
}
await removeTestVm(base, fixture);
await removeTestPool(base, testPool);
const failed = results.filter(([s]) => s === "FAIL");
// Exit before closing the window: pending app timers still reference it.
if (failed.length) { console.error(`\n${failed.length} check(s) failed`); process.exit(1); }
console.log("\nall datacenter navigation checks passed");
// Opening the console tab starts a noVNC WebSocket that jsdom cannot finish,
// which keeps the event loop alive; exit explicitly once assertions are done.
process.exit(0);
