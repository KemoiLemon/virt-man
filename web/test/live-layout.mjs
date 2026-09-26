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

check("datacenter table uses the compact action column", () => {
  const td = document.querySelector("table.grid td.col-actions");
  assert.ok(td, "action column present");
  assert.equal(window.getComputedStyle(td).whiteSpace, "nowrap", "column does not wrap");
  assert.ok(td.querySelector(".row-actions"), "buttons share a flex row");
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

check("VM table matches the datacenter table's action layout", () => {
  const td = document.querySelector("#vms-list table.grid td.col-actions");
  assert.ok(td, "VM rows use .col-actions like the datacenter table");
  const row = td.querySelector(".row-actions");
  assert.ok(row, "VM buttons share the same .row-actions wrapper");
  const buttons = [...row.querySelectorAll(".btn")];
  assert.ok(buttons.every((b) => b.classList.contains("icon")),
    "all VM actions are icon buttons, like the datacenter row");
});

check("VM quick actions include console and edit", () => {
  const row = document.querySelector("#vms-list .row-actions");
  assert.ok(row.querySelector("[data-console]"), "console button present");
  assert.ok(row.querySelector("[data-hwedit]"), "edit button present");
  assert.ok(row.querySelector("[data-start]"), "start retained");
  assert.ok(row.querySelector("[data-stop]"), "stop retained");
  assert.ok(row.querySelector("[data-delvm]"), "delete present");
  results.push(["info",
    `VM quick actions: ${[...row.querySelectorAll(".btn")].map((b) => b.title).join(", ")}`]);
});

check("start/stop reflect the guest's current state", () => {
  const row = document.querySelector("#vms-list tr:nth-child(2)");
  const running = /running/.test(row?.querySelector(".state")?.textContent ?? "");
  const start = row.querySelector("[data-start]");
  const stop = row.querySelector("[data-stop]");
  assert.equal(start.disabled, running, "start disabled only while running");
  assert.equal(stop.disabled, !running, "stop disabled only while stopped");
});

// VM detail view.
document.querySelector(".tree-row.tree-vm")
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1500);

check("VM detail actions live on the tab bar as icon buttons", () => {
  const actions = document.querySelector(".tabbar .tabbar-actions");
  assert.ok(actions, "VM tab bar carries the actions");
  for (const id of ["#vm-start", "#vm-shutdown", "#vm-reboot", "#vm-destroy", "#vm-hwedit", "#vm-delete"]) {
    assert.ok(actions.querySelector(id), `${id} present`);
  }
  assert.equal(document.querySelectorAll(".dhead + .actions").length, 0,
    "no separate action strip under the title");
});

for (const [state, name] of results) {
  console.log(`${state === "pass" ? "✔" : state === "info" ? "·" : "✖"} ${name}`);
}
const failed = results.filter(([s]) => s === "FAIL");
window.close();
if (failed.length) { console.error(`\n${failed.length} check(s) failed`); process.exit(1); }
console.log("\nall layout checks passed");
