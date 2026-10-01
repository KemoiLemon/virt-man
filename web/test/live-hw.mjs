// Live DOM checks for the sidebar auto-expand, edit button, tab-bar actions
// and the Proxmox-style hardware editor.
//
// Usage: node test/live-hw.mjs [baseUrl]

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";
import { ensureTestVm, removeTestVm } from "./_fixture.mjs";

const base = process.argv[2] ?? "http://127.0.0.1:8801";

execFileSync("npx", ["vite", "build", "--logLevel", "error", "--outDir", "test/_live"], {
  cwd: process.cwd(), stdio: "inherit", env: { ...process.env, VM_IIFE: "1" },
});

const code = readFileSync("test/_live/app.iife.js", "utf8");
const cssFile = readdirSync("dist/assets").find((f) => f.endsWith(".css"));
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

check("sidebar auto-expands guests without selecting the server", () => {
  // Nothing has been clicked yet: guests must already be nested.
  const servers = [...document.querySelectorAll(".tree-row.tree-group")];
  assert.ok(servers.length > 0, `${servers.length} server node(s)`);
  const guests = [...document.querySelectorAll(".tree-row.tree-vm")];
  assert.ok(guests.length > 0, `guests visible before any click (${guests.length})`);
  assert.equal(
    document.querySelectorAll(".tree-row.tree-group.sel").length, 0,
    "no server is selected yet",
  );
  results.push(["info", `auto-expanded guests: ${guests.length}`]);
});

// Server management lives on the admin page's Daemons section now (the
// Datacenter's Servers tab is gone): Add daemon plus per-row actions.
document.querySelector("#admin-btn")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(600);
document.querySelector('#tree [data-admin-section="daemons"]')?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(900);

check("server management lives on the admin Daemons page", () => {
  assert.ok(!document.querySelector('[data-dctab="servers"]'), "no Datacenter Servers tab");
  const add = document.querySelector("#admin-area #add-daemon");
  assert.ok(add, "Add daemon action");
  assert.match(add.textContent, /Add daemon/);
});

check("server rows expose an edit button, not a test button", () => {
  const edit = document.querySelector("[data-edit-conn]");
  assert.ok(edit, "edit button present");
  assert.ok(edit.querySelector("svg"), "edit button is an icon button");
  assert.equal(document.querySelectorAll("[data-test]").length, 0, "no test button remains");
  assert.ok(document.querySelector("[data-del-conn]"), "delete button still present");
});

check("server rows expose edit, permissions and delete icon buttons", () => {
  const row = document.querySelector("#admin-area tr:has([data-edit-conn])");
  assert.ok(row, "servers are a list, one row each");
  const actions = row.querySelector(".row-actions");
  assert.ok(actions, "buttons share a flex row");
  assert.ok([...actions.querySelectorAll(".btn")].every((b) => b.classList.contains("icon")), "all icon buttons");
  assert.ok(actions.querySelector("[data-edit-conn]") && actions.querySelector("[data-del-conn]"), "edit + delete");
});

// Edit dialog must prefill from the existing connection.
document.querySelector("[data-edit-conn]")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(400);
check("edit opens a prefilled dialog", () => {
  const modal = document.querySelector(".modal-back.open");
  assert.ok(modal, "modal opened");
  assert.match(modal.querySelector(".modal-head").textContent, /Edit Server/i);
  assert.ok(modal.querySelector("#conn-name").value, "name prefilled");
  assert.ok(modal.querySelector("#conn-url").value.startsWith("http"), "url prefilled");
  modal.remove();
});

// Leave the admin page (the return button) for the resource tree.
document.querySelector("#admin-btn")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(600);

// Connection view: create buttons on the tab bar.
document.querySelector(".tree-row.tree-group")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1200);
// The server opens on the Dashboard tab; Create VM lives on the VMs tab.
[...document.querySelectorAll(".tabbar .tab")]
  .find((t) => t.textContent.trim() === "VMs")
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(700);
check("connection view puts Create VM on the tab bar", () => {
  const actions = document.querySelector(".tabbar .tabbar-actions");
  assert.ok(actions, "connection tab bar has actions");
  assert.ok(actions.querySelector("#create-vm"), "Create VM on the tab bar");
});

// Create VM starts with an empty CD/DVD drive in the device sidebar.
document.querySelector("#create-vm")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1500);
check("Create VM includes an empty CD/DVD drive by default", () => {
  const modal = document.querySelector(".modal.hw-modal");
  assert.ok(modal, "create-vm editor opened");
  const items = [...modal.querySelectorAll(".hw-item")];
  assert.ok(
    items.some((i) => i.textContent.includes("CD/DVD Drive")),
    "CD/DVD Drive in the sidebar",
  );
  results.push(["info", `create-vm sidebar: ${items.map((i) => i.querySelector(".hw-tx b")?.textContent).join(", ")}`]);
});
document.querySelector("#hw-cancel")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(400);

for (const [tab, id] of [["Networks", "#create-net"], ["Storage", "#create-pool"]]) {
  [...document.querySelectorAll(".tabbar .tab")]
    .find((t) => t.textContent.trim() === tab)
    ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(700);
  check(`${tab} tab shows its create button on the tab bar`, () => {
    const actions = document.querySelector(".tabbar .tabbar-actions");
    assert.ok(actions?.querySelector(id), `${id} present on the tab bar`);
  });
}

// VM hardware tab.
document.querySelector(".tree-row.tree-vm")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1200);
check("VM view offers a Hardware tab", () => {
  const tabs = [...document.querySelectorAll(".tabbar .tab")].map((t) => t.textContent.trim());
  assert.deepEqual(tabs, ["Summary", "Console", "Hardware", "Options", "Network", "XML"]);
});

// Open the inline hardware editor embedded in the Hardware tab.
[...document.querySelectorAll(".tabbar .tab")]
  .find((t) => t.textContent.trim() === "Hardware")
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1800);
check("Hardware tab embeds the editor, no edit button", () => {
  assert.ok(document.querySelector(".hw-inline"), "inline editor rendered");
  assert.ok(document.querySelector(".hw-inline .hw-side"), "device sidebar present");
  assert.ok(document.querySelector(".hw-inline .hw-pane"), "form pane present");
  assert.equal(document.querySelectorAll("#hw-edit").length, 0, "no edit-hardware button remains");
  const items = [...document.querySelectorAll(".hw-inline .hw-item")];
  assert.ok(items.length >= 4, `${items.length} sidebar entries`);
  assert.equal(document.querySelectorAll(".hw-inline .hw-item.active").length, 1, "one active entry");
  assert.ok(document.querySelector(".hw-inline #hw-add"), "Add Hardware pinned to the sidebar");
  assert.ok(document.querySelector(".hw-inline #hw-save"), "Save action present");
});

// Navigate to Processors and confirm the form is populated.
[...document.querySelectorAll(".hw-inline .hw-item")]
  .find((i) => i.textContent.includes("Processors"))
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(500);
check("selecting a device opens its form page", () => {
  const page = document.querySelector(".hw-inline .hw-page.active");
  assert.ok(page, "a page is active");
  assert.match(page.querySelector(".hw-title").textContent, /Processors/);
  const input = page.querySelector("#g-cpus");
  assert.ok(input, "vCPU field rendered");
  assert.ok(+input.value >= 1, `prefilled from the guest (${input.value})`);
});

// Add Hardware chooser (still a modal, opened from the inline sidebar).
document.querySelector("#hw-add")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(900);
check("Add Hardware offers a catalogue with an XML preview", () => {
  const modal = document.querySelector(".modal.add-hw-modal");
  assert.ok(modal, "add-hw-modal opened");
  const types = [...modal.querySelectorAll(".add-hw-type")];
  assert.ok(types.length >= 6, `${types.length} device types`);
  assert.equal(modal.querySelectorAll(".add-hw-type.active").length, 1, "one type selected");
  assert.ok(modal.querySelector(".add-hw-preview code"), "shows an XML preview");
  results.push(["info", `device types: ${types.length}`]);
});

// Switch the display from VNC to SPICE and save, then back. This is the
// shape that was refused with "type='spice' on <audio> is only available to
// administrators": the guest has no <audio> of its own until libvirt adds the
// one that matches the display, so the save fails the first time it is tried.
// The save is asynchronous (runTask), so wait for the API to answer rather
// than for a fixed time.
[...document.querySelectorAll(".hw-inline .hw-item")]
  .find((i) => i.textContent.includes("Display"))
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(500);
const gfxNote = document.querySelector(".hw-inline .hw-page.active #g-gfx-note");
check("the Display page says what each graphics type costs", () => {
  assert.ok(document.querySelector(".hw-inline .hw-page.active #g-gfx"), "graphics picker present");
  assert.ok(gfxNote && gfxNote.textContent.trim().length > 0, "a note accompanies the picker");
});

// jsdom never fires `change` from a click on a listbox; the select is driven
// the way `enhanceSelects` does and the handler is called directly.
const switchDisplay = async (to) => {
  const sel = document.querySelector(".hw-inline .hw-page.active #g-gfx");
  sel.value = to;
  sel.onchange();
  await wait(400);
  const save = document.querySelector(".hw-inline #hw-save");
  const before = document.querySelectorAll(".toast").length;
  save.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  // The save is a runTask: it reports success or the server's reason itself.
  await wait(4000);
  const fresh = [...document.querySelectorAll(".toast")].slice(before);
  return fresh.map((t) => t.textContent.trim()).join(" | ");
};
const toSpice = await switchDisplay("spice");
check("switching a guest to SPICE saves", () => {
  assert.ok(!/administrators/.test(toSpice), `refused: ${toSpice}`);
  assert.match(toSpice, /saved|Hardware saved/i, `no success reported ("${toSpice}")`);
});
const backToVnc = await switchDisplay("vnc");
check("and back to VNC saves too", () => {
  assert.ok(!/administrators/.test(backToVnc), `refused: ${backToVnc}`);
  assert.match(backToVnc, /saved|Hardware saved/i, `no success reported ("${backToVnc}")`);
});

for (const [state, name] of results) {
  console.log(`${state === "pass" ? "✔" : state === "info" ? "·" : "✖"} ${name}`);
}
await removeTestVm(base, fixture);
const failed = results.filter(([s]) => s === "FAIL");
window.close();
if (failed.length) { console.error(`\n${failed.length} check(s) failed`); process.exit(1); }
console.log("\nall hardware/UI checks passed");
