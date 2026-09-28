// Empty lists: each says there is nothing and offers one small create button,
// with no create *card* in any grid. Servers are a list, not cards.
//
// Needs no guest and touches no real host: it starts its own throwaway web
// server (own data dir, auth disabled) and points it at a stub daemon that
// answers every list with []. Requires the built dist/ and the server binary.
//
//   node test/live-empty.mjs [server-binary]   (default /usr/local/bin/virt-man-web)

import { execFileSync, spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import http from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { JSDOM, VirtualConsole } from "jsdom";

const bin = process.argv[2] ?? "/usr/local/bin/virt-man-web";
execFileSync("npx", ["vite", "build", "--logLevel", "error", "--outDir", "test/_live"],
  { stdio: "ignore", env: { ...process.env, VM_IIFE: "1" } });
const code = readFileSync("test/_live/app.iife.js", "utf8");

// Stub daemon: empty lists, zero stats.
const stub = http.createServer((req, res) => {
  const path = req.url.split("?")[0];
  const body =
    path === "/host/stats" ? { cpu_total: 1, cpu_idle: 1, mem_total_kib: 1, mem_available_kib: 1, cpus: 1, sampled_at_ms: Date.now() }
    : path === "/metrics" ? { host: { cpu_total: 1, cpu_idle: 1, mem_total_kib: 1, mem_available_kib: 1 }, pools: [], domains: [] }
    : path === "/health" || path === "/version" ? { ok: true, version: "stub" }
    : [];
  // The proxy streams a (possibly empty) body; drain it before answering, or
  // the socket is torn down under the proxy.
  req.resume();
  req.on("end", () => {
    res.setHeader("content-type", "application/json");
    res.end(JSON.stringify(body));
  });
});
await new Promise((r) => stub.listen(0, "127.0.0.1", r));
const stubUrl = `http://127.0.0.1:${stub.address().port}`;

const data = mkdtempSync(join(tmpdir(), "vm-empty-"));
const port = 8700 + Math.floor(Math.random() * 90);
const base = `http://127.0.0.1:${port}`;
const server = spawn(bin, [], {
  env: {
    ...process.env, VIRT_MAN_WEB_LOOPBACK_HTTP: "1", VIRT_MAN_WEB_BIND: "127.0.0.1",
    VIRT_MAN_WEB_PORT: String(port), VIRT_MAN_WEB_DATA: data, VIRT_MAN_WEB_DIST: join(process.cwd(), "dist"),
  },
  stdio: "ignore",
});

const results = [];
const check = (name, ok, detail = "") => results.push([!!ok, name, detail]);
let w;
try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`${base}/health`); break; } catch { await new Promise((r) => setTimeout(r, 100)); }
  }
  const open = async (path) => {
    w?.close();
    const dom = new JSDOM(`<!doctype html><html><body><div id="app"></div></body></html>`,
      { url: base + path, runScripts: "dangerously", pretendToBeVisual: true, virtualConsole: new VirtualConsole() });
    w = dom.window;
    w.fetch = (i, init) => fetch(new URL(typeof i === "string" ? i : i.url, base).toString(), init);
    w.ResizeObserver ??= class { observe() {} unobserve() {} disconnect() {} };
    w.eval(code);
    await new Promise((r) => setTimeout(r, 1800));
    return w.document;
  };
  const noCards = (d) => d.querySelectorAll(".create-card, .net-create-card").length === 0;
  const modalTitle = (d) => d.querySelector(".modal-back.open .modal-head, .modal-back.open h3, .modal-back.open .modal-title")?.textContent.trim() ?? "";

  // No servers at all. Servers are managed on the admin Daemons page now (an
  // old /servers link lands there).
  let d = await open("/servers");
  check("no servers: the Daemons page says so", /No daemons yet/.test(d.querySelector("#admin-area")?.textContent ?? ""),
    d.querySelector("#admin-area")?.textContent);
  d.querySelector("#add-daemon")?.click();
  await new Promise((r) => setTimeout(r, 200));
  check("…and Add daemon opens the Add Server dialog", /Server/i.test(modalTitle(d)), modalTitle(d));
  let empty;

  d = await open("/guests");
  empty = d.querySelector(".empty-create");
  check("no guests: says so, with Create Guest", /No guests available/.test(empty?.textContent ?? "") && empty?.querySelector("#dc-guest-create"), empty?.textContent);
  check("…and no create card", noCards(d));

  // One server, with nothing on it.
  await fetch(`${base}/connections`, { method: "POST", headers: { "content-type": "application/json" },
    body: JSON.stringify({ name: "stub", url: stubUrl, token: "t" }) });
  d = await open("/servers");
  const rows = [...d.querySelectorAll("#admin-area table.grid tr")].filter((r) => r.querySelector("[data-edit-conn]"));
  check("servers are a list: one table row per server", rows.length === 1, rows.length);
  check("…with name, URL and actions", rows[0]?.textContent.includes("stub") && rows[0]?.textContent.includes(stubUrl)
    && rows[0]?.querySelector("[data-edit-conn]") && rows[0]?.querySelector("[data-del-conn]"));
  check("…and no server cards", !d.querySelector(".storage-card[data-open-conn-row]") && noCards(d));

  for (const [tab, what, id, dialog] of [
    ["vms", "VMs", "vm-create-empty", /Create/i],
    ["networks", "networks", "net-create-empty", /Network/i],
    ["storage", "storage pools", "pool-create-empty", /Pool/i],
  ]) {
    d = await open(`/stub/${tab}`);
    empty = d.querySelector(".empty-create");
    check(`no ${what}: says so`, new RegExp(`No ${what} available`).test(empty?.textContent ?? ""), empty?.textContent ?? d.querySelector("#content")?.textContent.slice(0, 120));
    check(`…with a small create button, and no create card`, !!empty?.querySelector(`button#${id}`) && noCards(d));
    empty?.querySelector("button")?.click();
    await new Promise((r) => setTimeout(r, 300));
    check(`…which opens the create dialog`, !!d.querySelector(".modal-back.open") && dialog.test(d.querySelector(".modal-back.open")?.textContent ?? ""),
      d.querySelector(".modal-back.open")?.textContent.slice(0, 60));
  }
} finally {
  w?.close();
  server.kill();
  stub.close();
  rmSync(data, { recursive: true, force: true });
}

for (const [ok, name, detail] of results) console.log(`${ok ? "\u2714" : "\u2716"} ${name}${ok ? "" : `  — ${detail}`}`);
const failed = results.filter(([ok]) => !ok);
if (failed.length) {
  console.error(`\n${failed.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall empty-state checks passed");
