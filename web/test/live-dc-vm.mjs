// A VM must be selectable directly from the Datacenter view.
//
// Usage: node test/live-dc-vm.mjs [baseUrl]

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";

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
window.fetch = (input, init) => {
  const url = typeof input === "string" ? new URL(input, base).toString() : input;
  return fetch(url, init);
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
  guest.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(1600);

  check("clicking a sidebar guest from Datacenter opens that VM", () => {
    const title = document.querySelector(".dtitle")?.textContent.trim();
    assert.equal(title, name, `VM view opened for ${name} (title: ${title})`);
    const tabs = [...document.querySelectorAll(".tabbar .tab")].map((t) => t.textContent.trim());
    assert.deepEqual(tabs, ["Summary", "Hardware", "Options", "Firewall", "Console", "XML"], "VM tabs render");
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

check("Datacenter has a dedicated Guests tab", () => {
  const tabs = [...document.querySelectorAll(".tabbar .tab")].map((t) => t.textContent.trim());
  assert.ok(tabs.includes("Servers"), `Servers tab (${tabs})`);
  assert.ok(tabs.includes("Guests"), `Guests tab (${tabs})`);
  // The servers tab must no longer carry the guests table.
  assert.equal(document.querySelectorAll("[data-open-vm]").length, 0,
    "guests are not listed on the Servers tab");
});

[...document.querySelectorAll(".tabbar .tab")]
  .find((t) => t.textContent.trim() === "Guests")
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(900);

check("Guests tab lists every guest", () => {
  const active = document.querySelector(".tabbar .tab.active")?.textContent.trim();
  assert.equal(active, "Guests", "Guests tab is active");
  const rows = [...document.querySelectorAll("[data-open-vm]")];
  assert.ok(rows.length > 0, `${rows.length} guest row(s) listed`);
  results.push(["info", `guests listed: ${rows.map((r) => r.textContent.trim()).join(", ")}`]);
});

check("each guest row shows its server and state", () => {
  const row = document.querySelector("[data-open-vm]").closest("tr");
  const cells = [...row.querySelectorAll("td")].map((c) => c.textContent.trim());
  assert.ok(cells[1], `server column populated (${cells[1]})`);
  assert.ok(row.querySelector(".state"), "state badge present");
});

check("guest rows carry quick actions", () => {
  const row = document.querySelector("[data-open-vm]").closest("tr");
  for (const sel of ["[data-dc-start]", "[data-dc-stop]", "[data-dc-console]", "[data-dc-hwedit]"]) {
    assert.ok(row.querySelector(sel), `${sel} present`);
  }
});

// Clicking a guest name in the table must open that VM.
const name = document.querySelector("[data-open-vm]").dataset.openVm;
document.querySelector("[data-open-vm]")
  .dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1600);

check("clicking a guest in the Guests tab opens its VM view", () => {
  const title = document.querySelector(".dtitle")?.textContent.trim();
  assert.equal(title, name, `VM view for ${name} is open (title: ${title})`);
  const tabs = [...document.querySelectorAll(".tabbar .tab")].map((t) => t.textContent.trim());
  assert.deepEqual(tabs, ["Summary", "Hardware", "Options", "Firewall", "Console", "XML"], "VM tabs render");
  assert.ok(document.querySelector(".tree-row.tree-vm.sel"), "sidebar marks the guest selected");
});

// Rename lives in the Options tab as a field, not as a toolbar button.
[...document.querySelectorAll(".tabbar .tab")]
  .find((t) => t.textContent.trim() === "Options")
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1200);

check("rename is an editable Options field, not a toolbar button", () => {
  assert.ok(!document.querySelector("#vm-rename"), "no rename button in the toolbar");
  const name = document.querySelector("#opt-name");
  assert.ok(name, "Options tab has a Name field");
  assert.ok(!name.disabled, "Name field is editable");
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

// Server rows should still be clickable too (Servers tab).
document.querySelector("#dc-row").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1200);
check("server rows are selectable from the table", () => {
  const cell = document.querySelector("[data-open-conn-row]");
  assert.ok(cell, "server name is clickable");
  cell.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
});
await wait(1400);
check("clicking a server opens its view", () => {
  const tabs = [...document.querySelectorAll(".tabbar .tab")].map((t) => t.textContent.trim());
  assert.deepEqual(tabs, ["VMs", "Networks", "Storage", "Firewall"], `server tabs render (${tabs})`);
});

for (const [state, name2] of results) {
  console.log(`${state === "pass" ? "✔" : state === "info" ? "·" : "✖"} ${name2}`);
}
const failed = results.filter(([s]) => s === "FAIL");
// Exit before closing the window: pending app timers still reference it.
if (failed.length) { console.error(`\n${failed.length} check(s) failed`); process.exit(1); }
console.log("\nall datacenter navigation checks passed");
// Opening the console tab starts a noVNC WebSocket that jsdom cannot finish,
// which keeps the event loop alive; exit explicitly once assertions are done.
process.exit(0);
