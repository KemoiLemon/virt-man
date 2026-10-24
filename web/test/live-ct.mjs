// Live container checks: the SPA against a running backend and a daemon with
// the native container runtime. Makes a small Alpine container from the
// catalog template (downloading the template if the pool lacks it), drives the
// real app in jsdom over it, then deletes it -- rootfs included, template kept.
//
//   node test/live-ct.mjs [base]      (default http://127.0.0.1:8799)
//
// Needs a built `dist/` (for the stylesheet) and a daemon running as root.
//
// Checks:
//   - a container is named by its UUID, with what was typed as its title
//   - the sidebar lists containers below the VMs and above networks and pools
//   - the server and datacenter both have a Containers tab listing it, and the
//     Datacenter card names its own server
//   - its page is addressed by UUID and has a Hardware tab (CPU, memory, swap,
//     disk) that applies, and a text console that answers -- and shows output
//     nobody typed for
//   - the admin page has a Containers section, tabbed by daemon, listing it
//   - a VM route cannot reach it, and a VM-only action is refused
//   - it gets an address on the network the daemon gave it (read from the
//     network's hosts: the supervisor reserves one; no DHCP exchange is needed)
//   - rename sets its title and keeps its id; autostart toggles
//   - a NIC added to the running container says it waits for the next start
//   - deleting it removes its rootfs and keeps the template

import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";
import { ensureTestNetwork, ensureTestPool, removeTestNetwork, removeTestPool } from "./_fixture.mjs";

const base = process.argv[2] ?? "http://127.0.0.1:8799";
const results = [];
const check = (name, ok, detail = "") => {
  results.push([ok, name, detail]);
  console.log(`${ok ? "✔" : "✖"} ${name}${ok || !detail ? "" : ` — ${detail}`}`);
};
const j = async (method, path, body) => {
  const r = await fetch(base + path, {
    method,
    headers: body ? { "content-type": "application/json" } : {},
    body: body ? JSON.stringify(body) : undefined,
  });
  const t = await r.text();
  let v = null;
  try { v = JSON.parse(t); } catch { v = t; }
  return { status: r.status, body: v };
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

const conns = (await j("GET", "/connections")).body;
if (!Array.isArray(conns) || !conns.length) {
  console.log("✖ the web server has no daemon connection: nothing to test");
  process.exit(1);
}
const conn = conns[0].id;
const node = conns[0].name;
const P = `/proxy/${conn}`;

const listed = await j("GET", `${P}/containers`);
if (listed.status !== 200) {
  console.log(`✖ the daemon does not list containers (${listed.status}): ${JSON.stringify(listed.body)}`);
  process.exit(1);
}

// ------------------------------------------------------------ a template
const poolFx = await ensureTestPool(base, { conn });
const pool = poolFx.name;
const vols = async () => (await j("GET", `${P}/pools/${encodeURIComponent(pool)}/volumes`)).body;
let tpl = (await vols()).find((v) => v.content === "vztmpl" && /^alpine/.test(v.name));
if (!tpl) {
  const cat = (await j("GET", `${P}/templates/catalog`)).body;
  const alp = (cat.templates ?? []).filter((t) => t.os === "alpine" && /^3\.\d+$/.test(t.release)).at(-1);
  check("the catalog offers an Alpine template", !!alp, JSON.stringify(cat.errors ?? []));
  if (alp) {
    const r = await j("POST", `${P}/pools/${encodeURIComponent(pool)}/templates`, { id: alp.id });
    check("a catalog template can be downloaded", r.status === 200, JSON.stringify(r.body));
    for (let i = 0; i < 180 && !tpl; i++) {
      await sleep(1000);
      tpl = (await vols()).find((v) => v.name === alp.file);
    }
  }
}
check("an Alpine template is in the pool, listed as a CT template", tpl?.content === "vztmpl", tpl?.name);
if (!tpl) {
  await removeTestPool(base, poolFx);
  process.exit(1);
}

// ------------------------------------------------------------ the container
// A container attaches to a network's bridge, like a VM. The fixture supplies
// one, creating it when the host has none.
const net = await ensureTestNetwork(base);
const TITLE = "live-ct suite";
const create = await j("POST", `${P}/containers`, {
  title: TITLE,
  template: { pool, volume: tpl.name },
  rootfs: { pool, size_gb: 1 },
  memory_mb: 128,
  vcpus: 1,
  networks: [net.name],
  // A root password, so the suite can log in on the console (a template's
  // root is locked, and a login prompt runs nothing typed at it).
  password: "live-ct-suite",
  start: true,
});
check("a container is created from the template and started", create.status === 200 && create.body.started,
  JSON.stringify(create.body));
const ct = create.body?.name;
check("it is named by its UUID", UUID.test(ct ?? "") && create.body.uuid === ct, JSON.stringify(create.body));

try {
  const detail = (await j("GET", `${P}/containers/${ct}`)).body;
  check("it is unprivileged by default", detail.privileged === false, JSON.stringify(detail));
  check("what was typed is its title", detail.title === TITLE, JSON.stringify(detail));
  check("it is running, spelled as a VM's state is", detail.state === "running" && detail.active === true,
    JSON.stringify(detail));
  check("its title reaches it too", (await j("GET", `${P}/containers/${encodeURIComponent(TITLE)}`)).body?.name === ct);
  check("a VM route does not reach a container", (await j("GET", `${P}/domains/${ct}`)).status !== 200);
  check("a VM-only action on it is refused", (await j("POST", `${P}/containers/${ct}/reset`)).status >= 400);

  const hostsUrl = `${P}/networks/${encodeURIComponent(net.name)}/hosts`;
  let ip = null;
  for (let i = 0; i < 40 && !ip; i++) {
    await sleep(1000);
    ip = (await j("GET", hostsUrl)).body.hosts?.find((h) => h.vm === ct)?.ip ?? null;
  }
  check("it is given an address on the network", !!ip, `on ${net.title}`);

  // ---------------------------------------------------------- the SPA
  execFileSync("npx", ["vite", "build", "--logLevel", "error", "--outDir", "test/_live"],
    { stdio: "ignore", env: { ...process.env, VM_IIFE: "1" } });
  const code = readFileSync("test/_live/app.iife.js", "utf8");
  const css = readFileSync(`dist/assets/${readdirSync("dist/assets").find((f) => f.endsWith(".css"))}`, "utf8");
  const page = async (path) => {
    const vc = new VirtualConsole();
    vc.on("jsdomError", (e) => { if (!/not implemented|canvas|WebSocket|matchMedia/i.test(e.message)) console.error("jsdom:", e.message); });
    const dom = new JSDOM(`<!doctype html><html><head><style>${css}</style></head><body><div id="app"></div></body></html>`,
      { url: base + path, runScripts: "dangerously", pretendToBeVisual: true, virtualConsole: vc });
    const w = dom.window;
    w.fetch = (i, init) => fetch(new URL(typeof i === "string" ? i : i.url, base).toString(), init);
    w.ResizeObserver ??= class { observe() {} unobserve() {} disconnect() {} };
    w.eval(code);
    await new Promise((r) => w.setTimeout(r, 2500));
    return w;
  };
  const enc = encodeURIComponent;

  let w = await page(`/${enc(node)}/containers`);
  const rows = [...w.document.querySelectorAll("#tree .tree-row")];
  const kind = (r) => r.dataset.vm ? "vm" : r.dataset.ct ? "ct" : r.dataset.net ? "net" : r.dataset.pool ? "pool" : "";
  const order = rows.map(kind).filter(Boolean);
  const firstCt = order.indexOf("ct");
  const lastVm = order.lastIndexOf("vm");
  const firstNet = order.findIndex((k) => k === "net" || k === "pool");
  check("the sidebar lists containers below VMs and above networks and pools",
    firstCt >= 0 && firstCt > lastVm && (firstNet < 0 || firstCt < firstNet), order.join(","));
  check("the sidebar row shows its title",
    w.document.querySelector(`#tree [data-ct="${ct}"] .tree-label`)?.textContent.trim() === TITLE);
  check("the server has a Containers tab, and it is open",
    /Containers/.test(w.document.querySelector("#content .tab.active")?.textContent ?? ""));
  check("the Containers tab lists it", !!w.document.querySelector(`#cts-list [data-dc-ct="${ct}"]`));
  w.close();

  w = await page("/containers");
  const card = w.document.querySelector(`[data-dc-ct="${ct}"]`);
  check("the datacenter has a Containers tab listing it",
    /Containers/.test(w.document.querySelector("#content .tab.active")?.textContent ?? "") && !!card);
  check("…and its card names its own server", card?.dataset.dcCtConn === conn, card?.dataset.dcCtConn);
  // Opening it from the Datacenter switches to its page, addressed by UUID.
  card?.click();
  await new Promise((r) => w.setTimeout(r, 1500));
  check("opening the card lands on its page, by UUID",
    w.location.pathname === `/${enc(node)}/containers/${ct}`, w.location.pathname);
  w.close();

  w = await page(`/${enc(node)}/containers/${ct}`);
  const tabs = [...w.document.querySelectorAll("#content [data-vmtab]")].map((t) => t.dataset.vmtab);
  check("its page has a Hardware tab", tabs.includes("hardware"), tabs.join(","));
  const shown = w.document.querySelector("#content .dtitle")?.textContent.trim();
  check("its page names it", shown === TITLE, `shows ${JSON.stringify(shown)}`);
  check("its page offers no virt-viewer and no reset",
    !w.document.querySelector("#vm-vv") && !w.document.querySelector("#vm-reset"));
  w.close();

  w = await page(`/${enc(node)}/containers/${ct}/hardware`);
  check("its Hardware tab is the resources editor",
    !!w.document.querySelector("#cthw-cpu") && !!w.document.querySelector("#cthw-mem-unit"));
  w.close();

  w = await page("/admin/containers");
  const adminText = w.document.querySelector("#admin-area")?.textContent ?? "";
  check("the admin page has a Containers section, tabbed by daemon, listing it",
    !!w.document.querySelector("#admin-area [data-res-tab]") && adminText.includes(TITLE), adminText.slice(0, 200));
  w.close();

  // The console, through the proxy, as the terminal uses it.
  const c = await j("POST", `${P}/containers/${ct}/console/connect`, { cols: 100, rows: 30 });
  check("its console opens", c.status === 200, JSON.stringify(c.body));
  let off = c.body?.offset ?? 0;
  // The browser tells the daemon its terminal size. Set it before login so the
  // shell inherits it, then check from inside: apt and every full-screen
  // program redraw the bottom line instead of scrolling when the two disagree.
  const rz = await j("POST", `${P}/containers/${ct}/console/resize`, { cols: 100, rows: 30 });
  check("the console accepts the browser's terminal size", rz.status === 200, JSON.stringify(rz.body));

  const type = (t) => j("POST", `${P}/containers/${ct}/console/write`, { data: Buffer.from(t).toString("base64") });
  const until = async (re, ms) => {
    let buf = "";
    const t = Date.now();
    while (!re.test(buf) && Date.now() - t < ms) {
      const r = (await j("GET", `${P}/containers/${ct}/console/read?after=${off}`)).body;
      buf += Buffer.from(r.data ?? "", "base64").toString();
      off = r.offset ?? off;
      if (r.closed) break;
    }
    return buf;
  };
  await sleep(1500);
  await type("\n");
  let seen = await until(/(login:|#)\s*$/, 8000);
  if (/login:\s*$/.test(seen)) {
    await type("root\n");
    seen += await until(/[Pp]assword:\s*$/, 5000);
    await type("live-ct-suite\n");
    seen += await until(/#\s*$/, 8000);
  }
  await type("echo vm-$((6*7))\n");
  seen += await until(/vm-42/, 6000);
  check("the console answers typed input", seen.includes("vm-42"), JSON.stringify(seen.slice(-200)));
  check("no getty spam on the console", !/can't open \/dev\/tty\d/.test(seen), JSON.stringify(seen.slice(-200)));

  // The container's own terminal must report what the browser asked for: this
  // is the value apt reads.
  await type("stty size\n");
  const sized = await until(/30 100/, 6000);
  check("…and the container's terminal adopts it", /30 100/.test(sized), JSON.stringify(sized.slice(-120)));

  // Output the container writes by itself must arrive without a keystroke.
  await type("for i in 1 2 3; do sleep 1; echo tick-$i; done\n");
  const t0 = Date.now();
  const at = [];
  let late = "";
  while (at.length < 3 && Date.now() - t0 < 10000) {
    const r = (await j("GET", `${P}/containers/${ct}/console/read?after=${off}`)).body;
    late += Buffer.from(r.data ?? "", "base64").toString();
    off = r.offset ?? off;
    while (at.length < 3 && late.includes(`tick-${at.length + 1}\r`)) at.push(Date.now() - t0);
  }
  check("output the container writes by itself arrives with no input, as it is written",
    at.length === 3 && at[2] - at[0] > 1500,
    `ticks at ${at.join(", ")} ms: ${JSON.stringify(late.slice(-120))}`);
  await j("POST", `${P}/containers/${ct}/console/disconnect`);

  // Hardware: read, change, read back; a shrink is refused. The native
  // runtime applies memory, swap, CPU limit and vCPUs to the running
  // container, so nothing should be left pending.
  const hw0 = (await j("GET", `${P}/containers/${ct}/hardware`)).body;
  const hw1 = { ...hw0, vcpus: 2, cpu_limit_pct: 50, swap_mb: 64, disk_gb: hw0.disk_gb + 1 };
  const put = await j("PUT", `${P}/containers/${ct}/hardware`, hw1);
  check("hardware changes are accepted, and the disk grows at once",
    put.status === 200 && put.body.disk_grown === true, JSON.stringify(put.body));
  check("…applied to the running container, none pending",
    Array.isArray(put.body?.pending) && put.body.pending.length === 0, JSON.stringify(put.body?.pending));
  const hw2 = (await j("GET", `${P}/containers/${ct}/hardware`)).body;
  check("…and read back as set", JSON.stringify(hw2) === JSON.stringify(hw1), `${JSON.stringify(hw2)}`);
  check("the root disk cannot shrink",
    (await j("PUT", `${P}/containers/${ct}/hardware`, { ...hw1, disk_gb: hw0.disk_gb })).status === 400);

  // Rename sets the title; the id (the URL and every stored key) stays.
  const rn = await j("POST", `${P}/containers/${ct}/rename`, { name: "live-ct renamed" });
  const afterRename = (await j("GET", `${P}/containers/${ct}`)).body;
  check("rename sets its title and keeps its id",
    rn.status === 200 && afterRename.name === ct && afterRename.title === "live-ct renamed", JSON.stringify(afterRename));

  // Start with the host: on, read back, off.
  const a1 = await j("POST", `${P}/containers/${ct}/autostart`, { autostart: true });
  const on = (await j("GET", `${P}/containers/${ct}`)).body.autostart;
  await j("POST", `${P}/containers/${ct}/autostart`, { autostart: false });
  const off2 = (await j("GET", `${P}/containers/${ct}`)).body.autostart;
  check("autostart turns on and off", a1.status === 200 && on === true && off2 === false, `${on} then ${off2}`);

  // A NIC added to the running container waits for its next start, and the
  // response says so rather than leaving the UI to guess.
  const nic = await j("POST", `${P}/containers/${ct}/interfaces`, { network: net.name });
  check("a NIC added while running is accepted and marked pending",
    nic.status === 200 && nic.body.pending === true && !!nic.body.mac, JSON.stringify(nic.body));
  if (nic.body?.mac) {
    const del = await j("DELETE", `${P}/containers/${ct}/interfaces/${encodeURIComponent(nic.body.mac)}`);
    check("…and can be removed again", del.status === 200, JSON.stringify(del.body));
  }
} finally {
  if (ct) {
    // Deleting a running container stops it first.
    const d = await j("DELETE", `${P}/containers/${ct}`);
    check("deleting it (running) stops it and removes its rootfs",
      d.status === 200 && d.body.stopped === true && d.body.removed?.length === 1, JSON.stringify(d.body));
    const after = await vols();
    check("…and keeps the template", after.some((v) => v.name === tpl.name));
    check("…and no ct-<uuid> volume is left", !after.some((v) => v.name === `ct-${ct}.raw`));
  }
  await removeTestNetwork(base, net);
  // The template stays in a reused pool; a pool made for this run goes.
  await removeTestPool(base, poolFx);
}

const failed = results.filter(([ok]) => !ok);
console.log(failed.length ? `\n${failed.length} check(s) failed` : "\nall container checks passed");
process.exit(failed.length ? 1 : 0);
