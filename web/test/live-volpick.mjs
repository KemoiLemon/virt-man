// Create VM can boot from one of the user's own volumes.
//
// Drives the real Create VM form as a limited, non-admin user: picks
// "Use One of My Volumes", and checks the guest boots from that exact file,
// as qcow2 (it was made empty here), that a refused create leaves the chosen
// volume alone (it was the user's before), and that other users' volumes are
// not offered.
//   NODE_TLS_REJECT_UNAUTHORIZED=0 VM_LOW_USER=bob VM_LOW_PASS=... \
//     node test/live-volpick.mjs https://127.0.0.1:8006

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";
import { ensureTestPool, removeTestPool } from "./_fixture.mjs";

const base = process.argv[2] ?? "https://127.0.0.1:8006";
const [USER, PASS] = [process.env.VM_LOW_USER, process.env.VM_LOW_PASS];
if (!USER || !PASS) { console.error("set VM_LOW_USER/VM_LOW_PASS"); process.exit(2); }

execFileSync("npx", ["vite", "build", "--logLevel", "error", "--outDir", "test/_live",
  "--assetsDir", ".", "--target", "es2020"], { stdio: "inherit", env: { ...process.env, VM_IIFE: "1" } });
const code = readFileSync("test/_live/app.iife.js", "utf8");
const dom = new JSDOM(`<!doctype html><html><body><div id="app"></div></body></html>`,
  { url: `${base}/`, runScripts: "dangerously", pretendToBeVisual: true, virtualConsole: new VirtualConsole() });
const { window: w } = dom;
const d = w.document;
w.ResizeObserver ??= class { observe() {} unobserve() {} disconnect() {} };
let cookie = "";
w.fetch = (input, init = {}) => {
  const url = typeof input === "string" ? new URL(input, base).toString() : input;
  if (cookie) init.headers = { ...(init.headers ?? {}), Cookie: cookie };
  return fetch(url, init).then((r) => {
    const m = r.headers.get("set-cookie")?.match(/virt-man-session=[^;]+/);
    if (m) cookie = m[0];
    return r;
  });
};
const toasts = [];
new w.MutationObserver((ms) => { for (const m of ms) for (const n of m.addedNodes) {
  if (n.classList?.contains("toast")) toasts.push({ bad: n.classList.contains("bad"), text: n.textContent });
} }).observe(d.body, { childList: true, subtree: true });
w.eval(code);

const wait = (ms) => new Promise((r) => w.setTimeout(r, ms));
const click = (el) => el?.dispatchEvent(new w.MouseEvent("click", { bubbles: true }));
const set = (sel, v) => {
  const el = d.querySelector(sel);
  assert.ok(el, `${sel} present`);
  el.value = v;
  el.dispatchEvent(new w.Event("change", { bubbles: true }));
};
const results = [];
const check = (name, fn) => { try { fn(); results.push(["pass", name]); } catch (e) { results.push(["FAIL", `${name} — ${e.message}`]); } };
const json = (p, i) => w.fetch(p, i).then(async (r) => ({ status: r.status, body: await r.json().catch(() => ({})) }));

await wait(1500);
d.querySelector("#lu").value = USER;
d.querySelector("#lp").value = PASS;
click(d.querySelector("#login-btn"));
await wait(2500);

const conn = (await json("/connections")).body[0].id;
// Pools are administrator-created and a non-admin can only reuse one. This
// suite signs in as a limited user, so pass that page's fetch: creation is
// refused for it, and an existing active pool is reused.
const asPage = (u, i) => w.fetch(u, i);
const testPool = await ensureTestPool(base, { conn, fetch: asPage });
const pool = testPool.name;
const volXml = (n) => `<volume><name>${n}</name><capacity>1073741824</capacity><target><format type="qcow2"/></target></volume>`;
const mine = (await json(`/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes`, {
  method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ xml: volXml("pick-disk.qcow2") }) })).body;
const net = await w.fetch(`/proxy/${conn}/networks`, { method: "POST",
  body: `<network><name>pick-net</name><forward mode="nat"/><ip address="10.92.0.1" netmask="255.255.255.0"/></network>` });
check("setup: bob has a volume and a network", () => {
  assert.ok(mine.path, JSON.stringify(mine));
  assert.equal(net.status, 200);
});

async function openCreate() {
  click(d.querySelector(`.tree-row.tree-group[data-conn="${conn}"]`));
  await wait(1500);
  click(d.querySelector('[data-tab="vms"]'));
  await wait(1000);
  click(d.querySelector("#create-vm"));
  await wait(2500);
  set("#g-name", "pick-vm");
  await wait(300);
  click(d.querySelector('[data-hw="disk"]'));
  await wait(400);
  set("#g-pool", pool);
  await wait(800);
  set("#g-disk-mode", "existing");
  await wait(1200);
}

await openCreate();
check("the Hard Disk page offers the user's own volumes", () => {
  const opts = [...d.querySelectorAll("#g-volume option")].map((o) => o.value);
  assert.ok(opts.includes("pick-disk.qcow2"), `offered: ${opts}`);
  assert.ok(!opts.some((o) => o.endsWith(".iso")), "no ISOs");
});
// A pool is named by its UUID and keeps the human name as a title, so the
// select must show the title -- but keep the libvirt name as the value, since
// that is what the XML and the API address the pool by. This form used a plain
// `opt()` and so listed the raw UUID.
const poolList = (await json(`/proxy/${conn}/pools`)).body;
check("the Storage Pool select is labelled by nickname, not the UUID", () => {
  const sel = d.querySelector("#g-pool");
  assert.ok(sel, "the pool select is on the Hard Disk page");
  assert.ok(sel.options.length, "at least one pool is offered");
  for (const o of sel.options) {
    const p = poolList.find((x) => x.name === o.value);
    if (!p) continue;
    assert.equal(
      o.textContent, p.title || p.name,
      `option label must be the nickname (pool ${p.name}, title ${p.title}): ${o.textContent}`,
    );
    assert.equal(o.value, p.name, "the value must stay the libvirt name");
  }
});
const all = (await json(`/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes`)).body;
check("…and only theirs", () => {
  const offered = [...d.querySelectorAll("#g-volume option")].map((o) => o.value);
  const others = all.filter((v) => !v.mine).map((v) => v.name);
  assert.ok(!offered.some((o) => others.includes(o)), `not offered: ${others.filter((o) => offered.includes(o))}`);
});
check("the sidebar summarises the choice", () => {
  assert.match(d.querySelector('[data-hw="disk"]').textContent, /pick-disk\.qcow2/);
});

// Over the vCPU limit first: refused, and the chosen volume must survive.
click(d.querySelector('[data-hw="processors"]'));
await wait(300);
set("#g-cpus", "4");
await wait(300);
click(d.querySelector("#hw-save"));
await wait(4000);
const afterRefused = (await json(`/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes`)).body;
check("a refused create leaves the chosen volume alone", () => {
  assert.ok(toasts.some((t) => t.bad && /vCPU limit/.test(t.text)), `refused (${JSON.stringify(toasts)})`);
  assert.ok(afterRefused.some((v) => v.name === "pick-disk.qcow2"), "still there");
});
d.querySelectorAll(".toast").forEach((t) => t.remove());

// Within limits: created, booting from exactly that file.
await openCreate();
click(d.querySelector('[data-hw="processors"]'));
await wait(300);
set("#g-cpus", "1");
await wait(300);
click(d.querySelector("#hw-save"));
await wait(5000);
// Named by a fresh UUID; "pick-vm" is its title.
const pickVm = (await json(`/proxy/${conn}/domains`)).body.find((d) => d.title === "pick-vm");
const xml = pickVm ? (await json(`/proxy/${conn}/domains/${pickVm.uuid}/xml`)).body.xml ?? "" : "";
check("the guest is named by its UUID, titled pick-vm", () => {
  assert.ok(pickVm, "found by title");
  assert.equal(pickVm.name, pickVm.uuid);
});
check("the guest is created from the chosen volume, as qcow2", () => {
  assert.ok(toasts.some((t) => !t.bad && /VM pick-vm created/.test(t.text)), JSON.stringify(toasts));
  assert.ok(xml.includes(`<source file='${mine.path}'/>`), `boots from ${mine.path}`);
  assert.match(xml, /<driver name='qemu' type='qcow2'\/>/, "qcow2: it was made empty here");
});
const volsAfter = (await json(`/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes`)).body;
check("no second disk was created", () => {
  assert.ok(!volsAfter.some((v) => v.name === `${pickVm?.uuid}.qcow2`), "no UUID-named disk");
});

// cleanup: the guest (with its storage -- the chosen volume), then the net,
// then the pool only when this run made it.
if (pickVm) await w.fetch(`/proxy/${conn}/domains/${pickVm.uuid}?remove_storage=true`, { method: "DELETE" });
await w.fetch(`/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes/pick-disk.qcow2`, { method: "DELETE" });
await w.fetch(`/proxy/${conn}/networks/pick-net`, { method: "DELETE" });
await removeTestPool(base, testPool, { fetch: asPage });

for (const [s, m] of results) console.log(`${s === "pass" ? "✔" : "✖"} ${m}`);
const fails = results.filter(([s]) => s === "FAIL");
if (fails.length) { console.log(`\n${fails.length} check(s) failed`); process.exit(1); }
console.log("\nall volume-picker checks passed");
process.exit(0);
