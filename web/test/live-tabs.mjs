// XML icon, Options tab, and the two Firewall tabs (daemon + VM).
//
// Usage: node test/live-tabs.mjs [baseUrl]

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";
import { ensureTestVm, removeTestVm } from "./_fixture.mjs";

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
// Live suites own their fixture: reuse a guest if the host has one,
// otherwise define a disposable one so the run never depends on
// whatever happens to be on this machine.
const fixture = await ensureTestVm(base);
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
  assert.match(body, /Storage Pools/, "storage pool summary rendered");
});

tabNamed("Networks")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1500);

check("Networks tab renders cards with type, subnet and merged controls", () => {
  const active = document.querySelector(".tabbar .tab.active")?.textContent.trim();
  assert.equal(active, "Networks", "Networks tab active");
  const cards = [...document.querySelectorAll("#nets-list .net-card")];
  assert.ok(cards.length > 0, `${cards.length} network card(s)`);
  const first = cards[0];
  const badges = [...first.querySelectorAll(".storage-card-head .badge")].map((b) => b.textContent.trim());
  assert.ok(badges.some((b) => ["NAT", "Bridge", "Isolated"].includes(b)), `type badge (${badges})`);
  // Bridge stacked above subnet.
  const specLabels = [...first.querySelectorAll(".spec b")].map((b) => b.textContent.trim());
  assert.ok(specLabels.indexOf("Bridge") < specLabels.indexOf("Subnet"),
    `Bridge above Subnet (${specLabels})`);
  // Merged start/stop toggle: icon-only, no text.
  const toggle = first.querySelector("[data-net-toggle]");
  assert.ok(toggle, "merged start/stop toggle");
  assert.equal(first.querySelectorAll("[data-net-toggle]").length, 1, "exactly one toggle");
  assert.ok(toggle.querySelector("svg"), "toggle has an icon");
  assert.equal(toggle.textContent.trim(), "", "toggle is icon-only");
  const auto = first.querySelector("[data-net-autostart]");
  assert.ok(auto, "autostart button");
  assert.ok(auto.querySelector("svg"), "autostart has an icon");
  assert.ok(first.querySelector("[data-net-edit]"), "edit button");
  assert.ok(first.querySelector("[data-delnet]"), "delete button");
});

check("the create-network card is present with a plus icon", () => {
  const create = document.querySelector("#net-create-card");
  assert.ok(create, "create card present");
  assert.ok(create.querySelector("svg"), "plus icon rendered");
  assert.match(create.textContent, /Create Network/);
});

check("start/stop and autostart highlights follow their state", () => {
  for (const c of document.querySelectorAll("#nets-list .net-card:not(.net-create-card)")) {
    const toggle = c.querySelector("[data-net-toggle]");
    const cardActive = c.querySelector(".storage-card-head .badge.on")?.textContent.trim() === "Active";
    assert.equal(toggle.classList.contains("on"), cardActive,
      `toggle highlight matches the active badge`);
    const auto = c.querySelector("[data-net-autostart]");
    assert.equal(
      auto.classList.contains("on"),
      /Autostart on/.test(auto.title),
      `autostart highlight matches its title (${auto.title})`,
    );
  }
});

tabNamed("Firewall")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1800);

check("daemon Firewall tab lists networks with a rules action", () => {
  const active = document.querySelector(".tabbar .tab.active")?.textContent.trim();
  assert.equal(active, "Firewall", "Firewall tab active");
  const rows = [...document.querySelectorAll("[data-fwedit]")];
  assert.ok(rows.length > 0, `${rows.length} network(s) listed`);
  assert.ok(document.querySelector("[data-fwedit]"), "Rules action present per network");
});

// Open the first network's firewall editor.
document.querySelector("[data-fwedit]")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1200);
check("firewall editor opens with rules and port-forward tables", () => {
  const modal = document.querySelector(".modal-back.open");
  assert.ok(modal, "modal opened");
  assert.ok(modal.querySelector("#fw-add-rule"), "add-rule action");
  assert.ok(modal.querySelector("#fw-add-fwd"), "add-forward action");
  assert.ok(modal.querySelector("#fw-save"), "apply action");
  assert.match(modal.textContent ?? "", /NAT Port Forwards/);
  modal.remove();
});

// The source/dest IP fields suggest what the daemon actually knows about.
{
  // Add a rule so there is a row to type into.
  document.querySelector("[data-fwedit]")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(1200);
  const modal = document.querySelector(".modal-back.open");
  modal?.querySelector("#fw-add-rule")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(500);
  const src = modal?.querySelector("[data-r-src]");
  check("firewall source IP field offers addresses to pick from", () => {
    assert.ok(src, "source IP input rendered");
    src.dispatchEvent(new window.Event("focus"));
    const options = [...document.querySelectorAll(".select-popover .select-option")]
      .map((b) => b.textContent);
    assert.ok(options.length > 0, `suggestions offered (${options})`);
    // They must be real addresses from this daemon's networks, not placeholders.
    assert.ok(
      options.every((o) => /^\d{1,3}(\.\d{1,3}){3}$/.test(o)),
      `each suggestion is an IPv4 address (${options})`,
    );
    results.push(["info", `IP suggestions: ${options.join(", ")}`]);
    // Typing narrows the list.
    src.value = options[0];
    src.dispatchEvent(new window.Event("input", { bubbles: true }));
    const filtered = [...document.querySelectorAll(".select-popover .select-option")]
      .map((b) => b.textContent);
    assert.ok(filtered.length <= options.length, "typing filters the list");
  });
  // Dismiss the suggestion popover (a fixed-position node) before moving on.
  document.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  modal?.remove();
  await wait(300);
}

// ---- Network detail page ---------------------------------------------------
// Selecting a network in the sidebar opens its own page (info cards + IPAM).
document.querySelector(".tree-net")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1500);

check("a network page shows card info and an IPAM table", () => {
  assert.ok(document.querySelector("#net-back"), "network page opened (back button)");
  const cards = [...document.querySelectorAll(".cards .card")];
  assert.ok(cards.length > 0, `${cards.length} info card(s)`);
  const labels = cards.map((c) => c.querySelector("i")?.textContent.trim());
  for (const want of ["TYPE", "SUBNET", "GATEWAY", "DHCP IPS REMAINING"]) {
    assert.ok(labels.includes(want), `${want} card present (${labels})`);
  }
  const remaining = cards.find((c) => c.querySelector("i")?.textContent.trim() === "DHCP IPS REMAINING");
  // Either "free / total" for a subnet, or a dash for a network with no pool.
  assert.match(remaining.querySelector("b").textContent.trim(), /^(\d+ \/ \d+|—)$/, "remaining count shape");
  assert.match(document.body.textContent, /IPAM Allocations/, "IPAM section present");
  assert.ok(document.querySelector("#ipam-add"), "add-allocation action present");
  results.push(["info", `network cards: ${labels.join(", ")}`]);
});

// ---- VM tabs ---------------------------------------------------------------
document.querySelector(".tree-row.tree-vm")
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1600);

check("VM view exposes Options and Firewall tabs, and XML has an icon", () => {
  const tabs = [...document.querySelectorAll(".tabbar .tab")].map((t) => t.textContent.trim());
  assert.deepEqual(tabs, ["Summary", "Console", "Hardware", "Options", "Firewall", "XML"],
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
  const lifecycle = [...document.querySelectorAll(".hwrow-label b")].map((b) => b.textContent.trim());
  for (const want of ["On Poweroff", "On Reboot", "On Crash", "Machine", "Architecture"]) {
    assert.ok(lifecycle.includes(want), `${want} shown (${lifecycle})`);
  }
  const machineRow = [...document.querySelectorAll(".hwrow")]
    .find((s) => s.querySelector(".hwrow-label b")?.textContent.trim() === "Machine");
  assert.match(machineRow?.querySelector(".hwctl")?.textContent ?? "", /pc-|q35/, `real machine type`);
});

tabNamed("Firewall")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1800);

check("VM Firewall tab is a rules editor with add and apply", () => {
  assert.equal(document.querySelector(".tabbar .tab.active")?.textContent.trim(), "Firewall");
  assert.ok(document.querySelector("#vmfw-add"), "add-rule action");
  assert.ok(document.querySelector("#vmfw-apply"), "apply action");
  assert.ok(document.querySelector("#vmfw-add").querySelector("svg"), "add-rule has an icon");
});

check("the VM rules table starts with the no-rules empty state", () => {
  const body = document.querySelector("#vm-body")?.textContent ?? "";
  assert.match(body, /No rules/, "empty state for a guest without rules");
});

for (const [state, name] of results) {
  console.log(`${state === "pass" ? "✔" : state === "info" ? "·" : "✖"} ${name}`);
}
await removeTestVm(base, fixture);
const failed = results.filter(([s]) => s === "FAIL");
window.close();
if (failed.length) { console.error(`\n${failed.length} check(s) failed`); process.exit(1); }
console.log("\nall tab checks passed");
process.exit(0);
