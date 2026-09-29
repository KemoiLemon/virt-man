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
//   - its page has no Hardware tab, and a text console that answers
//   - a VM route cannot reach it, and a VM-only action is refused
//   - it gets an address from the daemon's DHCP (read from the network's hosts)
//   - deleting it removes its rootfs and keeps the template

import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";

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
const nets = (await j("GET", `${P}/networks`)).body.filter((n) => n.active && n.ip);
const create = await j("POST", `${P}/containers`, {
  title: "live-ct suite",
  template: { pool, volume: tpl.name },
  rootfs: { pool, size_gb: 1 },
  memory_mb: 128,
  vcpus: 1,
  networks: nets.length ? [nets[0].name] : [],
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

  if (nets.length) {
    const hostsUrl = `${P}/networks/${encodeURIComponent(nets[0].name)}/hosts`;
    let ip = null;
    for (let i = 0; i < 40 && !ip; i++) {
      await sleep(1000);
      ip = (await j("GET", hostsUrl)).body.hosts?.find((h) => h.vm === ct)?.ip ?? null;
    }
    check("it gets an address from the daemon's DHCP", !!ip, `on ${nets[0].name}`);
  }

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
  check("its page has no Hardware tab", tabs.length > 0 && !tabs.includes("hardware"), tabs.join(","));
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
  await j("POST", `${P}/containers/${ct}/console/disconnect`);
} finally {
  await j("POST", `${P}/containers/${ct}/destroy`);
  const d = await j("DELETE", `${P}/containers/${ct}`);
  check("deleting it removes its rootfs", d.status === 200 && d.body.removed?.length === 1, JSON.stringify(d.body));
  const after = await vols();
  check("…and keeps the template", after.some((v) => v.name === tpl.name));
  check("…and no ct-<uuid> volume is left", !after.some((v) => v.name === `ct-${ct}.raw`));
}

const failed = results.filter(([ok]) => !ok);
console.log(failed.length ? `\n${failed.length} check(s) failed` : "\nall container checks passed");
process.exit(failed.length ? 1 : 0);
