// Console toolbar: centring, zoom, clipboard and performance presets.
//
// Usage: node test/live-console.mjs [baseUrl]

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";
import { ensureTestVm, removeTestVm } from "./_fixture.mjs";

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
// jsdom has no WebSocket, but the viewer no longer needs one: the RFB client
// lives in the daemon and frames arrive over fetch. Publish the globals the
// bundle expects when it runs outside a module scope.
globalThis.window = window;
globalThis.document = document;
// jsdom's canvas has no 2D context, so painting a frame would throw. The
// viewer's putImageData sits behind an optional call, so it degrades to a
// no-op here; geometry and controls are still assertable.
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

// The console tab renders one of two things: the canvas viewer for a VNC
// guest, or a virt-viewer handoff for anything else. Which one depends on the
// live VM, so detect it rather than assuming.
const offer = document.querySelector(".console-offer");
const spice = !!offer;
results.push(["info", spice
  ? "guest is not VNC — virt-viewer handoff shown"
  : "guest is VNC — canvas viewer attached"]);

if (spice) {
  check("a non-VNC guest is offered virt-viewer instead of a dead screen", () => {
    const link = document.querySelector("#con-vv");
    assert.ok(link, "download link present");
    assert.match(link.getAttribute("href") ?? "", /\/console\/vv$/, "points at the .vv endpoint");
    assert.ok((link.getAttribute("download") ?? "").endsWith(".vv"), "downloads a .vv file");
    const hint = document.querySelector("#con-hint")?.textContent ?? "";
    assert.match(hint, /virt-viewer/i, `explains why: ${hint}`);
  });

  check("controls that cannot work are disabled, not left pretending", () => {
    for (const id of ["#con-zoom-in", "#con-zoom-out", "#con-fit", "#con-zoom-reset", "#con-cad"]) {
      assert.equal(document.querySelector(id).disabled, true, `${id} disabled`);
    }
  });
} else {
  const canvas = document.querySelector(".console-stage canvas");
  const connected = !document.querySelector("#con-zoom-in").disabled;

  if (connected) {
    check("the viewer paints into a canvas sized to the guest", () => {
      assert.ok(canvas, "canvas present");
      assert.ok(canvas.width > 0 && canvas.height > 0,
        `backing store is guest-sized (${canvas.width}x${canvas.height})`);
    });

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
      for (const id of ["#con-zoom-in", "#con-zoom-out", "#con-fit", "#con-zoom-reset", "#con-cad"]) {
        assert.equal(document.querySelector(id).disabled, true, `${id} disabled`);
      }
      const hint = document.querySelector("#con-hint")?.textContent ?? "";
      assert.match(hint, /unavailable/i, `explains why: ${hint}`);
    });
  }
}

check("the toolbar offers zoom and Ctrl+Alt+Del, and no dead preset picker", () => {
  assert.ok(document.querySelector("#con-cad"), "Ctrl+Alt+Del");
  assert.ok(document.querySelector("#con-fit"), "fit");
  // The noVNC preset/clipboard controls are gone with the library itself;
  // leaving them behind would advertise features nothing implements.
  for (const id of ["#con-preset", "#con-paste", "#con-copy", "#con-type"]) {
    assert.equal(document.querySelector(id), null, `${id} removed`);
  }
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
await removeTestVm(base, fixture);
const failed = results.filter(([s]) => s === "FAIL");
// Exit before closing the window: pending app timers still reference it.
if (failed.length) { console.error(`\n${failed.length} check(s) failed`); process.exit(1); }
console.log("\nall console checks passed");
process.exit(0);
