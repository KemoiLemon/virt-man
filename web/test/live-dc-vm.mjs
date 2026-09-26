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
window.eval(code);
const wait = (ms) => new Promise((r) => window.setTimeout(r, ms));
await wait(2800);

const results = [];
const check = (name, fn) => {
  try { fn(); results.push(["pass", name]); }
  catch (e) { results.push(["FAIL", `${name} — ${e.message}`]); }
};

check("Datacenter lists guests, not just servers", () => {
  const titles = [...document.querySelectorAll(".section-title")].map((s) => s.textContent.trim());
  assert.ok(titles.includes("Servers"), `Servers section (${titles})`);
  assert.ok(titles.includes("Guests"), `Guests section (${titles})`);
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

// The actual bug: clicking a guest name must open that VM.
const name = document.querySelector("[data-open-vm]").dataset.openVm;
document.querySelector("[data-open-vm]")
  .dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1600);

check("clicking a guest opens its VM view", () => {
  const title = document.querySelector(".dtitle")?.textContent.trim();
  assert.equal(title, name, `VM view for ${name} is open (title: ${title})`);
  const tabs = [...document.querySelectorAll(".tabbar .tab")].map((t) => t.textContent.trim());
  assert.deepEqual(tabs, ["Summary", "Hardware", "Console", "XML"], "VM tabs render");
  assert.ok(document.querySelector(".tree-row.tree-vm.sel"), "sidebar marks the guest selected");
});

// Back to Datacenter, then straight into a console.
document.querySelector("#dc-row").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1200);
document.querySelector("[data-dc-console]")
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1600);

check("console action jumps straight to the console tab", () => {
  const active = document.querySelector(".tabbar .tab.active")?.textContent.trim();
  assert.equal(active, "Console", `console tab active (got ${active})`);
});

// Server rows should still be clickable too.
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
  assert.deepEqual(tabs, ["VMs", "Networks", "Storage"], `server tabs render (${tabs})`);
});

for (const [state, name2] of results) {
  console.log(`${state === "pass" ? "✔" : state === "info" ? "·" : "✖"} ${name2}`);
}
const failed = results.filter(([s]) => s === "FAIL");
window.close();
if (failed.length) { console.error(`\n${failed.length} check(s) failed`); process.exit(1); }
console.log("\nall datacenter navigation checks passed");
// Opening the console tab starts a noVNC WebSocket that jsdom cannot finish,
// which keeps the event loop alive; exit explicitly once assertions are done.
process.exit(0);
