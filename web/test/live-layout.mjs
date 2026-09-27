// Layout geometry checks: sidebar indentation, topbar grouping, tab-bar
// action alignment and spacing under the tabs.
//
// jsdom does no real layout, so the DOM assertions here are structural and
// the *numeric* ones come from computed styles that jsdom does resolve
// (padding), not from box positions.
//
// Usage: node test/live-layout.mjs [baseUrl]

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";
import { ensureTestVm, removeTestVm } from "./_fixture.mjs";

const base = process.argv[2] ?? "http://127.0.0.1:8803";

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
// Live suites own their fixture: reuse a guest if the host has one,
// otherwise define a disposable one so the run never depends on
// whatever happens to be on this machine.
const fixture = await ensureTestVm(base);
window.eval(code);
const wait = (ms) => new Promise((r) => window.setTimeout(r, ms));
await wait(2500);

const results = [];
const check = (name, fn) => {
  try { fn(); results.push(["pass", name]); }
  catch (e) { results.push(["FAIL", `${name} — ${e.message}`]); }
};
const px = (el, prop) => parseFloat(window.getComputedStyle(el)[prop]) || 0;

check("VMs are indented under their daemon", () => {
  const root = document.querySelector(".tree-row.tree-root");
  const group = document.querySelector(".tree-row.tree-group");
  const vm = document.querySelector(".tree-row.tree-vm");
  assert.ok(root && group && vm, "all three tree levels render");
  const [r, g, v] = [root, group, vm].map((e) => px(e, "paddingLeft"));
  assert.ok(g > r, `server (${g}px) indented past root (${r}px)`);
  assert.ok(v > g, `VM (${v}px) indented past its server (${g}px)`);
  results.push(["info", `indent: root ${r}px -> server ${g}px -> vm ${v}px`]);
});

check("window controls sit flush at the far right", () => {
  const bar = document.querySelector(".topbar");
  const controls = document.querySelector(".window-controls");
  // Browser build has no window controls; only assert when present.
  if (!controls) { results.push(["info", "browser build: no window controls"]); return; }
  assert.equal(px(bar, "paddingRight"), 0, "topbar has no right padding");
  assert.equal(controls.parentElement, bar, "controls are a direct topbar child");
  assert.equal(bar.lastElementChild, controls, "controls are the last element");
});

check("the topbar action group hugs the window controls", () => {
  const bar = document.querySelector(".topbar");
  const quick = document.querySelector(".topbar-quick");
  assert.ok(quick, ".topbar-quick group exists");
  assert.ok(quick.querySelector("#theme-btn"), "theme button is in the quick group");
  // Connect-to-WebUI is desktop-only now; the browser build must not show it.
  assert.ok(!quick.querySelector("#webui-btn"), "no WebUI button in the browser build");
  const gap = px(quick, "gap");
  assert.ok(gap > 0 && gap <= 8, `buttons sit close together (gap ${gap}px)`);
  // The quick group is the last element before the window controls.
  const controls = document.querySelector(".window-controls");
  if (controls) {
    assert.equal(controls.previousElementSibling, quick, "quick group sits directly before the window controls");
    assert.equal(bar.lastElementChild, controls, "controls remain the last element");
  }
});

// The dashboard is the landing tab; these layout checks target the Servers
// tab (Add Server action + the server table's action column).
[...document.querySelectorAll(".tabbar .tab")]
  .find((t) => t.textContent.trim() === "Servers")
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(800);

check("tab-bar actions align with the content edge", () => {
  const actions = document.querySelector(".tabbar-actions");
  assert.ok(actions, "tab bar carries actions");
  assert.equal(px(actions, "paddingRight"), 0,
    "no extra right padding, so buttons line up with the cards/table below");
  assert.equal(window.getComputedStyle(actions).marginLeft, "auto", "pushed to the right");
});

check("tabs have clearance above the content", () => {
  const bar = document.querySelector(".tabbar");
  assert.ok(px(bar, "marginBottom") >= 14,
    `tab bar reserves space below it (${px(bar, "marginBottom")}px)`);
});

check("datacenter servers render as cards with icon action buttons", () => {
  const cards = [...document.querySelectorAll("#content .card-grid .storage-card")];
  assert.ok(cards.length > 0, `${cards.length} server card(s)`);
  const first = cards[0];
  const actions = first.querySelector(".net-actions");
  assert.ok(actions, "card actions share a row");
  const buttons = [...actions.querySelectorAll(".btn")];
  assert.ok(buttons.every((b) => b.classList.contains("icon")), "all server actions are icon buttons");
  assert.ok(first.querySelector("[data-edit]"), "edit action present");
  assert.ok(first.querySelector("[data-del]"), "delete action present");
});

// VM list: actions must mirror the datacenter table and gain edit + console.
document.querySelector(".tree-row.tree-group")
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1500);
// The server opens on the Dashboard tab; the VM table lives on the VMs tab.
[...document.querySelectorAll(".tabbar .tab")]
  .find((t) => t.textContent.trim() === "VMs")
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(800);

check("VM cards mirror the server cards' action layout", () => {
  const card = document.querySelector("#vms-list [data-vm-open]");
  assert.ok(card, "VM cards use the same card style as servers");
  const actions = card.querySelector(".net-actions");
  assert.ok(actions, "VM buttons share a row");
  const buttons = [...actions.querySelectorAll(".btn")];
  assert.ok(buttons.every((b) => b.classList.contains("icon")), "all VM actions are icon buttons");
});

check("VM quick actions include console, edit and a merged start/stop toggle", () => {
  const card = document.querySelector("#vms-list [data-vm-open]");
  assert.ok(card.querySelector("[data-vm-toggle]"), "merged start/stop toggle present");
  assert.ok(card.querySelector("[data-vm-console]"), "console button present");
  assert.ok(card.querySelector("[data-vm-edit]"), "edit button present");
  assert.ok(card.querySelector("[data-vm-del]"), "delete present");
  results.push(["info",
    `VM quick actions: ${[...card.querySelectorAll(".btn")].map((b) => b.title).join(", ")}`]);
});

check("the start/stop toggle highlights the running state", () => {
  const card = document.querySelector("#vms-list [data-vm-open]");
  const running = card.querySelector(".badge.on")?.textContent.trim() === "Active";
  const toggle = card.querySelector("[data-vm-toggle]");
  assert.equal(toggle.classList.contains("on"), running,
    "toggle highlights only while the guest is active");
});

// VM detail view.
document.querySelector(".tree-row.tree-vm")
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1500);
// The guest that opened, and its state as libvirt reports it -- not as the
// page's guest list does: the header once painted from a list that had not
// loaded yet, and a check reading the same list agreed with it.
const vmName = document.querySelector(".tree-row.tree-vm.sel")?.dataset.vm ?? fixture.name;
const vmConn = document.querySelector(".tree-row.tree-vm.sel")?.dataset.vmConn ?? fixture.conn;
const fixtureRunning = (await (await fetch(`${base}/proxy/${vmConn}/domains/${encodeURIComponent(vmName)}`)).json()).active;

check("VM detail actions live on the tab bar as icon buttons", () => {
  const actions = document.querySelector(".tabbar .tabbar-actions");
  assert.ok(actions, "VM tab bar carries the actions");
  for (const id of ["#vm-start", "#vm-shutdown", "#vm-reboot", "#vm-reset", "#vm-destroy", "#vm-delete"]) {
    assert.ok(actions.querySelector(id), `${id} present`);
  }
  assert.equal(document.querySelectorAll(".dhead + .actions").length, 0,
    "no separate action strip under the title");
});

check("VM actions enable by state: stopped greys runtime buttons, running greys start", () => {
  const actions = document.querySelector(".tabbar .tabbar-actions");
  const start = actions.querySelector("#vm-start");
  const shutdown = actions.querySelector("#vm-shutdown");
  const reboot = actions.querySelector("#vm-reboot");
  const reset = actions.querySelector("#vm-reset");
  const destroy = actions.querySelector("#vm-destroy");
  const del = actions.querySelector("#vm-delete");
  // The fixture reuses a guest the host already has, which may be running.
  const running = fixtureRunning;
  const state = running ? "running" : "stopped";
  assert.equal(start.disabled, running, `start ${running ? "disabled" : "enabled"} while ${state}`);
  assert.ok(!del.disabled, "delete always enabled");
  for (const [b, name] of [[shutdown, "shutdown"], [reboot, "reboot"], [reset, "reset"], [destroy, "destroy"]]) {
    assert.equal(b.disabled, !running, `${name} ${running ? "enabled" : "disabled"} while ${state}`);
  }
});

// A reload lands straight on the VM's URL: the header is painted before the
// guest list arrives, so it must correct itself once the list is in.
{
  const conns = await (await fetch(`${base}/connections`)).json();
  const cname = conns.find((c) => c.id === vmConn)?.name ?? vmConn;
  const fresh = new JSDOM(`<!doctype html><html><body><div id="app"></div></body></html>`, {
    url: `${base}/${encodeURIComponent(cname)}/vms/${encodeURIComponent(vmName)}`,
    runScripts: "dangerously", pretendToBeVisual: true, virtualConsole,
  });
  fresh.window.fetch = window.fetch;
  fresh.window.ResizeObserver ??= class { observe() {} unobserve() {} disconnect() {} };
  fresh.window.eval(code);
  await new Promise((r) => setTimeout(r, 2500));
  check(`a reload shows the actions for the guest's real state (${fixtureRunning ? "running" : "stopped"})`, () => {
    const doc = fresh.window.document;
    assert.equal(doc.getElementById("vm-start")?.disabled, fixtureRunning, "start");
    assert.equal(doc.getElementById("vm-shutdown")?.disabled, !fixtureRunning, "shutdown");
    assert.equal(doc.getElementById("vm-destroy")?.disabled, !fixtureRunning, "force stop");
  });
  fresh.window.close();
}

for (const [state, name] of results) {
  console.log(`${state === "pass" ? "✔" : state === "info" ? "·" : "✖"} ${name}`);
}
await removeTestVm(base, fixture);
const failed = results.filter(([s]) => s === "FAIL");
window.close();
if (failed.length) { console.error(`\n${failed.length} check(s) failed`); process.exit(1); }
console.log("\nall layout checks passed");
