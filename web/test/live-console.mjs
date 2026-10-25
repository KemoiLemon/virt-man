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
  for (const id of ["#con-zoom-reset", "#con-shorter", "#con-taller", "#con-full"]) {
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
const guestRunning = document.querySelector("#vm-start")?.disabled === true;
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
    for (const id of ["#con-zoom-reset", "#con-shorter", "#con-taller", "#con-full", "#con-cad", "#con-type"]) {
      assert.equal(document.querySelector(id).disabled, true, `${id} disabled`);
    }
  });
} else {
  const canvas = document.querySelector(".console-stage canvas");
  // A stopped guest (the reused fixture) has no framebuffer: the toolbar stays
  // enabled until the attach fails, so also require the guest to be running.
  const connected = guestRunning && !document.querySelector("#con-zoom-reset").disabled;

  if (connected) {
    check("the viewer paints into a canvas sized to the guest", () => {
      assert.ok(canvas, "canvas present");
      assert.ok(canvas.width > 0 && canvas.height > 0,
        `backing store is guest-sized (${canvas.width}x${canvas.height})`);
    });

    const label = () => document.querySelector("#con-zoom-reset").textContent.trim();
    document.querySelector("#con-zoom-reset").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
    await wait(200);
    check("the zoom button leaves fit mode and reports 100%", () => {
      assert.equal(label(), "100%", `zoom label (${label()})`);
    });

    document.querySelector("#con-zoom-reset").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
    await wait(200);
    check("pressing it again restores automatic scaling", () => {
      assert.equal(label(), "Fit", "back to fit");
    });

    // jsdom has no layout, so the stage measures 0 and the height clamps at its
    // 160px floor; what is checkable is that the buttons set and store a height.
    document.querySelector("#con-taller").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
    check("Taller sets an explicit stage height and remembers it", () => {
      const h = document.querySelector(".console-stage").style.height;
      assert.match(h, /^\d+px$/, `stage height (${h})`);
      assert.equal(window.localStorage.getItem("console-height"), h, "stored for the next visit");
    });
    document.querySelector("#con-shorter").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
    check("Shorter never goes below the minimum height", () => {
      const h = parseInt(document.querySelector(".console-stage").style.height, 10);
      assert.ok(h >= 160, `stage height (${h}px)`);
    });
  } else if (!guestRunning) {
    // The attach to a stopped guest settles asynchronously; the suite never starts the VM.
    results.push(["info", "guest is stopped — viewer checks skipped"]);
  } else {
    check("an unavailable console disables its controls instead of going dead", () => {
      for (const id of ["#con-zoom-reset", "#con-shorter", "#con-taller", "#con-full", "#con-cad", "#con-type"]) {
        assert.equal(document.querySelector(id).disabled, true, `${id} disabled`);
      }
      const hint = document.querySelector("#con-hint")?.textContent ?? "";
      assert.match(hint, /unavailable/i, `explains why: ${hint}`);
    });
  }
}

check("the toolbar offers zoom, resize, fullscreen, clipboard and Ctrl+Alt+Del", () => {
  for (const id of ["#con-cad", "#con-zoom-reset", "#con-shorter", "#con-taller", "#con-full", "#con-type"]) {
    assert.ok(document.querySelector(id), `${id} present`);
  }
  // The noVNC clipboard buttons are still gone; our own type button replaced
  // them rather than reviving them. The speed preset went with them.
  for (const id of ["#con-preset", "#con-paste", "#con-copy", "#con-speed"]) {
    assert.equal(document.querySelector(id), null, `${id} removed`);
  }
});

// The typing flow needs a LIVE viewer: the daemon only accepts /type against a
// running VNC session, and a guest that is shut off (which the fixture defines,
// so the state-aware checks hold) has none. So this runs only when the viewer
// attached; otherwise the controls must be inert, which is asserted below.
const attached = guestRunning && !document.querySelector("#con-zoom-reset")?.disabled;
const typed = [];
const realFetch = window.fetch;

if (!spice && attached) {
  window.fetch = async (input, init) => {
    const url = typeof input === "string" ? input : input.url;
    if (url.endsWith("/type")) typed.push(JSON.parse(init?.body ?? "{}").text);
    return realFetch(input, init);
  };

  document.querySelector("#con-type")
    ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(400);

  check("the clipboard button opens a multi-line box", () => {
    const ta = document.querySelector("#ask-text");
    assert.ok(ta, "a textarea is offered");
    assert.equal(ta.tagName, "TEXTAREA", "multi-line, not a one-line input");
    assert.equal(ta.rows, 8, `room for a snippet (rows=${ta.rows})`);
  });

  const ta = document.querySelector("#ask-text");
  if (ta) {
    ta.value = "echo hello\n";
    document.querySelector("#ask-ok")
      ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
    await wait(900);
  }

  check("submitting types the text into the guest, newline and all", () => {
    assert.equal(document.querySelector("#ask-text"), null, "the modal closed");
    assert.equal(typed.length, 1, `one type request: ${JSON.stringify(typed)}`);
    assert.equal(typed[0], "echo hello\n", "sent verbatim, in a single request");
  });

  window.fetch = realFetch;
} else {
  results.push(["info", spice
    ? "guest is not VNC — typing flow not exercised"
    : "guest is not running — typing flow not exercised"]);

  if (guestRunning) check("without a live viewer the clipboard button is disabled, not a silent no-op", () => {
    const b = document.querySelector("#con-type");
    assert.ok(b, "the button is still shown");
    assert.equal(b.disabled, true, "and cannot be clicked into nothing");
  });
}

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
