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
window.eval(code);
await new Promise((r) => window.setTimeout(r, 2000));

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
  const addServer = document.querySelector("#add-conn-row");
  assert.ok(addServer, "sidebar has an Add Server affordance");
  addServer.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  const modal = document.querySelector(".modal-back.open");
  assert.ok(modal, "a themed modal opened (not window.prompt)");
  modal.remove();
});

check("sidebar renders a nested resource tree", () => {
  assert.ok(document.querySelector(".restree"), ".restree container");
  const root = document.querySelector(".tree-row.tree-root");
  assert.ok(root, ".tree-root Datacenter node");
  assert.match(root.textContent, /Datacenter/);
  assert.ok(root.querySelector(".tree-ic svg"), "root has a Lucide icon");

  const servers = [...document.querySelectorAll(".tree-row.tree-group")];
  assert.ok(servers.length > 0, `${servers.length} .tree-group server node(s)`);
  for (const s of servers) {
    assert.ok(s.querySelector(".tree-ic svg"), "server node has an icon");
    assert.ok(s.querySelector(".tree-label"), "server node has a label");
  }
  assert.ok(document.querySelector(".tree-add"), "Add Server pinned footer");
});

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
      assert.ok(g.querySelector(".tree-ic svg"), "guest has a Lucide icon");
      assert.ok(g.querySelector(".tree-state"), "guest shows its state");
    }
  });

  check("connection view uses a real tabbar with one active tab", () => {
    const bar = document.querySelector(".tabbar");
    assert.ok(bar, ".tabbar present on the connection view");
    const tabs = [...bar.querySelectorAll(".tab")];
    assert.deepEqual(
      tabs.map((t) => t.textContent.trim()),
      ["VMs", "Networks", "Storage", "Firewall"],
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
  for (const name of ["Networks", "Storage", "VMs"]) {
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
      ["Summary", "Hardware", "Options", "Firewall", "Console", "XML"],
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
  check("VM summary renders .metric/.meter/.spark widgets from live stats", () => {
    const metrics = [...document.querySelectorAll(".metric")];
    assert.ok(metrics.length >= 2, `${metrics.length} .metric panel(s) (CPU + memory)`);
    const labels = metrics.map((m) => m.querySelector("b")?.textContent.trim());
    assert.ok(labels.includes("CPU"), `CPU metric present (${labels})`);
    assert.ok(labels.includes("Memory"), `Memory metric present (${labels})`);

    for (const m of metrics) {
      assert.ok(m.querySelector(".metric-ic svg"), "metric has a Lucide icon");
      assert.ok(m.querySelector(".metric-value"), "metric shows a headline value");
      assert.ok(m.querySelector(".meter .meter-fill"), "metric has a meter");
      const spark = m.querySelector("svg.spark");
      assert.ok(spark, "metric has a sparkline");
      assert.ok(spark.querySelector(".spark-line"), "sparkline has a trace");
      assert.ok(spark.querySelector(".spark-dot"), "sparkline marks the newest value");
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

  // Datacenter aggregate cards.
  document.querySelector("#dc-row")
    ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await new Promise((r) => window.setTimeout(r, 600));

  check("Datacenter renders .card summary widgets", () => {
    const cards = [...document.querySelectorAll(".cards .card")];
    assert.ok(cards.length >= 4, `${cards.length} summary card(s)`);
    const labels = cards.map((c) => c.querySelector("i")?.textContent.trim());
    for (const want of ["SERVERS", "GUESTS", "RUNNING", "STOPPED"]) {
      assert.ok(labels.includes(want), `${want} card present (${labels})`);
    }
    for (const c of cards) {
      assert.ok(c.querySelector(".card-ic svg"), "card has a Lucide icon");
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

check("Escape closes the palette", () => {
  document.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  assert.ok(!document.querySelector(".tayne-back.open"), "palette closed");
});

for (const [state, name] of results) {
  console.log(`${state === "pass" ? "✔" : state === "info" ? "·" : "✖"} ${name}`);
}
const failed = results.filter(([s]) => s === "FAIL");
window.close();
if (failed.length) {
  console.error(`\n${failed.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall live UI checks passed");
