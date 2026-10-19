// The admin panel can create and delete resources: the Storage Pools page
// offers a "Storage pool" create button, and every resource row (VM, network,
// pool, volume) carries a delete action. Drives the real app in jsdom
// against a running backend.
//
// Pools are native directories now, so the create flow is checked end to end:
// it defines one at a throwaway test directory through the real dialog, and the
// delete removes the definition and leaves the files. Nothing assumes a libvirt
// pool (or any pool at all) already exists on the host.
//
//   node test/live-admin-res.mjs [base]   (default http://127.0.0.1:8799)

import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { join } from "node:path";
import { JSDOM, VirtualConsole } from "jsdom";

const base = process.argv[2] ?? "http://127.0.0.1:8799";

execFileSync("npx", ["vite", "build", "--logLevel", "error", "--outDir", "test/_live"],
  { stdio: "ignore", env: { ...process.env, VM_IIFE: "1" } });
const code = readFileSync("test/_live/app.iife.js", "utf8");
const css = readFileSync(`dist/assets/${readdirSync("dist/assets").find((f) => f.endsWith(".css"))}`, "utf8");

const results = [];
const check = (name, ok, detail = "") => results.push([ok, name, detail]);

const conns = await (await fetch(`${base}/connections`)).json();
const conn = conns[0]?.id;
if (!conn) {
  console.error("no saved server on this backend — add one before running live suites");
  process.exit(2);
}
const pools = async () => (await fetch(`${base}/proxy/${conn}/pools`)).json().catch(() => []);

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
const wait = (w, ms) => new Promise((r) => w.setTimeout(r, ms));
const click = (w, el) => el?.dispatchEvent(new w.MouseEvent("click", { bubbles: true }));

// A directory the daemon accepts as a pool root: inside the repo, and not in
// `/tmp` or `/var/tmp`, which are reserved by the daemon.
const dir = join(process.cwd(), "test", "_pools", `admin-${process.pid}-${Date.now()}`);
mkdirSync(dir, { recursive: true });
const title = `vmtest-admin-pool-${Date.now()}`;

try {
  // ---- Storage Pools: the create dialog defines a native dir pool.
  const w = await page("/admin/pools");
  const doc = w.document;
  const create = doc.querySelector("#res-new");
  check("pools: a create button is rendered", !!create, create ? "" : "#res-new missing");
  check("pools: the create button says 'Storage pool'",
    !!create && /storage pool/i.test(create.textContent), create?.textContent?.trim() ?? "");

  click(w, create);
  await wait(w, 300);
  const nameField = doc.querySelector("#pool-name");
  const targetField = doc.querySelector("#pool-target");
  check("pools: the create dialog asks for a name and a target directory",
    !!(nameField && targetField), nameField ? "" : "no #pool-name/#pool-target");
  if (nameField && targetField) {
    nameField.value = title;
    targetField.value = dir;
    click(w, doc.querySelector("#pool-create"));
  }

  let made = null;
  for (let i = 0; i < 40 && !made; i++) {
    await wait(w, 250);
    made = (await pools()).find((p) => p.title === title) ?? null;
  }
  check("pools: the admin create flow defines an active dir pool", !!made,
    made ? "" : `no pool titled ${title} — ${JSON.stringify(await pools())}`);
  check("pools: it is named by a UUID and keeps the typed title",
    !!made && made.name === made.uuid && made.title === title,
    made ? `${made.name}/${made.title}` : "");
  w.close();

  if (made) {
    // ---- The card, and a delete that asks before acting.
    const w2 = await page("/admin/pools");
    const d2 = w2.document;
    const card = [...d2.querySelectorAll(".storage-card")]
      .find((c) => c.querySelector(".storage-card-title")?.textContent.trim() === title);
    check("pools: the created pool has a card", !!card, card ? "" : "card missing");
    const del = card?.querySelector("[data-del-res]");
    check("pools: a pool card has a delete action", !!del);
    click(w2, del);
    await wait(w2, 300);
    const modal = d2.querySelector(".modal-back.open");
    check("pools: delete opens a confirmation", !!modal, modal ? "" : "no .modal-back.open");
    check("pools: the confirmation is a delete prompt",
      !!modal && /delete/i.test(modal.textContent), modal?.textContent?.slice(0, 60) ?? "");
    click(w2, modal?.querySelector("#ask-no"));
    await wait(w2, 400);
    check("pools: cancelling leaves the pool defined",
      (await pools()).some((p) => p.name === made.name));

    // The delete removes the definition, never the files in it.
    click(w2, card?.querySelector("[data-del-res]"));
    await wait(w2, 300);
    click(w2, d2.querySelector(".modal-back.open #ask-yes"));
    let gone = false;
    for (let i = 0; i < 40 && !gone; i++) {
      await wait(w2, 250);
      gone = !(await pools()).some((p) => p.name === made.name);
    }
    check("pools: deleting removes the definition", gone);
    check("pools: deleting a pool leaves its files", existsSync(dir));

    // The remaining cards are the host's own pools, if it has any.
    const cards = [...d2.querySelectorAll(".storage-card")];
    check("pools: every pool card has a delete action",
      cards.every((c) => !!c.querySelector("[data-del-res]")), `${cards.length} card(s)`);
    // Volumes, if any, have their own delete.
    const volRows = [...d2.querySelectorAll(".storage-card table.grid tbody tr, .storage-card table.grid tr")].slice(1);
    if (volRows.length) {
      check("pools: a volume row has a delete action", !!volRows[0].querySelector("[data-del-res]"));
    }
    w2.close();
  }
} finally {
  // Best effort, in case a check failed between create and delete.
  const left = (await pools()).find((p) => p.title === title);
  if (left) {
    await fetch(`${base}/proxy/${conn}/pools/${encodeURIComponent(left.name)}`, { method: "DELETE" })
      .catch(() => undefined);
  }
  rmSync(dir, { recursive: true, force: true });
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
