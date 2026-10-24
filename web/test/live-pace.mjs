// How often does the page visibly change while it just sits there?
//
// Boots the real app in jsdom against a stub daemon that answers with random
// latency (like a remote host) and changing figures, then records every burst
// of DOM changes in the sidebar and the content pane. A "burst" is the changes
// of one task, which is one frame in a browser; two bursts a few ms apart are
// the quick-succession updates a person sees as flicker.
//
//   node test/live-pace.mjs [server-binary] [seconds]

import { execFileSync, spawn } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import http from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { JSDOM, VirtualConsole } from "jsdom";

const bin = process.argv[2] ?? "web/server/target/debug/virt-man-web";
const seconds = Number(process.argv[3] ?? 14);
execFileSync("npx", ["vite", "build", "--logLevel", "error", "--outDir", "test/_live"],
  { stdio: "ignore", env: { ...process.env, VM_IIFE: "1" } });
const code = readFileSync("test/_live/app.iife.js", "utf8");

let tick = 0;
const LAT = Number(process.env.LAT ?? 600);
const started = [];
const U = "11111111-1111-4111-8111-111111111111";
const stub = http.createServer((req, res) => {
  const path = req.url.split("?")[0];
  tick++;
  if (path === "/host/stats" || path === "/domains") started.push(Date.now());
  const now = Date.now();
  const host = { cpu_total: 1000 + tick * 10, cpu_idle: 800 + tick * 7, mem_total_kib: 8e6, mem_available_kib: 4e6 - tick * 100, cpus: 4, sampled_at_ms: now };
  const vm = { name: "vm1", uuid: U, title: "vm1", state: "running", vcpus: 2, memory_kib: 1048576, id: 1 };
  const body =
    path === "/host/stats" ? host
    : path === "/metrics" ? { host, pools: [], domains: [] }
    : path === "/domains" ? [vm]
    : path === `/domains/${U}/stats` || path === "/domains/vm1/stats"
      ? { name: "vm1", state: "running", cpu_time_ns: tick * 1e7, vcpus: 2, memory_kib: 1048576, max_memory_kib: 1048576,
          rss_kib: 500000 + tick, usable_kib: null, host: { cpus: 4, memory_kib: 8e6, mhz: 3000, model: "x" }, sampled_at_ms: now }
    : path === "/health" || path === "/version" ? { ok: true, version: "stub" }
    : [];
  req.resume();
  req.on("end", () => setTimeout(() => {
    res.setHeader("content-type", "application/json");
    res.end(JSON.stringify(body));
  }, 20 + Math.random() * LAT));
});
await new Promise((r) => stub.listen(0, "127.0.0.1", r));
const stubUrl = `http://127.0.0.1:${stub.address().port}`;

const data = mkdtempSync(join(tmpdir(), "vm-pace-"));
const port = 8800 + Math.floor(Math.random() * 90);
const base = `http://127.0.0.1:${port}`;
const server = spawn(bin, [], {
  env: { ...process.env, VIRT_MAN_WEB_LOOPBACK_HTTP: "1", VIRT_MAN_WEB_BIND: "127.0.0.1",
    VIRT_MAN_WEB_PORT: String(port), VIRT_MAN_WEB_DATA: data, VIRT_MAN_WEB_DIST: join(process.cwd(), "dist") },
  stdio: "ignore",
});
let w;
try {
  for (let i = 0; i < 50; i++) { try { await fetch(`${base}/health`); break; } catch { await new Promise((r) => setTimeout(r, 100)); } }
  await fetch(`${base}/connections`, { method: "POST", headers: { "content-type": "application/json" },
    body: JSON.stringify({ name: "stub", url: stubUrl, token: "t" }) });
  const conn = (await (await fetch(`${base}/connections`)).json())[0].id;

  for (const [label, path] of [["datacenter", "/"], ["server dashboard", `/${conn}`], ["server vms", `/${conn}/vms`], ["vm summary", `/${conn}/vms/${U}`]]) {
    w?.close();
    const dom = new JSDOM(`<!doctype html><html><body><div id="app"></div></body></html>`,
      { url: base + path, runScripts: "dangerously", pretendToBeVisual: true, virtualConsole: new VirtualConsole() });
    w = dom.window;
    w.fetch = (i, init) => fetch(new URL(typeof i === "string" ? i : i.url, base).toString(), init);
    w.ResizeObserver ??= class { observe() {} unobserve() {} disconnect() {} };
    w.eval(code);
    await new Promise((r) => setTimeout(r, 3000 + LAT * 2)); // settle first paint (boot reads are not paced)
    const bursts = [];
    const t0 = Date.now();
    const mo = new w.MutationObserver((recs) => {
      const where = new Set(recs.map((r) => (r.target.closest?.("#tree") ? "tree" : r.target.closest?.("#content") ? "content" : "other")));
      bursts.push([Date.now() - t0, [...where].join("+"), recs.length]);
    });
    mo.observe(w.document.body, { childList: true, subtree: true, characterData: true, attributes: true });
    // A person is using the page: any input counts as the user acting.
    const poke = process.env.POKE ? setInterval(() => w.document.body.dispatchEvent(new w.Event("wheel", { bubbles: true })), Number(process.env.POKE)) : 0;
    await new Promise((r) => setTimeout(r, seconds * 1000));
    clearInterval(poke);
    mo.disconnect();
    // Merge changes of one task (<=15 ms apart) into one visible update.
    const updates = [];
    for (const b of bursts) {
      const last = updates.at(-1);
      if (last && b[0] - last.at <= 15) { last.where.add(b[1]); last.n += b[2]; last.end = b[0]; }
      else updates.push({ at: b[0], end: b[0], where: new Set([b[1]]), n: b[2] });
    }
    const gaps = updates.slice(1).map((u, i) => u.at - updates[i].at);
    const quick = gaps.filter((g) => g < 500).length;
    console.log(`\n${label}: ${updates.length} updates in ${seconds}s, ${quick} within 500 ms of the previous one`);
    console.log("  at (ms):", updates.map((u) => `${u.at}[${[...u.where].join("+")}]`).join("  "));
  }
} finally {
  w?.close();
  server.kill();
  stub.close();
}
