// Operations against a real daemon: virt-viewer files (one-shot relay), a
// daemon going down and coming back, and downloading an ISO from a URL
// straight into a pool.
//
// Needs sudo (defines a SPICE guest, stops/starts virt-man-daemon) and a
// running loopback sidecar. Everything it creates is removed at the end.
//
//   node test/live-ops.mjs [base]      (default http://127.0.0.1:8799)

import { execFileSync, spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { JSDOM, VirtualConsole } from "jsdom";

const base = process.argv[2] ?? "http://127.0.0.1:8799";
const VM = "vmtest-ops-spice";
const VOL = "vmtest-ops-fetch.iso";
execFileSync("npx", ["vite", "build", "--logLevel", "error", "--outDir", "test/_live"],
  { stdio: "ignore", env: { ...process.env, VM_IIFE: "1" } });
const code = readFileSync("test/_live/app.iife.js", "utf8");
const conn = (await (await fetch(base + "/connections")).json())[0];
const sh = (...a) => execFileSync("sudo", a, { stdio: ["ignore", "pipe", "ignore"] }).toString();
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (f, ms) => { const t = Date.now(); while (Date.now() - t < ms) { if (await f()) return true; await wait(100); } return false; };

const results = [];
const check = (name, ok, detail = "") => results.push([!!ok, name, detail]);

// SPICE's first message: a link header the server answers with "REDQ".
const spiceHello = (port) => new Promise((resolve) => {
  const s = net.connect(port, "127.0.0.1");
  const mess = Buffer.alloc(18);
  mess.writeUInt8(1, 4); // channel type: main
  mess.writeUInt32LE(18, 14);
  const head = Buffer.alloc(16);
  head.write("REDQ", 0); head.writeUInt32LE(2, 4); head.writeUInt32LE(2, 8); head.writeUInt32LE(18, 12);
  s.on("connect", () => s.write(Buffer.concat([head, mess])));
  s.once("data", (d) => { resolve(d.subarray(0, 4).toString()); s.destroy(); });
  s.on("error", () => resolve("error"));
  s.on("close", () => resolve("closed"));
  setTimeout(() => { resolve("timeout"); s.destroy(); }, 3000);
});
const refused = (port) => new Promise((resolve) => {
  const s = net.connect(port, "127.0.0.1");
  s.on("connect", () => { s.destroy(); resolve(false); });
  s.on("error", () => resolve(true));
});

const openApp = async (path) => {
  const dom = new JSDOM(`<!doctype html><html><body><div id="app"></div></body></html>`,
    { url: base + path, runScripts: "dangerously", pretendToBeVisual: true, virtualConsole: new VirtualConsole() });
  const w = dom.window;
  w.fetch = (i, init) => fetch(new URL(typeof i === "string" ? i : i.url, base).toString(), init);
  w.ResizeObserver ??= class { observe() {} unobserve() {} disconnect() {} };
  w.eval(code);
  return w;
};

const tmp = mkdtempSync(join(tmpdir(), "vm-ops-"));
let isoServer;
try {
  // ------------------------------------------------------------ virt-viewer
  writeFileSync(join(tmp, "spice.xml"), `<domain type='kvm'><name>${VM}</name><memory unit='MiB'>128</memory><vcpu>1</vcpu>
    <os><type arch='x86_64' machine='q35'>hvm</type></os><features><acpi/><apic/></features>
    <devices><graphics type='spice' autoport='yes' listen='127.0.0.1'/><video><model type='qxl'/></video></devices></domain>`);
  sh("virsh", "define", join(tmp, "spice.xml"));
  sh("virsh", "start", VM);
  const vv = async () => {
    const r = await fetch(`${base}/proxy/${conn.id}/domains/${VM}/console/vv`);
    const body = await r.text();
    return { status: r.status, body, kv: Object.fromEntries(body.split("\n").filter((l) => l.includes("=")).map((l) => l.split(/=(.*)/s).slice(0, 2))) };
  };
  const guestPort = sh("virsh", "domdisplay", "--type", "spice", VM).trim().split(":").pop();
  const f = await vv();
  check("a SPICE guest gets a virt-viewer file", f.status === 200 && f.kv.type === "spice", f.body);
  check("…naming a relay, not the guest's loopback-only listener", f.kv.port && f.kv.port !== guestPort, `${f.kv.port} vs ${guestPort}`);
  check("…and the relay reaches the guest's SPICE server", (await spiceHello(Number(f.kv.port))) === "REDQ");
  check("…exactly once", await refused(Number(f.kv.port)));
  const alpine = await fetch(`${base}/proxy/${conn.id}/domains/${VM}/console/vv?kind=vnc`);
  check("asking for a display the guest lacks is a 404, not a wrong file", alpine.status === 404);

  let w = await openApp(`/${encodeURIComponent(conn.name)}/vms/${VM}`);
  let d = w.document;
  await until(() => d.getElementById("vm-vv") && !d.getElementById("vm-vv").disabled, 6000);
  check("the VM header offers virt-viewer for a running guest", d.getElementById("vm-vv") && !d.getElementById("vm-vv").disabled);
  const row = [...d.querySelectorAll(".tree-row.tree-vm")].find((r) => r.dataset.vm === VM);
  row?.dispatchEvent(new w.MouseEvent("contextmenu", { bubbles: true, clientX: 5, clientY: 5 }));
  check("…and so does its menu", [...d.querySelectorAll(".ctxmenu-item")].some((b) => /virt-viewer/.test(b.textContent) && !b.disabled));
  d.querySelector(".ctxmenu")?.remove();
  w.close();

  // ------------------------------------------------------------ downtime
  w = await openApp(`/${encodeURIComponent(conn.name)}/vms/${VM}`);
  d = w.document;
  await until(() => d.querySelector(".tabbar [data-vmtab]"), 6000);
  sh("systemctl", "stop", "virt-man-daemon");
  let panel = false;
  try {
    panel = await until(() => d.querySelector(".offline-panel"), 15000);
    check("a daemon going down shows one offline panel", panel);
    check("…with a reason a person can act on", /connection refused/.test(d.querySelector(".offline-panel")?.textContent ?? ""),
      d.querySelector(".offline-panel")?.textContent.replace(/\s+/g, " "));
    await wait(3000);
    check("…and never the raw transport error", !/daemon error|error sending request/.test(d.querySelector("#content")?.textContent ?? ""));
    check("the sidebar marks the server offline and greys its stale rows",
      d.querySelector(".tree-row.tree-group.offline") && d.querySelector(".tree-stale .tree-vm"));
  } finally {
    sh("systemctl", "start", "virt-man-daemon");
  }
  check("when it comes back the page reloads by itself",
    await until(() => !d.querySelector(".offline-panel") && d.querySelector(".tabbar [data-vmtab]"), 15000));
  check("…and the sidebar is live again", !d.querySelector(".tree-row.tree-group.offline") && !d.querySelector(".tree-stale"));
  w.close();
  const log = (await (await fetch(`${base}/activity?limit=10`)).json()).entries;
  check("the outage and the recovery are in the activity log",
    log.some((e) => e.username === "system" && /went offline/.test(e.label)) && log.some((e) => e.username === "system" && /back online/.test(e.label)));

  // ------------------------------------------------------------ URL fetch
  writeFileSync(join(tmp, "img.iso"), Buffer.alloc(3_000_000, 7));
  const sha = execFileSync("sha256sum", [join(tmp, "img.iso")]).toString().split(" ")[0];
  isoServer = spawn("python3", ["-m", "http.server", "8782", "--bind", "127.0.0.1"], { cwd: tmp, stdio: "ignore" });
  await until(async () => (await fetch("http://127.0.0.1:8782/img.iso").catch(() => null))?.ok, 5000);
  w = await openApp(`/${encodeURIComponent(conn.name)}/storage/default`);
  d = w.document;
  await until(() => d.getElementById("pool-fetch"), 8000);
  d.getElementById("pool-fetch").click();
  await wait(100);
  d.getElementById("fetch-url").value = "http://127.0.0.1:8782/img.iso";
  d.getElementById("fetch-name").value = VOL;
  d.getElementById("fetch-sha").value = sha;
  d.getElementById("fetch-go").click();
  check("a URL download shows its progress card on the pool page", await until(() => d.querySelector("#pool-fetches [data-fetch-id]"), 5000));
  check("…and ends Stored", await until(() => /Stored/.test(d.querySelector("#pool-fetches .metric-value")?.textContent ?? ""), 30000),
    d.querySelector("#pool-fetches")?.textContent.replace(/\s+/g, " "));
  const onDisk = sh("sha256sum", `/var/lib/libvirt/images/${VOL}`).split(" ")[0];
  check("the stored volume is byte-identical to the source", onDisk === sha);
  w.close();

  const bad = await fetch(`${base}/proxy/${conn.id}/pools/default/fetch`, { method: "POST", headers: { "content-type": "application/json" },
    body: JSON.stringify({ url: "http://127.0.0.1:8782/img.iso", name: "vmtest-ops-bad.iso", sha256: "0".repeat(64) }) });
  const badId = (await bad.json()).id;
  const failed = await until(async () => (await (await fetch(`${base}/proxy/${conn.id}/fetches`)).json()).find((j) => j.id === badId)?.phase === "failed", 20000);
  check("a SHA-256 mismatch fails the download and stores nothing", failed && !sh("virsh", "vol-list", "default").includes("vmtest-ops-bad.iso"));
  const dup = await fetch(`${base}/proxy/${conn.id}/pools/default/fetch`, { method: "POST", headers: { "content-type": "application/json" },
    body: JSON.stringify({ url: "http://127.0.0.1:8782/img.iso", name: VOL }) });
  check("an existing volume is never overwritten", dup.status === 409);
} finally {
  isoServer?.kill();
  try { sh("virsh", "destroy", VM); } catch { /* already off */ }
  try { sh("virsh", "undefine", VM); } catch { /* already gone */ }
  try { sh("virsh", "vol-delete", "--pool", "default", VOL); } catch { /* not created */ }
  try { sh("systemctl", "start", "virt-man-daemon"); } catch { /* running */ }
  rmSync(tmp, { recursive: true, force: true });
}

for (const [ok, name, detail] of results) console.log(`${ok ? "\u2714" : "\u2716"} ${name}${ok ? "" : `  — ${detail}`}`);
const failed = results.filter(([ok]) => !ok);
if (failed.length) {
  console.error(`\n${failed.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall ops checks passed");
