// Live container checks: the SPA against a running backend and a daemon with
// the LXC driver. Makes a small Alpine container from the catalog template
// (downloading the template if the pool lacks it), drives the real app in
// jsdom over it, then deletes it -- rootfs included, template kept.
//
//   node test/live-ct.mjs [base]      (default http://127.0.0.1:8799)
//
// Checks:
//   - the sidebar lists containers below the VMs and above networks and pools
//   - the server and datacenter both have a Containers tab listing it
//   - its page has a Hardware tab (CPU, memory, swap, disk) that applies,
//     and a text console that answers -- and shows output nobody typed for
//   - a VM route cannot reach it, and a VM-only action is refused
//   - it gets an address on the network the daemon gave it (read from the
//     network's hosts: the supervisor reserves one; no DHCP exchange is needed)
//   - deleting it removes its rootfs and keeps the template

import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";
import { ensureTestNetwork, removeTestNetwork } from "./_fixture.mjs";

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

const conns = (await j("GET", "/connections")).body;
const conn = conns[0].id;
const node = conns[0].name;
const P = `/proxy/${conn}`;

const ver = (await j("GET", `${P}/version`)).body;
if (!ver.lxc) {
  console.log("✖ the daemon reports no LXC driver: nothing to test");
  process.exit(1);
}

// ------------------------------------------------------------ a template
const pools = (await j("GET", `${P}/pools`)).body.filter((p) => p.active);
const pool = pools[0].name;
const vols = async () => (await j("GET", `${P}/pools/${encodeURIComponent(pool)}/volumes`)).body;
let tpl = (await vols()).find((v) => v.content === "vztmpl" && /^alpine/.test(v.name));
if (!tpl) {
  const cat = (await j("GET", `${P}/templates/catalog`)).body;
  const alp = cat.templates.filter((t) => t.os === "alpine" && /^3\.\d+$/.test(t.release)).at(-1);
  const r = await j("POST", `${P}/pools/${encodeURIComponent(pool)}/templates`, { id: alp.id });
  check("a catalog template can be downloaded", r.status === 200, JSON.stringify(r.body));
  for (let i = 0; i < 120 && !tpl; i++) {
    await sleep(1000);
    tpl = (await vols()).find((v) => v.name === alp.file);
  }
}
check("an Alpine template is in the pool, listed as a CT template", tpl?.content === "vztmpl", tpl?.name);

// ------------------------------------------------------------ the container
// A container attaches to a network's bridge, like a VM. The fixture supplies
// one, creating it when the host has none.
const net = await ensureTestNetwork(base);
const create = await j("POST", `${P}/containers`, {
  title: "live-ct suite",
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
const ct = create.body.name;

try {
  const detail = (await j("GET", `${P}/containers/${ct}`)).body;
  check("it is unprivileged by default", detail.privileged === false, JSON.stringify(detail));
  check("a VM route does not reach a container", (await j("GET", `${P}/domains/${ct}`)).status !== 200);
  check("a VM-only action on it is refused", (await j("POST", `${P}/containers/${ct}/reset`)).status === 403);

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
  const css = readFileSync(`dist/assets/${readdirSync("dist/assets").find((f) => /^index.*\.css$/.test(f))}`, "utf8");
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
  check("the server has a Containers tab, and it is open",
    /Containers/.test(w.document.querySelector("#content .tab.active")?.textContent ?? ""));
  check("the Containers tab lists it", !!w.document.querySelector(`#cts-list [data-dc-ct="${ct}"]`));
  w.close();

  w = await page("/containers");
  check("the datacenter has a Containers tab listing it",
    /Containers/.test(w.document.querySelector("#content .tab.active")?.textContent ?? "")
      && !!w.document.querySelector(`[data-dc-ct="${ct}"]`));
  w.close();

  w = await page(`/${enc(node)}/containers/${ct}`);
  const tabs = [...w.document.querySelectorAll("#content [data-vmtab]")].map((t) => t.dataset.vmtab);
  check("its page has a Hardware tab", tabs.includes("hardware"), tabs.join(","));
  const shown = w.document.querySelector("#content .dtitle")?.textContent.trim();
  check("its page names it", shown === "live-ct suite", `shows ${JSON.stringify(shown)}`);
  w.close();

  // The console, through the proxy, as the terminal uses it.
  const c = await j("POST", `${P}/containers/${ct}/console/connect`);
  check("its console opens", c.status === 200, JSON.stringify(c.body));
  await sleep(1500);
  await j("POST", `${P}/containers/${ct}/console/write`, { data: Buffer.from("\necho vm-$((6*7))\n").toString("base64") });
  let seen = "";
  let off = 0;
  for (let i = 0; i < 10 && !seen.includes("vm-42"); i++) {
    const r = (await j("GET", `${P}/containers/${ct}/console/read?after=${off}`)).body;
    seen += Buffer.from(r.data ?? "", "base64").toString();
    off = r.offset;
    if (!seen.includes("vm-42") && /login:\s*$/.test(seen)) {
      await j("POST", `${P}/containers/${ct}/console/write`, { data: Buffer.from("root\n").toString("base64") });
      await sleep(500);
      await j("POST", `${P}/containers/${ct}/console/write`, { data: Buffer.from("echo vm-$((6*7))\n").toString("base64") });
    }
  }
  check("the console answers typed input", seen.includes("vm-42") || /login:/.test(seen), JSON.stringify(seen.slice(-200)));
  check("no getty spam on the console", !/can't open \/dev\/tty\d/.test(seen), JSON.stringify(seen.slice(-200)));

  // The browser tells the daemon its terminal size. libvirt gives an LXC
  // container its own console pty and relays to the one the stream sees, so
  // the size has to be written onto the container's console itself -- setting
  // it on the stream's pty left the container at 80x24, which is what made apt
  // and every full-screen program redraw the bottom line instead of scrolling.
  // Set it before login so the shell inherits it, then check from inside.
  const rz = await j("POST", `${P}/containers/${ct}/console/resize`, { cols: 100, rows: 30 });
  check("the console accepts the browser's terminal size", rz.status === 200 && rz.body.cols === 100 && rz.body.rows === 30,
    JSON.stringify(rz.body));

  // Output the container writes by itself must arrive without a keystroke.
  // It used to be read only as a side effect of sending input (no libvirt
  // event loop), so the console updated only when Enter was pressed.
  // Log in (a fresh getty sits at its prompt), then background a delayed echo.
  const type = (t) => j("POST", `${P}/containers/${ct}/console/write`, { data: Buffer.from(t).toString("base64") });
  const until = async (re, ms) => {
    let buf = "";
    const t = Date.now();
    while (!re.test(buf) && Date.now() - t < ms) {
      const r = (await j("GET", `${P}/containers/${ct}/console/read?after=${off}`)).body;
      buf += Buffer.from(r.data ?? "", "base64").toString();
      off = r.offset;
    }
    return buf;
  };
  await type("\n");
  if (/login:\s*$/.test(await until(/(login:|#)\s*$/, 5000))) {
    await type("root\n");
    await until(/[Pp]assword:\s*$/, 5000);
    await type("live-ct-suite\n");
    await until(/#\s*$/, 8000);
  }
  // The container's own terminal must report what the browser asked for: this
  // is the value apt reads.
  await type("stty size\n");
  const sized = await until(/30 100|100 30/, 6000);
  check("…and the container's terminal adopts it", /30 100/.test(sized),
    JSON.stringify(sized.slice(-120)));
  // One command, then no input at all while it prints a line a second.
  await type("for i in 1 2 3; do sleep 1; echo tick-$i; done\n");
  const t0 = Date.now();
  const at = [];
  let late = "";
  while (at.length < 3 && Date.now() - t0 < 10000) {
    const r = (await j("GET", `${P}/containers/${ct}/console/read?after=${off}`)).body;
    late += Buffer.from(r.data ?? "", "base64").toString();
    off = r.offset;
    while (at.length < 3 && late.includes(`tick-${at.length + 1}\r`)) at.push(Date.now() - t0);
  }
  check("output the container writes by itself arrives with no input, as it is written",
    at.length === 3 && at[2] - at[0] > 1500,
    `ticks at ${at.join(", ")} ms: ${JSON.stringify(late.slice(-120))}`);

  // Hardware: read, change, read back; a shrink is refused.
  const hw0 = (await j("GET", `${P}/containers/${ct}/hardware`)).body;
  const hw1 = { ...hw0, vcpus: 2, cpu_limit_pct: 50, swap_mb: 64, disk_gb: hw0.disk_gb + 1 };
  const put = await j("PUT", `${P}/containers/${ct}/hardware`, hw1);
  check("hardware changes are accepted, and the disk grows at once",
    put.status === 200 && put.body.disk_grown === true, JSON.stringify(put.body));
  const hw2 = (await j("GET", `${P}/containers/${ct}/hardware`)).body;
  check("…and read back as set", JSON.stringify(hw2) === JSON.stringify(hw1), `${JSON.stringify(hw2)}`);
  check("the root disk cannot shrink",
    (await j("PUT", `${P}/containers/${ct}/hardware`, { ...hw1, disk_gb: hw0.disk_gb })).status === 400);
  await j("POST", `${P}/containers/${ct}/console/disconnect`);
} finally {
  await j("POST", `${P}/containers/${ct}/destroy`);
  const d = await j("DELETE", `${P}/containers/${ct}`);
  check("deleting it removes its rootfs", d.status === 200 && d.body.removed?.length === 1, JSON.stringify(d.body));
  const after = await vols();
  check("…and keeps the template", after.some((v) => v.name === tpl.name));
  check("…and no ct-<uuid> volume is left", !after.some((v) => v.name === `ct-${ct}.raw`));
  await removeTestNetwork(base, net);
}

const failed = results.filter(([ok]) => !ok);
console.log(failed.length ? `\n${failed.length} check(s) failed` : "\nall container checks passed");
process.exit(failed.length ? 1 : 0);
