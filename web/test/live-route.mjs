// Live routing checks: a reload lands on the view in the URL, clicks and tabs
// write the URL, Back/Forward restore views, and cards open their pages.
// Drives the real app in jsdom against a running backend.
//
//   node test/live-route.mjs [base]      (default http://127.0.0.1:8799)

import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";
import { ensureTestVm, removeTestVm } from "./_fixture.mjs";

const base = process.argv[2] ?? "http://127.0.0.1:8799";
const fx = await ensureTestVm(base);
const vmName = fx.name ?? fx.vm ?? fx;

execFileSync("npx", ["vite", "build", "--logLevel", "error", "--outDir", "test/_live"],
  { stdio: "ignore", env: { ...process.env, VM_IIFE: "1" } });
const code = readFileSync("test/_live/app.iife.js", "utf8");
const css = readFileSync(`dist/assets/${readdirSync("dist/assets").find((f) => f.endsWith(".css"))}`, "utf8");

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
const results = [];
const check = (name, ok, detail = "") => results.push([ok, name, detail]);
const title = (w) => w.document.querySelector("#content .dtitle")?.textContent.trim();
const activeTab = (w) => w.document.querySelector("#content .tab.active")?.textContent.trim();
const click = (w, el) => el?.dispatchEvent(new w.MouseEvent("click", { bubbles: true }));
const wait = (w, ms) => new Promise((r) => w.setTimeout(r, ms));

const node = (await (await fetch(base + "/connections")).json())[0].name;
const conn = (await (await fetch(base + "/connections")).json())[0].id;
const net = (await (await fetch(`${base}/proxy/${conn}/networks`)).json())[0]?.name;
const pool = (await (await fetch(`${base}/proxy/${conn}/pools`)).json())[0]?.name;
const enc = encodeURIComponent;

try {
  const reloads = [
    ["/", (w) => title(w) === "Datacenter" && /Dashboard/.test(activeTab(w))],
    ["/guests", (w) => title(w) === "Datacenter" && /Guests/.test(activeTab(w))],
    [`/${enc(node)}`, (w) => title(w) === node && /Dashboard/.test(activeTab(w))],
    [`/${enc(node)}/networks`, (w) => /Networks/.test(activeTab(w))],
    [`/${enc(node)}/storage`, (w) => /Storage/.test(activeTab(w))],
    [`/${enc(node)}/vms/${enc(vmName)}`, (w) => /Summary/.test(activeTab(w))],
    [`/${enc(node)}/vms/${enc(vmName)}/console`, (w) => /Console/.test(activeTab(w))],
    [`/${enc(node)}/vms/${enc(vmName)}/hardware`, (w) => /Hardware/.test(activeTab(w))],
    ["/no-such-node/vms/x", (w) => title(w) === "Datacenter" && w.location.pathname === "/"],
    [`/${enc(node)}/networks/gone-net-x`, (w) => w.location.pathname === `/${enc(node)}/networks` && /Networks/.test(activeTab(w))],
    [`/${enc(node)}/storage/gone-pool-x`, (w) => w.location.pathname === `/${enc(node)}/storage` && /Storage/.test(activeTab(w))],
  ];
  if (net) reloads.push([`/${enc(node)}/networks/${enc(net)}`, (w) => title(w) === net]);
  if (pool) reloads.push([`/${enc(node)}/storage/${enc(pool)}`, (w) => title(w) === pool]);
  for (const [path, ok] of reloads) {
    const w = await page(path);
    check(`reload ${path}`, ok(w), `title=${title(w)} tab=${activeTab(w)} url=${w.location.pathname}`);
    w.close();
  }

  const w = await page("/");
  click(w, w.document.querySelector(".tree-row[data-conn]")); await wait(w, 400);
  check("clicking a node writes /<node>", w.location.pathname === `/${enc(node)}`, w.location.pathname);
  click(w, w.document.querySelector('[data-tab="storage"]')); await wait(w, 800);
  check("a tab writes /<node>/storage", w.location.pathname === `/${enc(node)}/storage`, w.location.pathname);
  if (pool) {
    const card = w.document.querySelector(`#pools-list .storage-card[data-pool-open="${pool}"]`);
    check("pool cards are openable (hover highlight applies)", !!card?.hasAttribute("data-open"));
    click(w, card?.querySelector(".storage-card-title")); await wait(w, 800);
    check("a pool card opens its pool page", w.location.pathname === `/${enc(node)}/storage/${enc(pool)}` && title(w) === pool, w.location.pathname);
    w.history.back(); await wait(w, 800);
    check("Back restores the storage tab", w.location.pathname === `/${enc(node)}/storage` && /Storage/.test(activeTab(w)), w.location.pathname);
    w.history.forward(); await wait(w, 800);
    check("Forward restores the pool page", title(w) === pool, `${w.location.pathname} ${title(w)}`);
  }
  if (net) {
    click(w, w.document.querySelector(".tree-row[data-conn]")); await wait(w, 400);
    click(w, w.document.querySelector('[data-tab="networks"]')); await wait(w, 800);
    const card = w.document.querySelector(`#nets-list .storage-card[data-net-open="${net}"]`);
    const before = w.location.pathname;
    click(w, card?.querySelector("[data-net-edit]")); await wait(w, 300);
    check("a card's own button does not navigate", w.location.pathname === before, w.location.pathname);
    w.document.querySelectorAll(".modal-backdrop, .modal").forEach((m) => m.remove());
    click(w, card?.querySelector(".specs")); await wait(w, 800);
    check("a network card opens its network page", w.location.pathname === `/${enc(node)}/networks/${enc(net)}` && title(w) === net, w.location.pathname);
  }
  const len = w.history.length; await wait(w, 2500);
  check("the 1s poll adds no history entries", w.history.length === len, `${len} -> ${w.history.length}`);
  click(w, w.document.querySelector("#dc-row")); await wait(w, 1500);
  if (pool) {
    click(w, w.document.querySelector("#dc-dash [data-dash-pool]")); await wait(w, 800);
    check("a dashboard pool card opens its pool page", /\/storage\/[^/]+$/.test(w.location.pathname), w.location.pathname);
    click(w, w.document.querySelector("#dc-row")); await wait(w, 1500);
  }
  if (net) {
    click(w, w.document.querySelector("#dc-dash [data-dash-net]")); await wait(w, 800);
    check("a dashboard network row opens its network page", /\/networks\/[^/]+$/.test(w.location.pathname), w.location.pathname);
  }
  w.close();
} finally {
  await removeTestVm(base, fx);
}

for (const [ok, name, d] of results) console.log(`${ok ? "\u2714" : "\u2716"} ${name}${ok ? "" : `  — ${d}`}`);
const failed = results.filter(([ok]) => !ok);
if (failed.length) {
  console.error(`\n${failed.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall routing checks passed");
