// The admin panel's primary action rides in the page header, inline with the
// section title, instead of a row of its own above the table. Asserts the real
// rendered DOM: the button is inside .dhead, after the .dtitle, and the body
// carries no actions row.
//
// Drives the real app in jsdom against a running backend.
//
//   node test/live-admin-head.mjs [base]   (default http://127.0.0.1:8799)

import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";

const base = process.argv[2] ?? "http://127.0.0.1:8799";

execFileSync("npx", ["vite", "build", "--logLevel", "error", "--outDir", "test/_live"],
  { stdio: "ignore", env: { ...process.env, VM_IIFE: "1" } });
const code = readFileSync("test/_live/app.iife.js", "utf8");
const css = readFileSync(`dist/assets/${readdirSync("dist/assets").find((f) => f.endsWith(".css"))}`, "utf8");

const results = [];
const check = (name, ok, detail = "") => results.push([ok, name, detail]);

// section -> the button id its painter puts in the header.
const WANT = {
  users: "add-user",
  groups: "add-group",
  roles: "add-role",
  permissions: "add-acl",
  daemons: "add-daemon",
  keys: "add-key",
  activity: "act-clear",
};

const page = async (path) => {
  const vc = new VirtualConsole();
  vc.on("jsdomError", (e) => { if (!/not implemented|canvas|WebSocket/i.test(e.message)) console.error("jsdom:", e.message); });
  const dom = new JSDOM(`<!doctype html><html><head><style>${css}</style></head><body><div id="app"></div></body></html>`,
    { url: base + path, runScripts: "dangerously", pretendToBeVisual: true, virtualConsole: vc });
  const w = dom.window;
  w.fetch = (i, init) => fetch(new URL(typeof i === "string" ? i : i.url, base).toString(), init);
  w.ResizeObserver ??= class { observe() {} unobserve() {} disconnect() {} };
  w.eval(code);
  await new Promise((r) => w.setTimeout(r, 2500));
  return w;
};

for (const [section, id] of Object.entries(WANT)) {
  const w = await page(`/admin/${section}`);
  const head = w.document.querySelector("#content .dhead");
  const title = head?.querySelector(".dtitle");
  const btn = w.document.querySelector(`#${id}`);
  const inHead = !!(btn && head && head.contains(btn));

  check(`${section}: the action is in the page header`, inHead,
    btn ? (head?.contains(btn) ? "" : "button is outside .dhead") : `#${id} not rendered`);

  // Inline with the title: the title precedes it and they share one flex row.
  if (btn && title && head) {
    const order = title.compareDocumentPosition(btn) & w.Node.DOCUMENT_POSITION_FOLLOWING;
    check(`${section}: it follows the title`, !!order);
  }

  // No leftover actions row in the body.
  const bodyRow = w.document.querySelector("#admin-area > .actions, #admin-area > .actions-end");
  check(`${section}: no separate actions row in the body`, !bodyRow,
    bodyRow ? bodyRow.className : "");

  // No mini-hint left in this section.
  const hint = w.document.querySelector("#admin-area .hint");
  check(`${section}: no hint text`, !hint, hint ? hint.textContent.slice(0, 60) : "");
}

let bad = 0;
for (const [ok, name, detail] of results) {
  if (!ok) bad++;
  console.log(`${ok ? "ok  " : "FAIL"}  ${name}${!ok && detail ? `  [${detail}]` : ""}`);
}
console.log(bad ? `\n${bad} failed of ${results.length}` : `\nall ${results.length} passed`);
process.exit(bad ? 1 : 0);
