// A refused VM create must SAY it was refused, and why.
//
// It used to toast "VM created" the moment the work started, whatever the
// server then answered: a create over the user's limits failed silently, and
// its reason only reached the activity log. This drives the real Create VM
// form as a limited, non-admin user, over their vCPU limit, and checks what
// the user is told and what is left behind.
//
// Needs an auth-enabled instance with a non-admin user limited to 2 vCPUs
// (VM_LOW_USER/VM_LOW_PASS), who owns a network (so the form has one to pick):
//   NODE_TLS_REJECT_UNAUTHORIZED=0 VM_LOW_USER=bob VM_LOW_PASS=... \
//     node test/live-quota.mjs https://127.0.0.1:8006

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";
import { ensureTestPool, removeTestPool } from "./_fixture.mjs";

const base = process.argv[2] ?? "https://127.0.0.1:8006";
const USER = process.env.VM_LOW_USER;
const PASS = process.env.VM_LOW_PASS;
if (!USER || !PASS) {
  console.error("set VM_LOW_USER/VM_LOW_PASS (a non-admin limited to 2 vCPUs)");
  process.exit(2);
}

execFileSync("npx", ["vite", "build", "--logLevel", "error", "--outDir", "test/_live",
  "--assetsDir", ".", "--target", "es2020"],
{ stdio: "inherit", env: { ...process.env, VM_IIFE: "1" } });
const code = readFileSync("test/_live/app.iife.js", "utf8");

const dom = new JSDOM(`<!doctype html><html><body><div id="app"></div></body></html>`,
  { url: `${base}/`, runScripts: "dangerously", pretendToBeVisual: true, virtualConsole: new VirtualConsole() });
const { window } = dom;
const { document } = window;
window.ResizeObserver ??= class { observe() {} unobserve() {} disconnect() {} };
let cookie = "";
window.fetch = (input, init = {}) => {
  const url = typeof input === "string" ? new URL(input, base).toString() : input;
  if (cookie) init.headers = { ...(init.headers ?? {}), Cookie: cookie };
  return fetch(url, init).then((r) => {
    const m = r.headers.get("set-cookie")?.match(/virt-man-session=[^;]+/);
    if (m) cookie = m[0];
    return r;
  });
};
window.eval(code);

const wait = (ms) => new Promise((r) => window.setTimeout(r, ms));
const click = (el) => el?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
const results = [];
const check = (name, fn) => {
  try { fn(); results.push(["pass", name]); } catch (e) { results.push(["FAIL", `${name} — ${e.message}`]); }
};
// Every toast, recorded as it appears, so a success toast that faded before
// the checks ran is still counted.
const seen = [];
new window.MutationObserver((muts) => {
  for (const m of muts) for (const n of m.addedNodes) {
    if (n.classList?.contains("toast")) {
      seen.push({ bad: n.classList.contains("bad"), text: n.querySelector(".toast-msg")?.textContent ?? "" });
    }
  }
}).observe(document.body, { childList: true, subtree: true });
const toasts = () => seen;

await wait(1500);
document.querySelector("#lu").value = USER;
document.querySelector("#lp").value = PASS;
click(document.querySelector("#login-btn"));
await wait(2500);

const me = await window.fetch("/auth/me").then((r) => r.json());
const conns = await window.fetch("/connections").then((r) => r.json());
const conn = conns[0]?.id;
// Storage is a native directory pool now, and a limited user cannot create one:
// reuse the administrator's active pool and keep the flow identical.
const asPage = (u, i) => window.fetch(u, i);
const testPool = await ensureTestPool(base, { conn, fetch: asPage });
const pool = testPool.name;
const volsPath = `/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes`;
const volsBefore = await window.fetch(volsPath).then((r) => r.json());
check("signed in as a limited non-admin", () => {
  assert.equal(me.admin, false);
  assert.equal(me.quota?.default?.vcpus, 2, `vCPU limit is 2 (${JSON.stringify(me.quota)})`);
});

// The real form: open the server, Create VM, ask for 4 vCPUs.
click(document.querySelector(`.tree-row.tree-group[data-conn="${conn}"]`));
await wait(1500);
click(document.querySelector('[data-tab="vms"]'));
await wait(1000);
click(document.querySelector("#create-vm"));
await wait(2500);
const setField = (sel, v) => {
  const el = document.querySelector(sel);
  assert.ok(el, `${sel} present`);
  el.value = v;
  el.dispatchEvent(new window.Event("change", { bubbles: true }));
};
check("the Create VM form opened", () => assert.ok(document.querySelector("#hw-save"), "form open"));
setField("#g-name", "quota-probe");
await wait(400);
click(document.querySelector('[data-hw="processors"]'));
await wait(400);
setField("#g-cpus", "4");
await wait(400);
click(document.querySelector('[data-hw="disk"]'));
await wait(400);
setField("#g-size", "1");
await wait(400);
click(document.querySelector("#hw-save"));
await wait(5000);

const shown = toasts();
check("the user is told it failed, not that it was created", () => {
  assert.ok(!shown.some((t) => /VM quota-probe created|^VM created/.test(t.text)), `no success toast (${JSON.stringify(shown)})`);
  const bad = shown.find((t) => t.bad && /quota-probe/.test(t.text));
  assert.ok(bad, `a failure toast (${JSON.stringify(shown)})`);
});
check("the failure says which limit, the ask, the use, what is left, and the server", () => {
  const bad = shown.find((t) => t.bad && /quota-probe/.test(t.text));
  assert.match(bad.text, /Create VM quota-probe failed: vCPU limit reached on '[^']+': this needs 4 more, you already use 0 of your 2 \(2 left\)/);
  assert.match(bad.text, /ask an administrator to raise your limit/);
});

const doms = await window.fetch(`/proxy/${conn}/domains`).then((r) => r.json());
const volsAfter = await window.fetch(volsPath).then((r) => r.json());
check("no guest was defined", () => assert.ok(!doms.some((d) => d.name === "quota-probe"), "no quota-probe guest"));
check("the disk made on the way was removed again", () => {
  const leftover = volsAfter.filter((v) => /^quota-probe/.test(v.name));
  assert.equal(leftover.length, 0, `leftover volumes: ${leftover.map((v) => v.name)}`);
  assert.equal(volsAfter.length, volsBefore.length, "pool back to where it was");
});

const act = await window.fetch("/activity").then((r) => r.json());
check("the failure stays on screen until dismissed", () => {
  const still = [...document.querySelectorAll(".toast.bad")].some((t) => /quota-probe/.test(t.textContent));
  assert.ok(still, "still shown 5 s later");
});

check("the activity log records the same reason", () => {
  const e = act.entries.find((x) => x.label === "Create VM quota-probe");
  assert.ok(e, "entry present");
  assert.equal(e.status, "failed");
  assert.match(e.message, /vCPU limit reached/);
});

// A permission refusal is descriptive too: another user's resource. A pool
// root is a host-level choice, so creating one now needs Sys.Modify rather than
// a storage grant — an owner of Datastore.* must not be able to expose a
// host directory as a pool.
const nope = await window.fetch(`/proxy/${conn}/pools`, { method: "POST", body: "<pool/>" });
const nopeBody = await nope.json();
check("a permission refusal says what was missing and on what", () => {
  assert.equal(nope.status, 403);
  assert.match(nopeBody.error, /^you do not have permission to manage servers \(storage on '[^']+'; needs Sys\.Modify\)\. Ask an administrator to grant it\.$/);
});

await removeTestPool(base, testPool, { fetch: asPage });

for (const [s, m] of results) console.log(`${s === "pass" ? "✔" : "✖"} ${m}`);
const fails = results.filter(([s]) => s === "FAIL");
if (fails.length) { console.log(`\n${fails.length} check(s) failed`); process.exit(1); }
console.log("\nall quota checks passed");
process.exit(0);
