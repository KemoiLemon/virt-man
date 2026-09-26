// Console toolbar: centring, zoom, clipboard and performance presets.
//
// Usage: node test/live-console.mjs [baseUrl]

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";

const base = process.argv[2] ?? "http://127.0.0.1:8813";

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
// jsdom has no ResizeObserver; the console registers one to re-fit.
window.ResizeObserver = class { observe() {} unobserve() {} disconnect() {} };
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

// Open a guest's console.
document.querySelector(".tree-row.tree-vm")
  ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1500);
tabNamed("Console")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(2200);

check("console renders a toolbar above the screen", () => {
  const bar = document.querySelector(".console-bar");
  assert.ok(bar, ".console-bar present");
  const stage = document.querySelector(".console-stage");
  assert.ok(stage, ".console-stage present");
  assert.ok(
    bar.compareDocumentPosition(stage) & window.Node.DOCUMENT_POSITION_FOLLOWING,
    "toolbar sits above the screen",
  );
});

check("the stage centres its content and has real height", () => {
  const cs = window.getComputedStyle(document.querySelector(".console-stage"));
  assert.equal(cs.display, "flex");
  // `safe center` is what keeps zoomed-in overflow reachable.
  assert.match(cs.alignItems, /center/, `align-items: ${cs.alignItems}`);
  assert.match(cs.justifyContent, /center/, `justify-content: ${cs.justifyContent}`);
  assert.equal(cs.overflow, "auto", "overflow scrolls when zoomed past the stage");
  // A collapsed stage would make the screen invisible; the height must not
  // depend on a flex parent that does not exist here.
  assert.ok(/clamp|px|vh/.test(cs.height), `stage has an explicit height (${cs.height})`);
});

check("zoom controls are present and start fitted", () => {
  for (const id of ["#con-zoom-in", "#con-zoom-out", "#con-fit", "#con-zoom-reset"]) {
    assert.ok(document.querySelector(id), `${id} present`);
  }
  assert.equal(document.querySelector("#con-zoom-reset").textContent.trim(), "Fit",
    "starts fitted to the window");
});

// jsdom cannot complete the console WebSocket, so the viewer never attaches.
// That is the "console unavailable" path: the toolbar must stay visible but
// inert, never silently dead. Interactive zoom/preset behaviour is covered by
// the unit tests over the same module.
const connected = !document.querySelector("#con-zoom-in").disabled;
results.push(["info", connected ? "viewer attached" : "viewer unavailable (jsdom has no WebSocket)"]);

if (connected) {
  const label = () => document.querySelector("#con-zoom-reset").textContent.trim();
  document.querySelector("#con-zoom-in").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(200);
  check("zooming in leaves fit mode and reports a percentage", () => {
    assert.match(label(), /^\d+%$/, `zoom label is a percentage (${label()})`);
  });

  document.querySelector("#con-fit").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(200);
  check("fit restores automatic scaling", () => {
    assert.equal(label(), "Fit", "back to fit");
  });

  for (let i = 0; i < 30; i++) {
    document.querySelector("#con-zoom-in").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  }
  await wait(200);
  check("zoom clamps at a sane maximum", () => {
    assert.ok(parseInt(label(), 10) <= 400, `clamped (${label()})`);
  });
  for (let i = 0; i < 40; i++) {
    document.querySelector("#con-zoom-out").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  }
  await wait(200);
  check("zoom clamps at a sane minimum", () => {
    assert.ok(parseInt(label(), 10) >= 25, `clamped (${label()})`);
  });
} else {
  check("an unavailable console disables its controls instead of going dead", () => {
    for (const id of ["#con-zoom-in", "#con-zoom-out", "#con-fit", "#con-preset",
      "#con-paste", "#con-type", "#con-copy", "#con-cad"]) {
      assert.equal(document.querySelector(id).disabled, true, `${id} disabled`);
    }
    const hint = document.querySelector("#con-hint")?.textContent ?? "";
    assert.match(hint, /unavailable/i, `explains why: ${hint}`);
  });
}

check("performance presets are offered", () => {
  const sel = document.querySelector("#con-preset");
  assert.ok(sel, "preset selector present");
  const values = [...sel.options].map((o) => o.value);
  assert.deepEqual(values, ["fast", "balanced", "quality"], `presets: ${values}`);
  assert.equal(sel.value, "balanced", "defaults to balanced");
  const hint = document.querySelector("#con-hint")?.textContent ?? "";
  assert.ok(hint.length > 10, `explains the trade-off: ${hint}`);
});

if (connected) {
  const sel = document.querySelector("#con-preset");
  sel.value = "fast";
  sel.dispatchEvent(new window.Event("change", { bubbles: true }));
  await wait(300);
  check("choosing a preset updates the explanation", () => {
    const hint = document.querySelector("#con-hint")?.textContent ?? "";
    assert.match(hint, /latency|slow links/i, `hint tracks the preset: ${hint}`);
  });
}

check("clipboard controls cover both directions plus typing", () => {
  assert.ok(document.querySelector("#con-paste"), "paste (host -> guest)");
  assert.ok(document.querySelector("#con-copy"), "copy (guest -> host)");
  assert.ok(document.querySelector("#con-type"), "type as key events");
  assert.ok(document.querySelector("#con-cad"), "Ctrl+Alt+Del");
});

check("the preset picker uses the themed listbox", () => {
  const sel = document.querySelector("#con-preset");
  assert.equal(window.getComputedStyle(sel).display, "none", "native select hidden");
  assert.ok(sel.nextElementSibling?.classList.contains("select-trigger"), "themed trigger");
});

// Leaving the console must tear the session down.
tabNamed("Summary")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1200);
check("leaving the console tab tears the session down", () => {
  assert.ok(!document.querySelector(".console-stage"), "console stage removed");
});

for (const [state, name] of results) {
  console.log(`${state === "pass" ? "✔" : state === "info" ? "·" : "✖"} ${name}`);
}
const failed = results.filter(([s]) => s === "FAIL");
window.close();
if (failed.length) { console.error(`\n${failed.length} check(s) failed`); process.exit(1); }
console.log("\nall console checks passed");
process.exit(0);
