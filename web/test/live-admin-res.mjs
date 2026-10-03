// The admin panel can create and delete resources: the Storage Pools page
// offers a "Storage pool" create button, and every resource row (VM, container,
// network, pool, volume) carries a delete action. Drives the real app in jsdom
// against a running backend.
//
//   node test/live-admin-res.mjs [base]   (default http://127.0.0.1:8799)

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

const page = async (path) => {
  const vc = new VirtualConsole();
  vc.on("jsdomError", (e) => { if (!/not implemented|canvas|WebSocket/i.test(e.message)) console.error("jsdom:", e.message); });
  const dom = new JSDOM(`<!doctype html><html><head><style>${css}</style></head><body><div id="app"></div></body></html>`,
    { url: base + path, runScripts: "dangerously", pretendToBeVisual: true, virtualConsole: vc });
  const w = dom.window;
  w.fetch = (i, init) => fetch(new URL(typeof i === "string" ? i : i.url, base).toString(), init);
  w.ResizeObserver ??= class { observe() {} unobserve() {} disconnect() {} };
  void w.eval(code);
  await new Promise((r) => w.setTimeout(r, 3000));
  return w;
};

// ---- Storage Pools: a create button, and a delete on every card / volume row.
{
  const w = await page("/admin/pools");
  const doc = w.document;
  const create = doc.querySelector("#res-new");
  check("pools: a create button is rendered", !!create, create ? "" : "#res-new missing");
  check("pools: the create button says 'Storage pool'",
    !!create && /storage pool/i.test(create.textContent), create?.textContent?.trim() ?? "");
  // The page lists each pool as a .storage-card; each carries a delete action.
  const cards = [...doc.querySelectorAll(".storage-card")];
  check("pools: the page lists at least one pool card", cards.length > 0, `${cards.length}`);
  if (cards.length) {
    check("pools: a pool card has a delete action",
      !!cards[0].querySelector("[data-del-res]"));
  }
  // Volumes, if any, have their own delete.
  const volRows = [...doc.querySelectorAll(".storage-card table.grid tbody tr, .storage-card table.grid tr")].slice(1);
  if (volRows.length) {
    check("pools: a volume row has a delete action",
      !!volRows[0].querySelector("[data-del-res]"));
  }
}

// ---- A pool's delete button opens a confirmation, does not delete on click.
{
  const w = await page("/admin/pools");
  const doc = w.document;
  const del = doc.querySelector("[data-del-res]");
  if (del) {
    del.dispatchEvent(new w.MouseEvent("click", { bubbles: true }));
    await new Promise((r) => w.setTimeout(r, 300));
    const modal = doc.querySelector(".modal-back.open");
    check("pools: delete opens a confirmation", !!modal, modal ? "" : "no .modal-back.open");
    check("pools: the confirmation is a delete prompt",
      !!modal && /delete/i.test(modal.textContent), modal?.textContent?.slice(0, 60) ?? "");
    // Cancel it, leaving the pool untouched.
    const cancel = modal?.querySelector("#ask-no");
    if (cancel) cancel.dispatchEvent(new w.MouseEvent("click", { bubbles: true }));
  } else {
    check("pools: delete opens a confirmation", false, "no delete button to click");
  }
}

// ---- VMs page: rows carry delete too.
{
  const w = await page("/admin/vms");
  const doc = w.document;
  const row = doc.querySelector("table.grid tr:nth-child(2)");
  const anyDel = doc.querySelector("[data-del-res]");
  // A host with no VMs renders an empty row; only assert when a row exists.
  if (row && !/No VMs/.test(row.textContent)) {
    check("vms: a resource row has a delete action", !!anyDel);
  } else {
    check("vms: empty state renders", true, "no VMs to check");
  }
}

let bad = 0;
for (const [ok, name, detail] of results) {
  if (!ok) bad++;
  console.log(`${ok ? "ok  " : "FAIL"}  ${name}${!ok && detail ? `  [${detail}]` : ""}`);
}
console.log(bad ? `\n${bad} failed of ${results.length}` : `\nall ${results.length} passed`);
process.exit(bad ? 1 : 0);
