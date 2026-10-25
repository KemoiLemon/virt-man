// XML icon, Options tab, and the two Firewall tabs (daemon + VM).
//
// Usage: node test/live-tabs.mjs [baseUrl]

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";
import { ensureTestPool, ensureTestVm, removeTestPool, removeTestVm } from "./_fixture.mjs";

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
// whatever happens to be on this machine. The storage tab and the pool page
// checks need a pool, so one is provisioned first.
const testPool = await ensureTestPool(base);
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
  assert.deepEqual(tabs, ["Dashboard", "VMs", "Containers", "Networks", "Storage", "Firewall", "Shell"], `tabs: ${tabs}`);
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

check("Networks tab renders cards with subnet and gateway, no bridge", () => {
  const active = document.querySelector(".tabbar .tab.active")?.textContent.trim();
  assert.equal(active, "Networks", "Networks tab active");
  const cards = [...document.querySelectorAll("#nets-list .net-card")];
  assert.ok(cards.length > 0, `${cards.length} network card(s)`);
  const first = cards[0];
  const badges = [...first.querySelectorAll(".storage-card-head .badge")].map((b) => b.textContent.trim());
  assert.ok(badges.some((b) => ["NAT", "Bridge", "Isolated"].includes(b)), `type badge (${badges})`);
  const specLabels = [...first.querySelectorAll(".spec b")].map((b) => b.textContent.trim());
  assert.deepEqual(specLabels.slice(0, 2), ["Subnet", "Gateway"],
    `subnet and gateway specs (${specLabels})`);
  assert.ok(!specLabels.includes("Bridge"), `no bridge spec (${specLabels})`);
  // Merged start/stop toggle: icon-only, no text.
  const toggle = first.querySelector("[data-net-toggle]");
  assert.ok(toggle, "merged start/stop toggle");
  assert.equal(first.querySelectorAll("[data-net-toggle]").length, 1, "exactly one toggle");
  assert.ok(toggle.querySelector("svg"), "toggle has an icon");
  assert.equal(toggle.textContent.trim(), "", "toggle is icon-only");
  assert.ok(first.querySelector("[data-net-edit]"), "edit button");
  assert.ok(first.querySelector("[data-delnet]"), "delete button");
});

check("the network list has no create card (creating is the tab bar's button)", () => {
  assert.equal(document.querySelectorAll("#nets-list .create-card, #nets-list .net-create-card").length, 0);
  assert.ok(document.querySelector("#create-net"), "the tab bar's Create Network remains");
});

check("start/stop highlight follows its state", () => {
  for (const c of document.querySelectorAll("#nets-list .net-card")) {
    const toggle = c.querySelector("[data-net-toggle]");
    const cardActive = c.querySelector(".storage-card-head .badge.on")?.textContent.trim() === "Active";
    assert.equal(toggle.classList.contains("on"), cardActive,
      `toggle highlight matches the active badge`);
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
  assert.ok(document.querySelector("#net-edit"), "network page opened");
  const cards = [...document.querySelectorAll(".cards .card")];
  assert.ok(cards.length > 0, `${cards.length} info card(s)`);
  const labels = cards.map((c) => c.querySelector("i")?.textContent.trim());
  for (const want of ["TYPE", "SUBNET", "GATEWAY", "DHCP IPS REMAINING"]) {
    assert.ok(labels.includes(want), `${want} card present (${labels})`);
  }
  assert.ok(!labels.includes("BRIDGE"), `no bridge card (${labels})`);
  assert.ok(!labels.includes("PERSISTENCE"), `no persistence card (${labels})`);
  const remaining = cards.find((c) => c.querySelector("i")?.textContent.trim() === "DHCP IPS REMAINING");
  // Either "free / total" for a subnet, or a dash for a network with no pool.
  assert.match(remaining.querySelector("b").textContent.trim(), /^(\d+ \/ \d+|—)$/, "remaining count shape");
  assert.match(document.body.textContent, /IPAM Allocations/, "IPAM section present");
  assert.ok(document.querySelector("#ipam-add"), "add-allocation action present");
  results.push(["info", `network cards: ${labels.join(", ")}`]);
});

// The 1s daemon poll repaints the pane. A wide table is its own scroller now,
// so the repaint must not send it back to the first column.
{
  const table = document.querySelector("#ipam-hosts");
  if (table) {
    table.scrollLeft = 60;
    await wait(1400);
    check("a daemon poll keeps the table's horizontal position", () => {
      const again = document.querySelector("#ipam-hosts");
      assert.ok(again, "the table is still rendered");
      assert.equal(again.scrollLeft, 60, "scrollLeft survives the repaint");
    });
  }
}

// Switching away from a resource page must stick: a stale selection lets the
// 1s poll keep re-rendering the network page over whatever the user moved to,
// which is the reported "the network page just returns".
//
// Navigation has to go through the sidebar: a resource page has no tab bar of
// its own (it replaces the server view entirely), so the only way out is
// clicking the server or a guest in the tree — exactly what the user does.
{
  document.querySelector(".tree-net")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(1300);
  assert.ok(document.querySelector("#net-edit"), "precondition: network page open");

  // Switch to the server, then to one of its guests.
  document.querySelector(".tree-row.tree-group")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(1200);
  check("switching to a server from a resource page leaves it behind", () => {
    assert.equal(document.querySelector("#net-edit"), null, "network page gone");
    assert.equal(document.querySelector("#pool-newvol"), null, "no pool page either");
    assert.ok(document.querySelector(".tabbar .tab"), "server view is showing");
  });

  // The resurrection only happened once the poll ticked, so wait past one.
  await wait(1600);
  check("…and it stays gone across a poll tick", () => {
    assert.equal(document.querySelector("#net-edit"), null, "poll did not resurrect it");
    assert.equal(document.querySelector("#pool-newvol"), null, "nor the pool page");
  });

  // Same for a pool page, and for switching straight to a guest.
  document.querySelector(".tree-pool")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(1300);
  assert.ok(document.querySelector("#pool-newvol"), "precondition: pool page open");
  document.querySelector(".tree-row.tree-vm")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(1600);
  check("switching to a guest from a pool page leaves it behind", () => {
    assert.equal(document.querySelector("#pool-newvol"), null, "pool page gone");
    assert.equal(document.querySelector("#net-edit"), null, "no network page either");
  });
}

// Uploading an ISO names the volume after the file unless told otherwise.
{
  document.querySelector(".tree-pool")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(1400);
  document.querySelector("#pool-upload")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(600);
  const modal = document.querySelector(".modal-back.open");
  const nameInput = modal?.querySelector("#up-name");
  const fileInput = modal?.querySelector("#up-file");

  // jsdom has no DataTransfer, so `files` is stubbed and `change` fired —
  // which is exactly what a real picker does.
  const pick = (filename) => {
    Object.defineProperty(fileInput, "files", {
      value: [{ name: filename }],
      configurable: true,
    });
    fileInput.dispatchEvent(new window.Event("change", { bubbles: true }));
  };

  check("picking a file names the upload volume", () => {
    assert.ok(nameInput && fileInput, "upload dialog has name and file fields");
    pick("debian-13.2.0-amd64-netinst.iso");
    assert.equal(nameInput.value, "debian-13.2.0-amd64-netinst.iso");
    // Re-picking replaces the name we filled in, but never a typed one.
    pick("archlinux-2026.03.01-x86_64.iso");
    assert.equal(nameInput.value, "archlinux-2026.03.01-x86_64.iso", "our auto-name is replaced");
    nameInput.value = "my custom iso";
    pick("ubuntu-26.04.iso");
    assert.equal(nameInput.value, "my custom iso", "a typed name is preserved");
  });
  modal?.remove();
  await wait(300);
}

// A slow resource fetch must not paint over a newer view: the poll can start
// a network render and the user then click a guest before it resolves.
{
  document.querySelector(".tree-net")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  // Immediately switch, without waiting for that fetch.
  document.querySelector(".tree-row.tree-vm")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(1800);
  check("an in-flight resource fetch cannot overwrite a newer view", () => {
    assert.equal(document.querySelector("#net-edit"), null, "network page did not win the race");
    assert.ok(document.querySelector(".tabbar .tab"), "VM view is intact");
    assert.ok(document.querySelector(".dhead-ic"), "VM title carries its kind icon");
  });
}

// ---- VM tabs ---------------------------------------------------------------
document.querySelector(".tree-row.tree-vm")
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1600);

check("VM view exposes Options and Firewall tabs, and XML has an icon", () => {
  const tabs = [...document.querySelectorAll(".tabbar .tab")].map((t) => t.textContent.trim());
  assert.deepEqual(tabs, ["Summary", "Console", "Hardware", "Options", "Network", "XML"],
    `tabs: ${tabs}`);
  assert.ok(tabNamed("XML").querySelector("svg"), "XML tab now carries an icon");
  assert.ok(tabNamed("Options").querySelector("svg"), "Options tab has an icon");
  assert.ok(tabNamed("Network").querySelector("svg"), "Network tab has an icon");
});

tabNamed("Options")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1600);

check("Options tab shows editable metadata read from the guest", () => {
  assert.equal(document.querySelector(".tabbar .tab.active")?.textContent.trim(), "Options");
  assert.ok(!document.querySelector("#opt-name"), "no resource name field");
  assert.ok(document.querySelector("#opt-title"), "nickname field");
  assert.ok(document.querySelector("#opt-desc"), "description field");
  assert.ok(document.querySelector("#opt-boot-list"), "boot device list");
  assert.ok(document.querySelector("#opt-save"), "save button");
  // Lifecycle facts must come from the real domain, not placeholders.
  const lifecycle = [...document.querySelectorAll(".hwrow-label b")].map((b) => b.textContent.trim());
  for (const want of ["On Poweroff", "On Reboot", "On Crash", "Machine", "Architecture"]) {
    assert.ok(lifecycle.includes(want), `${want} shown (${lifecycle})`);
  }
  assert.ok(!lifecycle.includes("Persistence"), `no persistence row (${lifecycle})`);
  const machineRow = [...document.querySelectorAll(".hwrow")]
    .find((s) => s.querySelector(".hwrow-label b")?.textContent.trim() === "Machine");
  assert.match(machineRow?.querySelector(".hwctl")?.textContent ?? "", /pc-|q35/, `real machine type`);
});

tabNamed("Network")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1800);
// The fixture may reuse a guest that already has rules; compare against what
// the daemon holds for it rather than assuming an empty ruleset.
const openVm = document.querySelector(".tree-row.tree-vm.sel")?.dataset.vm ?? fixture.name;
const fwRules = (await (await fetch(`${base}/proxy/${fixture.conn}/domains/${encodeURIComponent(openVm)}/firewall`)).json()).rules ?? [];

check("VM Network tab lists the guest's interfaces", () => {
  assert.equal(document.querySelector(".tabbar .tab.active")?.textContent.trim(), "Network");
  const rows = document.querySelectorAll("#vmnet-nics tr[data-nic]");
  assert.ok(rows.length >= 1, "the fixture's NIC is listed");
  for (const r of rows) {
    assert.match(r.textContent, /([0-9a-f]{2}:){5}[0-9a-f]{2}/, "each row shows its MAC");
    for (const a of ["data-nic-ipam", "data-nic-rate", "data-nic-del"]) {
      assert.ok(r.querySelector(`[${a}]`), `${a} action`);
    }
  }
  assert.ok(document.querySelector("#vmnet-add"), "add-interface action");
});

check("VM Network tab carries the firewall editor with add and apply", () => {
  assert.ok(document.querySelector("#vmfw-add"), "add-rule action");
  assert.ok(document.querySelector("#vmfw-apply"), "apply action");
  const logs = document.querySelector("#vmfw-logs");
  assert.ok(logs, "access-logs action");
  assert.equal(logs.parentElement, document.querySelector("#vmfw-apply").parentElement, "in the same row as Add rule / Apply");
  assert.ok(logs.classList.contains("al-open"), "pushed to the right-hand side");
  assert.ok(document.querySelector("#vmfw-add").querySelector("svg"), "add-rule has an icon");
});

check("the VM rules table shows the guest's rules, or the empty state when it has none", () => {
  const body = document.querySelector("#vm-body")?.textContent ?? "";
  const rows = document.querySelectorAll("#vmnet-fw [data-r-dir]").length;
  if (fwRules.length) assert.equal(rows, fwRules.length, `${rows} rows for ${fwRules.length} rules`);
  else assert.match(body, /No rules/, "empty state for a guest without rules");
});

for (const [state, name] of results) {
  console.log(`${state === "pass" ? "✔" : state === "info" ? "·" : "✖"} ${name}`);
}
await removeTestVm(base, fixture);
await removeTestPool(base, testPool);
const failed = results.filter(([s]) => s === "FAIL");
window.close();
if (failed.length) { console.error(`\n${failed.length} check(s) failed`); process.exit(1); }
console.log("\nall tab checks passed");
process.exit(0);
