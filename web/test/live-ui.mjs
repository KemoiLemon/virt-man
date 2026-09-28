// End-to-end DOM smoke check that boots the REAL app against a RUNNING
// virt-man-web instance.
//
// jsdom does not execute <script type="module"> at all, so the shipped ESM
// bundle can never run there. Instead this builds the same entry (src/main.ts)
// as a classic IIFE, injects it into the served index.html, and lets it boot
// against the live API — exercising the real render paths.
//
// Usage: node test/live-ui.mjs [baseUrl]

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";
import { ensureTestVm, removeTestVm } from "./_fixture.mjs";

const base = process.argv[2] ?? "http://127.0.0.1:8799";

// Build an IIFE copy of the app entry that jsdom can execute.
execFileSync(
  "npx",
  ["vite", "build", "--logLevel", "error", "--outDir", "test/_live",
   "--assetsDir", ".", "--target", "es2020"],
  { cwd: process.cwd(), stdio: "inherit", env: { ...process.env, VM_IIFE: "1" } },
);

const code = readFileSync("test/_live/app.iife.js", "utf8");
// Use the CSS from the real production build (lib mode inlines its own copy),
// so the assertions run against exactly what ships.
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

// jsdom ships no fetch. Hand the page Node's fetch, resolving the app's
// same-origin relative paths against the live server.
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
await wait(2000);

const results = [];
const check = (name, fn) => {
  try {
    fn();
    results.push(["pass", name]);
  } catch (e) {
    results.push(["FAIL", `${name} — ${e.message}`]);
  }
};

check("app shell rendered", () => {
  assert.ok(document.querySelector(".topbar"), "topbar present");
  assert.ok(document.querySelector(".layout"), "layout present");
});

// The full vocabulary ported from vm-man.
const VARS = [
  "--bg", "--panel", "--panel2", "--line", "--line2", "--fg", "--dim",
  "--accent", "--accent-fg", "--accent-bg", "--accent-bg-hover",
  "--storage-system", "--ok", "--warn", "--bad", "--bad-fg", "--bad-bg",
  "--input-bg", "--code-bg", "--hover", "--hover-line", "--scroll",
  "--scroll-hover", "--shade",
];

check("all 24 theme variables resolve from the stylesheet", () => {
  const cs = window.getComputedStyle(document.documentElement);
  const missing = VARS.filter((v) => !cs.getPropertyValue(v).trim());
  assert.equal(missing.length, 0, `unresolved: ${missing}`);
});

check("both themes define every variable, with distinct palettes", () => {
  // jsdom reads custom properties but never substitutes var() in computed
  // styles, so assert the variable values themselves rather than painted
  // colours. (The var() plumbing is covered by the stylesheet audit.)
  const root = document.documentElement;
  const readAll = () => {
    const cs = window.getComputedStyle(root);
    return Object.fromEntries(
      VARS.map((v) => [v, cs.getPropertyValue(v).trim()]),
    );
  };
  root.setAttribute("data-theme", "dark");
  const dark = readAll();
  root.setAttribute("data-theme", "light");
  const light = readAll();
  root.setAttribute("data-theme", "dark");

  const missing = VARS.filter((v) => !dark[v] || !light[v]);
  assert.equal(missing.length, 0, `variables missing in a theme: ${missing}`);
  const same = VARS.filter((v) => dark[v] === light[v]);
  assert.ok(
    same.length <= 1,
    `themes should differ on nearly every colour, shared: ${same}`,
  );
});

check("icons render as inline svg, not text glyphs", () => {
  const svgs = document.querySelectorAll(".topbar svg");
  assert.ok(svgs.length > 0, `topbar has ${svgs.length} inline svg icon(s)`);
});

check("no native select is visible; each has a themed trigger", () => {
  const selects = [...document.querySelectorAll("select")];
  for (const s of selects) {
    assert.equal(window.getComputedStyle(s).display, "none", "select hidden");
    assert.ok(
      s.nextElementSibling?.classList.contains("select-trigger"),
      "select followed by .select-trigger",
    );
  }
  results.push(["info", `selects on the default view: ${selects.length}`]);
});

check("themed dialog opens instead of native window.prompt", () => {
  // Add Resource moved off the sidebar into the root's right-click menu.
  assert.equal(document.querySelector("#add-resource"), null, "no sidebar Add Resource button");
  document.querySelector("#dc-row").dispatchEvent(new window.MouseEvent("contextmenu", {
    bubbles: true, cancelable: true, clientX: 50, clientY: 50,
  }));
  const addResource = [...document.querySelectorAll(".ctxmenu-item")]
    .find((b) => b.textContent.trim() === "Add Resource");
  assert.ok(addResource, "the Datacenter menu offers Add Resource");
  addResource.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  const modal = document.querySelector(".modal-back.open");
  assert.ok(modal, "a themed modal opened (not window.prompt)");
  // The chooser offers all four resource kinds, daemon included.
  const kinds = [...modal.querySelectorAll("[data-create]")].map((b) => b.dataset.create);
  for (const want of ["daemon", "vm", "network", "pool"]) {
    assert.ok(kinds.includes(want), `${want} offered (${kinds})`);
  }
  // Choosing Daemon hands off to the server connection dialog.
  modal.querySelector('[data-create="daemon"]').dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  const conn = document.querySelector(".modal-back.open");
  assert.ok(conn, "Daemon opens a follow-up dialog");
  assert.match(conn.querySelector(".modal-head").textContent, /Server/i, "Add Server dialog");
  conn.remove();
});

// Admin page: the activity log lives there now, and the old bottom-right
// task dock is gone.
{
  const click = (el) => el?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  click(document.querySelector("#admin-btn"));
  await wait(900);
  const openActivity = async () => {
    click(document.querySelector('#tree [data-admin-section="activity"]'));
    await wait(900);
  };
  check("the admin page offers an Activity section", () => {
    assert.ok(document.querySelector("#tree [data-admin-section]"), "admin page opened");
    assert.ok(document.querySelector('#tree [data-admin-section="activity"]'), "activity section present");
    assert.equal(document.querySelector("#tasks"), null, "the old task dock is gone");
  });

  await openActivity();
  check("Activity renders the server log with a clear action", () => {
    const area = document.querySelector("#admin-area");
    assert.ok(area, "admin area rendered");
    // Either recorded rows, or the explicit empty state — never a blank pane.
    assert.ok(area.querySelector("table.grid"), "activity table rendered");
    assert.ok(area.querySelector("#act-clear"), "clear action present");
  });

  // An action recorded through the real API must show up in the log, which is
  // what makes the panel a log rather than a placeholder.
  await window.fetch(new URL("/activity", base).toString(), {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ label: "live-suite probe", status: "done", message: "recorded" }),
  });
  await openActivity();
  check("a recorded action appears in the Activity log", () => {
    assert.match(document.querySelector("#admin-area").textContent, /live-suite probe/, "recorded entry listed");
  });

  // NOTE: deliberately does NOT click #act-clear. That action clears the
  // *whole* log, not the entry this suite added, so running the suite wiped
  // every user's audit trail — including the records needed to explain a
  // missing volume. The cost of leaving one "live-suite probe" row behind is
  // far smaller than destroying the log this panel exists to display, and the
  // button's presence is already asserted above.
  click(document.querySelector("#admin-btn")); // return to the resource tree
  await wait(900);
}

check("sidebar renders a nested resource tree", () => {
  assert.ok(document.querySelector(".restree"), ".restree container");
  const root = document.querySelector(".tree-row.tree-root");
  assert.ok(root, ".tree-root Datacenter node");
  assert.match(root.textContent, /Datacenter/);
  assert.ok(root.querySelector(".tree-ic svg"), "root has an icon");

  const servers = [...document.querySelectorAll(".tree-row.tree-group")];
  assert.ok(servers.length > 0, `${servers.length} .tree-group server node(s)`);
  for (const s of servers) {
    assert.ok(s.querySelector(".tree-ic svg"), "server node has an icon");
    assert.ok(s.querySelector(".tree-label"), "server node has a label");
  }
  assert.equal(document.querySelector(".tree-add"), null, "no Add Resource footer");
});

await (async () => {
  const rightClick = (el) => {
    const e = new window.MouseEvent("contextmenu", {
      bubbles: true, cancelable: true, clientX: 120, clientY: 200,
    });
    el.dispatchEvent(e);
    return e.defaultPrevented;
  };
  const labels = () =>
    [...document.querySelectorAll(".ctxmenu .ctxmenu-label")].map((x) => x.textContent.trim());

  const row = document.querySelector(".tree-row.tree-root");
  const prevented = rightClick(row);
  check("right-click replaces the browser menu with resource actions", () => {
    assert.ok(prevented, "the native context menu is suppressed");
    const menu = document.querySelector(".ctxmenu");
    assert.ok(menu, ".ctxmenu opened");
    const items = labels();
    assert.ok(items.length > 0, `menu has actions (${items})`);
    // Icons must land where `.ico > svg` can style them.
    assert.ok(menu.querySelector(".ctxmenu-item .ico > svg"), "item icon is a direct svg child");
    results.push(["info", `context actions: ${items.join(", ")}`]);
  });

  check("Escape dismisses the context menu", () => {
    document.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    assert.equal(document.querySelector(".ctxmenu"), null, "menu removed");
  });

  // Each row type offers its own actions.
  const guest = document.querySelector(".tree-row.tree-vm");
  if (guest) {
    rightClick(guest);
    check("a guest row offers its lifecycle and tab actions", () => {
      const items = labels();
      for (const want of ["Start", "Shutdown", "Console", "Hardware", "Delete"]) {
        assert.ok(items.includes(want), `${want} offered (${items})`);
      }
      const del = [...document.querySelectorAll(".ctxmenu-item")]
        .find((b) => b.textContent.trim() === "Delete");
      assert.ok(del.classList.contains("danger"), "Delete is styled destructive");
    });
    document.querySelector(".ctxmenu")?.remove();
  }

  const server = document.querySelector(".tree-row.tree-group");
  if (server) {
    rightClick(server);
    check("a server row offers its tabs and server actions", () => {
      const items = labels();
      for (const want of ["Dashboard", "VMs", "Networks", "Storage", "Remove server"]) {
        assert.ok(items.includes(want), `${want} offered (${items})`);
      }
    });
    document.querySelector(".ctxmenu")?.remove();
  }

  check("right-click on plain content keeps the native menu", () => {
    const pane = document.querySelector("#content");
    assert.equal(rightClick(pane), false, "content pane background is not hijacked");
    document.querySelector(".ctxmenu")?.remove();
  });

  // Resource surfaces in the content pane get the same menu as their tree row.
  const dcRow = document.querySelector("#dc-row");
  dcRow.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  document.querySelector('[data-dctab="guests"]')?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  const guestCard = document.querySelector("[data-dc-vm]");
  if (guestCard) {
    const prevented = rightClick(guestCard.querySelector(".storage-card-title") ?? guestCard);
    check("a guest card offers the guest's menu", () => {
      assert.ok(prevented, "native menu suppressed on a card");
      const items = labels();
      for (const want of ["Open", "Console", "Hardware", "XML", "Delete"]) {
        assert.ok(items.includes(want), `${want} offered (${items})`);
      }
    });
    document.querySelector(".ctxmenu")?.remove();
  }
  document.querySelector('[data-dctab="servers"]')?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  const serverCard = document.querySelector("[data-open-conn-row]");
  if (serverCard) {
    rightClick(serverCard);
    check("a server card offers the server's menu", () => {
      const items = labels();
      for (const want of ["Dashboard", "Storage", "Create VM", "Remove server"]) {
        assert.ok(items.includes(want), `${want} offered (${items})`);
      }
    });
    document.querySelector(".ctxmenu")?.remove();
  }
  check("every context-menu item has an icon", () => {
    // Re-queried here: a tab click re-renders the pane, so the cards found
    // above are detached and events on them never reach the listener.
    const surfaces = [
      document.querySelector("#dc-row"),
      document.querySelector(".tree-row.tree-group"),
      document.querySelector(".tree-row.tree-vm"),
      document.querySelector(".tree-row.tree-net"),
      document.querySelector(".tree-row.tree-pool"),
      document.querySelector("[data-open-conn-row]"),
    ];
    for (const el of surfaces.filter(Boolean)) {
      rightClick(el);
      const items = [...document.querySelectorAll(".ctxmenu-item")];
      assert.ok(items.length, "menu opened");
      for (const b of items) {
        assert.ok(b.querySelector(".ico > svg"), `"${b.textContent.trim()}" has an icon`);
      }
      document.querySelector(".ctxmenu")?.remove();
    }
  });
  document.querySelector('[data-dctab="dashboard"]')?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
})();

await (async () => {
  // Selecting a server must nest its guests beneath it and switch the pane.
  const server = document.querySelector(".tree-row.tree-group");
  server?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await new Promise((r) => window.setTimeout(r, 1200));

  check("selecting a server nests its guests and marks selection", () => {
    assert.ok(
      document.querySelector(".tree-row.tree-group.sel"),
      "selected server carries .sel",
    );
    const guests = [...document.querySelectorAll(".tree-row.tree-vm")];
    assert.ok(guests.length > 0, `${guests.length} nested .tree-vm guest row(s)`);
    const running = guests.filter((g) => g.querySelector(".tree-ic.on"));
    results.push((["info", `guests: ${guests.length}, running: ${running.length}`]));
    for (const g of guests) {
      assert.ok(g.querySelector(".tree-ic svg"), "guest has an icon");
      assert.ok(g.querySelector(".tree-state"), "guest shows its state");
    }
  });

  check("connection view uses a real tabbar with one active tab", () => {
    const bar = document.querySelector(".tabbar");
    assert.ok(bar, ".tabbar present on the connection view");
    const tabs = [...bar.querySelectorAll(".tab")];
    assert.deepEqual(
      tabs.map((t) => t.textContent.trim()),
      ["Dashboard", "VMs", "Networks", "Storage", "Firewall"],
      "all connection tabs render",
    );
    assert.equal(
      bar.querySelectorAll(".tab.active").length,
      1,
      "exactly one tab is active",
    );
    assert.ok(tabs[0].querySelector("svg"), "tabs carry icons");
  });

  // Every tab must still switch.
  for (const name of ["Networks", "Storage", "VMs", "Dashboard"]) {
    const tab = [...document.querySelectorAll(".tabbar .tab")]
      .find((t) => t.textContent.trim() === name);
    tab?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
    await new Promise((r) => window.setTimeout(r, 400));
    check(`tab "${name}" switches and becomes active`, () => {
      const active = document.querySelector(".tabbar .tab.active");
      assert.ok(active, "an active tab exists after switching");
      assert.equal(active.textContent.trim(), name, `${name} is active`);
    });
  }

  // Drill into a guest: the VM view has its own tabbar.
  const guest = document.querySelector(".tree-row.tree-vm");
  guest?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await new Promise((r) => window.setTimeout(r, 1200));

  check("VM view uses a tabbar and marks the guest selected", () => {
    assert.ok(document.querySelector(".tree-row.tree-vm.sel"), "guest .sel");
    const tabs = [...document.querySelectorAll(".tabbar .tab")];
    assert.deepEqual(
      tabs.map((t) => t.textContent.trim()),
      ["Summary", "Console", "Hardware", "Options", "Network", "XML"],
      "all VM tabs render",
    );
    assert.equal(
      document.querySelectorAll(".tabbar .tab.active").length,
      1,
      "exactly one VM tab is active",
    );
  });

  for (const name of ["XML", "Summary"]) {
    const tab = [...document.querySelectorAll(".tabbar .tab")]
      .find((t) => t.textContent.trim() === name);
    tab?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
    await new Promise((r) => window.setTimeout(r, 500));
    check(`VM tab "${name}" switches and becomes active`, () => {
      const active = document.querySelector(".tabbar .tab.active");
      assert.equal(active?.textContent.trim(), name, `${name} is active`);
    });
  }

  // The Summary tab is open again; it must carry the live dashboard widgets.
  await new Promise((r) => window.setTimeout(r, 800));
  check("VM summary lays out CPU/Memory/Disks on top, Network/Firewall below, hardware strip at bottom", () => {
    const metrics = [...document.querySelectorAll(".metric")];
    const labels = metrics.map((m) => m.querySelector("b")?.textContent.trim());
    for (const want of ["CPU", "Memory", "Disks", "Network", "Firewall"]) {
      assert.ok(labels.includes(want), `${want} card present (${labels})`);
    }

    // Top tier: three metric cards in one row.
    const top = document.querySelector(".vm-top");
    assert.ok(top, "vm-top row present");
    assert.equal(top.querySelectorAll(":scope > .metric").length, 3, "three cards across the top");
    for (const label of ["CPU", "Memory", "Disks"]) {
      const m = top.querySelector(`.metric b`);
      assert.ok(m, `${label} in top row`);
    }
    for (const label of ["CPU", "Memory", "Disks"]) {
      const m = [...top.querySelectorAll(".metric")].find((x) => x.querySelector("b")?.textContent.trim() === label);
      assert.ok(m, `${label} metric`);
      assert.ok(m.querySelector(".metric-ic svg"), "metric has an icon");
      assert.ok(m.querySelector(".metric-value"), "metric shows a headline value");
      assert.ok(m.querySelector(".meter .meter-fill"), "metric has a meter");
      assert.ok(m.querySelector("svg.spark"), "metric has a sparkline");
    }

    // Middle tier: Network and Firewall side by side.
    const mid = document.querySelector(".vm-mid");
    assert.ok(mid, "vm-mid row present");
    assert.equal(mid.querySelectorAll(":scope > .metric").length, 2, "two cards in the middle");
    const fwCard = [...mid.querySelectorAll(".metric")].find((x) => x.querySelector("b")?.textContent.trim() === "Firewall");
    assert.ok(fwCard, "Firewall card");
    const fwChips = [...fwCard.querySelectorAll(".fw-policy-chip")];
    assert.equal(fwChips.length, 3, "three policy chips (in/out/forward)");
    const fwChipLabels = fwChips.map((c) => c.querySelector("b")?.textContent.trim());
    for (const want of ["In", "Out", "Forward"]) {
      assert.ok(fwChipLabels.includes(want), `${want} policy chip (${fwChipLabels})`);
    }
    const netCard = [...mid.querySelectorAll(".metric")].find((x) => x.querySelector("b")?.textContent.trim() === "Network");
    assert.ok(netCard, "Network card");
    assert.ok(netCard.querySelector(".net-chart svg.spark"), "Network card graphs Rx/Tx");

    // Bottom: full-width hardware strip.
    const strip = document.querySelector(".hw-strip");
    assert.ok(strip, "hardware strip present");
    const chips = [...strip.querySelectorAll(".hw-chip")];
    assert.ok(chips.length >= 3, `${chips.length} hardware chips`);
    const chipLabels = chips.map((c) => c.querySelector(".hw-chip-tx b")?.textContent.trim());
    for (const want of ["BIOS", "Display"]) {
      assert.ok(chipLabels.includes(want), `${want} chip present (${chipLabels})`);
    }

    // Memory must reflect the real guest, not a placeholder.
    const memFoot = metrics
      .find((m) => m.querySelector("b")?.textContent.trim() === "Memory")
      ?.querySelector(".metric-foot")?.textContent ?? "";
    assert.match(memFoot, /\d.*(KiB|MiB|GiB)/, `memory footer shows real sizes: ${memFoot}`);
  });

  // Storage pools must show real capacity/allocation meters.
  const storageTab = [...document.querySelectorAll(".tabbar .tab")]
    .find((t) => t.textContent.trim() === "Storage");
  if (!storageTab) {
    const server = document.querySelector(".tree-row.tree-group");
    server?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
    await new Promise((r) => window.setTimeout(r, 800));
  }
  [...document.querySelectorAll(".tabbar .tab")]
    .find((t) => t.textContent.trim() === "Storage")
    ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await new Promise((r) => window.setTimeout(r, 1200));

  check("storage pools render meters from real capacity/allocation", () => {
    const cards = [...document.querySelectorAll(".storage-card")];
    assert.ok(cards.length > 0, `${cards.length} .storage-card(s)`);
    for (const c of cards) {
      assert.ok(c.querySelector(".storage-card-icon svg"), "pool card has an icon");
      const fill = c.querySelector(".meter .meter-fill");
      assert.ok(fill, "pool card has a meter");
      const width = fill.getAttribute("style") ?? "";
      assert.match(width, /width:\s*[\d.]+%/, `meter width is set: ${width}`);
      const foot = c.querySelector(".metric-foot")?.textContent ?? "";
      assert.match(foot, /used/, "shows used");
      assert.match(foot, /free/, "shows free");
      assert.match(foot, /(KiB|MiB|GiB|TiB)/, `real sizes: ${foot}`);
    }
    results.push(["info", `pool cards: ${cards.length}`]);
  });

  // Datacenter aggregate cards (the dashboard is the landing tab).
  document.querySelector("#dc-row")
    ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await new Promise((r) => window.setTimeout(r, 1200));

  check("Datacenter dashboard renders .card summary widgets", () => {
    const cards = [...document.querySelectorAll(".cards .card")];
    assert.ok(cards.length >= 5, `${cards.length} summary card(s)`);
    const labels = cards.map((c) => c.querySelector("i")?.textContent.trim());
    for (const want of ["SERVERS", "GUESTS", "RUNNING", "NETWORKS", "POOLS"]) {
      assert.ok(labels.includes(want), `${want} card present (${labels})`);
    }
    for (const c of cards) {
      assert.ok(c.querySelector(".card-ic svg"), "card has an icon");
      assert.match(c.querySelector("b")?.textContent ?? "", /^\d+$/, "card shows a count");
    }
    const servers = cards.find((c) => c.querySelector("i")?.textContent.trim() === "SERVERS");
    assert.ok(
      Number(servers.querySelector("b").textContent) > 0,
      "server count reflects the real connection",
    );
  });
})();

// Drive a form that actually contains selects, so the listbox is exercised
// against real app markup rather than only in unit tests.
await (async () => {
  const body = document.createElement("div");
  body.innerHTML = `
    <label class="field"><span>Format</span>
      <select id="probe-format">
        <option value="qcow2">qcow2</option>
        <option value="raw">raw</option>
      </select></label>`;
  document.body.append(body);
  // main.ts exports nothing (it self-boots), so drive the same listbox module
  // it calls from setHtml. The module binds document-level listeners at import
  // time, so point the globals at this jsdom window first.
  globalThis.window = window;
  globalThis.document = document;
  globalThis.Element = window.Element;
  globalThis.Event = window.Event;
  const { enhanceSelects } = await import("./_build/listbox.js");
  enhanceSelects(body);

  check("a rendered select becomes a themed, working listbox", () => {
    const sel = document.querySelector("#probe-format");
    const trigger = sel.nextElementSibling;
    assert.ok(trigger?.classList.contains("select-trigger"), "trigger built");
    assert.equal(window.getComputedStyle(sel).display, "none", "native hidden");

    trigger.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
    const opts = [...document.querySelectorAll(".select-popover .select-option")];
    assert.equal(opts.length, 2, "popover lists both options");

    opts.find((o) => o.textContent === "raw").dispatchEvent(
      new window.MouseEvent("click", { bubbles: true }),
    );
    assert.equal(sel.value, "raw", "value written back to the native select");
    assert.equal(trigger.querySelector(".select-value").textContent, "raw");
  });
  body.remove();
})();

// ---- tayne command palette (Ctrl+K) --------------------------------------
check("Ctrl+K opens the command palette", () => {
  document.dispatchEvent(new window.KeyboardEvent("keydown", { key: "k", ctrlKey: true, bubbles: true }));
  assert.ok(document.querySelector(".tayne-back.open"), "palette visible");
});

check("the palette filters commands as you type", () => {
  const input = document.querySelector(".tayne-input");
  assert.ok(input, "palette input present");
  assert.ok(document.querySelectorAll(".tayne-row").length > 0, "commands listed");
  input.value = "theme";
  input.dispatchEvent(new window.Event("input", { bubbles: true }));
  const rows = [...document.querySelectorAll(".tayne-row")];
  assert.equal(rows.length, 1, "only the theme command matches");
  assert.ok(rows[0].textContent.includes("Toggle theme"), "theme command shown");
});

check("the palette reaches every page, with an icon on each command", () => {
  const input = document.querySelector(".tayne-input");
  input.value = "";
  input.dispatchEvent(new window.Event("input", { bubbles: true }));
  const rows = [...document.querySelectorAll(".tayne-row")];
  for (const r of rows) {
    assert.ok(r.querySelector(".ico > svg"), `"${r.textContent.trim()}" has an icon`);
  }
  const labels = rows.map((r) => r.querySelector(".tayne-label").textContent);
  for (const want of ["Datacenter", "Datacenter — Guests", "Add resource", "Add server"]) {
    assert.ok(labels.includes(want), `${want} offered`);
  }
  assert.ok(labels.some((l) => / — Storage$/.test(l)), "a server's Storage tab");
  assert.ok(labels.some((l) => /^Pool — /.test(l)), "a storage pool page");
  assert.ok(labels.some((l) => /^Network — /.test(l)), "a network page");
  // Typing a guest's name finds its console directly.
  const vmRow = labels.find((l) => /^VM — .+ — Console$/.test(l));
  assert.ok(vmRow, "a guest's console");
});

check("running a palette page command navigates there", () => {
  const input = document.querySelector(".tayne-input");
  input.value = "Datacenter — Guests";
  input.dispatchEvent(new window.Event("input", { bubbles: true }));
  input.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  assert.equal(window.location.pathname, "/guests");
  document.dispatchEvent(new window.KeyboardEvent("keydown", { key: "k", ctrlKey: true, bubbles: true }));
});

check("Escape closes the palette", () => {
  document.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  assert.ok(!document.querySelector(".tayne-back.open"), "palette closed");
});

for (const [state, name] of results) {
  console.log(`${state === "pass" ? "✔" : state === "info" ? "·" : "✖"} ${name}`);
}
await removeTestVm(base, fixture);
const failed = results.filter(([s]) => s === "FAIL");
window.close();
if (failed.length) {
  console.error(`\n${failed.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall live UI checks passed");
