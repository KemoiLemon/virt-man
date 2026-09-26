// XML icon, Options tab, and the two Firewall tabs (daemon + VM).
//
// Usage: node test/live-tabs.mjs [baseUrl]

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";

const base = process.argv[2] ?? "http://127.0.0.1:8811";

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
const tabNamed = (n) => [...document.querySelectorAll(".tabbar .tab")]
  .find((t) => t.textContent.trim() === n);
await wait(2800);

const results = [];
const check = (name, fn) => {
  try { fn(); results.push(["pass", name]); }
  catch (e) { results.push(["FAIL", `${name} — ${e.message}`]); }
};

// ---- daemon-level Firewall tab --------------------------------------------
document.querySelector(".tree-row.tree-group")
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1500);

check("server view has a Dashboard tab first, then the resource tabs", () => {
  const tabs = [...document.querySelectorAll(".tabbar .tab")].map((t) => t.textContent.trim());
  assert.deepEqual(tabs, ["Dashboard", "VMs", "Networks", "Storage", "Firewall"], `tabs: ${tabs}`);
  assert.ok(tabNamed("Dashboard").querySelector("svg"), "Dashboard tab has an icon");
  assert.ok(tabNamed("Firewall").querySelector("svg"), "Firewall tab has an icon");
});

await wait(1500); // first dashboard render fetches host stats + lists
check("the dashboard is the landing tab and shows host + resource summaries", () => {
  const active = document.querySelector(".tabbar .tab.active")?.textContent.trim();
  assert.equal(active, "Dashboard", "Dashboard tab active on open");
  const body = document.querySelector("#conn-body")?.textContent ?? "";
  assert.match(body, /CPU/, "CPU metric rendered");
  assert.match(body, /Memory/, "Memory metric rendered");
  assert.match(body, /VMS/, "VM count card rendered");
  assert.match(body, /NETWORKS/, "network count card rendered");
  assert.match(body, /POOLS/, "pool count card rendered");
  assert.match(body, /Networking/, "networking summary rendered");
  assert.match(body, /Storage pools/, "storage pool summary rendered");
});

tabNamed("Firewall")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1800);

check("daemon Firewall tab lists host nwfilters", () => {
  const active = document.querySelector(".tabbar .tab.active")?.textContent.trim();
  assert.equal(active, "Firewall", "Firewall tab active");
  const rows = [...document.querySelectorAll("[data-fxml]")];
  assert.ok(rows.length > 5, `${rows.length} filters listed (host defines ~24)`);
  results.push(["info", `host filters listed: ${rows.length}`]);
});

check("each filter row can show its XML", () => {
  assert.ok(document.querySelector("[data-fxml]"), "XML action present");
});

document.querySelector("[data-fxml]")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1200);
check("filter XML opens in a themed modal", () => {
  const modal = document.querySelector(".modal-back.open");
  assert.ok(modal, "modal opened");
  const body = modal.querySelector(".cmd")?.textContent ?? "";
  assert.match(body, /<filter/, `shows filter XML (got: ${body.slice(0, 60)})`);
  assert.ok(modal.querySelector("#fxml-close"), "close button is wired in JS, not inline");
  modal.remove();
});

// ---- VM tabs ---------------------------------------------------------------
document.querySelector(".tree-row.tree-vm")
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1600);

check("VM view exposes Options and Firewall tabs, and XML has an icon", () => {
  const tabs = [...document.querySelectorAll(".tabbar .tab")].map((t) => t.textContent.trim());
  assert.deepEqual(tabs, ["Summary", "Hardware", "Options", "Firewall", "Console", "XML"],
    `tabs: ${tabs}`);
  assert.ok(tabNamed("XML").querySelector("svg"), "XML tab now carries an icon");
  assert.ok(tabNamed("Options").querySelector("svg"), "Options tab has an icon");
  assert.ok(tabNamed("Firewall").querySelector("svg"), "Firewall tab has an icon");
});

tabNamed("Options")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1600);

check("Options tab shows editable metadata read from the guest", () => {
  assert.equal(document.querySelector(".tabbar .tab.active")?.textContent.trim(), "Options");
  const name = document.querySelector("#opt-name");
  assert.ok(name, "name field present");
  assert.ok(name.value, `name populated (${name.value})`);
  assert.ok(!name.disabled, "name is editable — rename lives in the Options form");
  assert.ok(document.querySelector("#opt-title"), "title field");
  assert.ok(document.querySelector("#opt-desc"), "description field");
  assert.ok(document.querySelector("#opt-boot"), "boot device selector");
  assert.ok(document.querySelector("#opt-save"), "save button");
  // Lifecycle facts must come from the real domain, not placeholders.
  const specs = [...document.querySelectorAll(".spec b")].map((b) => b.textContent.trim());
  for (const want of ["On poweroff", "On reboot", "Machine", "Architecture"]) {
    assert.ok(specs.includes(want), `${want} shown (${specs})`);
  }
  const machine = [...document.querySelectorAll(".spec")]
    .find((s) => s.querySelector("b")?.textContent.trim() === "Machine")
    ?.querySelector("span")?.textContent.trim();
  assert.match(machine, /pc-|q35/, `real machine type (${machine})`);
});

tabNamed("Firewall")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1800);

check("VM Firewall tab offers host filters and lists interfaces", () => {
  assert.equal(document.querySelector(".tabbar .tab.active")?.textContent.trim(), "Firewall");
  const sel = document.querySelector("#fw-filter");
  assert.ok(sel, "filter selector present");
  assert.ok(sel.options.length > 5, `${sel.options.length} options (none + host filters)`);
  assert.equal(sel.options[0].value, "", "first option clears the filter");
  assert.ok([...sel.options].some((o) => o.value === "clean-traffic"),
    "real host filters offered");
  assert.ok(document.querySelector("#fw-apply"), "apply button");

  const rows = [...document.querySelectorAll("table.grid tr")].slice(1);
  assert.ok(rows.length >= 1, `${rows.length} interface row(s)`);
  const mac = rows[0].querySelector("td")?.textContent.trim();
  assert.match(mac, /^([0-9a-f]{2}:){5}[0-9a-f]{2}$/i, `real MAC shown (${mac})`);
  results.push(["info", `interface: ${mac}`]);
});

check("the themed listbox replaced the native filter select", () => {
  const sel = document.querySelector("#fw-filter");
  assert.equal(window.getComputedStyle(sel).display, "none", "native select hidden");
  assert.ok(sel.nextElementSibling?.classList.contains("select-trigger"), "themed trigger");
});

for (const [state, name] of results) {
  console.log(`${state === "pass" ? "✔" : state === "info" ? "·" : "✖"} ${name}`);
}
const failed = results.filter(([s]) => s === "FAIL");
window.close();
if (failed.length) { console.error(`\n${failed.length} check(s) failed`); process.exit(1); }
console.log("\nall tab checks passed");
process.exit(0);
