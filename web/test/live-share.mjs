// Owners share their VMs; admins create resources for users without leaving
// the admin page; users see their limits in their profile.
//
// Needs an auth-enabled instance with an admin (VM_USER/VM_PASS) and two
// non-admin users (VM_LOW_USER/VM_LOW_PASS and VM_OTHER_USER/VM_OTHER_PASS),
// the first limited to 2 vCPUs. The suite creates, and removes again, one
// small network and one guest owned by VM_LOW_USER.
//   NODE_TLS_REJECT_UNAUTHORIZED=0 VM_USER=root VM_PASS=... \
//     VM_LOW_USER=bob VM_LOW_PASS=... VM_OTHER_USER=eve VM_OTHER_PASS=... \
//     node test/live-share.mjs https://127.0.0.1:8006

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";

const base = process.argv[2] ?? "https://127.0.0.1:8006";
const env = (k) => {
  const v = process.env[k];
  if (!v) { console.error(`set ${k}`); process.exit(2); }
  return v;
};
const ADMIN = [env("VM_USER"), env("VM_PASS")];
const BOB = [env("VM_LOW_USER"), env("VM_LOW_PASS")];
const EVE = [env("VM_OTHER_USER"), env("VM_OTHER_PASS")];

execFileSync("npx", ["vite", "build", "--logLevel", "error", "--outDir", "test/_live",
  "--assetsDir", ".", "--target", "es2020"], { stdio: "inherit", env: { ...process.env, VM_IIFE: "1" } });
const code = readFileSync("test/_live/app.iife.js", "utf8");

const results = [];
const check = (name, fn) => {
  try { fn(); results.push(["pass", name]); } catch (e) { results.push(["FAIL", `${name} — ${e.message}`]); }
};

/// A signed-in app window, and a fetch carrying its session.
async function session([user, pass]) {
  const dom = new JSDOM(`<!doctype html><html><body><div id="app"></div></body></html>`,
    { url: `${base}/`, runScripts: "dangerously", pretendToBeVisual: true, virtualConsole: new VirtualConsole() });
  const w = dom.window;
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
  const seen = [];
  new w.MutationObserver((ms) => {
    for (const m of ms) for (const n of m.addedNodes) {
      if (n.classList?.contains("toast")) seen.push({ bad: n.classList.contains("bad"), text: n.textContent });
    }
  }).observe(w.document.body, { childList: true, subtree: true });
  w.eval(code);
  const wait = (ms) => new Promise((r) => w.setTimeout(r, ms));
  await wait(1500);
  w.document.querySelector("#lu").value = user;
  w.document.querySelector("#lp").value = pass;
  w.document.querySelector("#login-btn").dispatchEvent(new w.MouseEvent("click", { bubbles: true }));
  await wait(2500);
  const json = (path, init) => w.fetch(path, init).then(async (r) => ({ status: r.status, body: await r.json().catch(() => ({})) }));
  const click = (el) => el?.dispatchEvent(new w.MouseEvent("click", { bubbles: true }));
  const $ = (sel) => w.document.querySelector(sel);
  const top = () => [...w.document.querySelectorAll(".modal-back.open")].pop();
  return { w, d: w.document, wait, json, click, $, top, toasts: seen };
}

const admin = await session(ADMIN);
const me = (await admin.json("/auth/me")).body;
const conn = (await admin.json("/connections")).body[0]?.id;
const users = (await admin.json("/users")).body;
const bobId = users.find((u) => u.username === BOB[0])?.id;
const eveId = users.find((u) => u.username === EVE[0])?.id;
check("setup: admin, two users, a daemon", () => {
  assert.equal(me.admin, true);
  assert.ok(conn && bobId && eveId, `conn=${conn} bob=${bobId} eve=${eveId}`);
});

// ---- Admin: resources are split by kind, tabbed by daemon.
admin.click(admin.$("#admin-btn"));
await admin.wait(1500);
admin.click(admin.$('#tree [data-admin-section="networks"]'));
await admin.wait(2500);
check("Networks is its own page, tabbed by daemon", () => {
  assert.equal(admin.w.location.pathname, "/admin/networks");
  assert.equal(admin.$("#content .dtitle").textContent.trim(), "Networks");
  const tabs = [...admin.d.querySelectorAll("[data-res-tab]")];
  assert.ok(tabs.length >= 1, "a tab per daemon");
  assert.equal(admin.$(".tab.active")?.dataset.resTab, conn);
  const heads = [...admin.d.querySelectorAll("#admin-area table.grid th")].map((t) => t.textContent.trim());
  assert.deepEqual(heads.slice(0, 6), ["Name", "Owner", "State", "Mode", "Subnet", "Bridge"], "network columns");
});

// ---- Admin: create a network for bob without leaving the admin page.
admin.click(admin.$("#res-new"));
await admin.wait(400);
admin.top().querySelector("#cf-user").value = bobId;
admin.click(admin.top().querySelector("#cf-next"));
await admin.wait(1500);
check("the create form opens over the admin page", () => {
  assert.ok(admin.$("#net-name"), "network form open");
  assert.equal(admin.w.location.pathname, "/admin/networks", "still on the admin page");
  assert.ok(admin.$("#tree [data-admin-section]"), "admin sidebar still shown");
});
const setVal = (s, sel, v) => {
  const el = s.$(sel);
  el.value = v;
  el.dispatchEvent(new s.w.Event("change", { bubbles: true }));
  el.dispatchEvent(new s.w.Event("input", { bubbles: true }));
};
setVal(admin, "#net-name", "share-net");
setVal(admin, "#net-ip", "10.91.0.1/24");
admin.click(admin.$("#net-save"));
await admin.wait(5000);
const nets = (await admin.json(`/proxy/${conn}/networks`)).body;
check("…the network exists, owned by bob", () => {
  const n = nets.find((x) => x.name === "share-net");
  assert.ok(n, `created (${admin.toasts.map((t) => t.text)})`);
  assert.equal(n.owner, bobId);
});
check("…and the admin is still on the admin page, which repainted", () => {
  assert.equal(admin.w.location.pathname, "/admin/networks");
  assert.ok(admin.$("#tree [data-admin-section]"), "admin sidebar");
  assert.match(admin.$("#admin-area").textContent, /share-net/, "the new network is listed");
});

// ---- bob creates a small guest on his network, then shares it with eve.
const bob = await session(BOB);
const vmXml = `<domain type="kvm"><name>share-vm</name><memory unit="KiB">262144</memory><vcpu>1</vcpu>
  <os><type arch="x86_64" machine="q35">hvm</type></os><devices><emulator>/usr/bin/qemu-system-x86_64</emulator>
  <interface type="network"><source network="share-net"/><model type="virtio"/></interface>
  <graphics type="vnc" port="-1" autoport="yes" listen="127.0.0.1"/></devices></domain>`;
const made = await bob.w.fetch(`/proxy/${conn}/domains?start=false`, { method: "POST", body: vmXml });
check("bob creates a guest", () => assert.equal(made.status, 200));

const eve = await session(EVE);
const eveBefore = (await eve.json(`/proxy/${conn}/domains/share-vm`)).status;
const eveShare = await eve.json(`/shares?conn=${conn}&vm=share-vm`);
check("before sharing, eve can neither see nor share it", () => {
  assert.equal(eveBefore, 403);
  assert.equal(eveShare.status, 403);
  assert.match(eveShare.body.error, /only the owner of VM 'share-vm'/);
});

// bob shares through the UI: VM page, Share button.
await bob.wait(1500);
bob.w.history.pushState(null, "", `/${encodeURIComponent((await bob.json("/connections")).body[0].name)}/vms/share-vm`);
bob.w.dispatchEvent(new bob.w.PopStateEvent("popstate"));
await bob.wait(2500);
check("the owner sees Share on the VM page", () => assert.ok(bob.$("#vm-share"), "share button"));
bob.click(bob.$("#vm-share"));
await bob.wait(1500);
check("the share dialog lists people and two access levels", () => {
  const m = bob.top();
  assert.ok(m, "dialog");
  const people = [...m.querySelectorAll("#sh-user option")].map((o) => o.textContent);
  assert.ok(people.includes(EVE[0]) && !people.includes(BOB[0]), `people: ${people}`);
  assert.deepEqual([...m.querySelectorAll("#sh-role option")].map((o) => o.value), ["VMUser", "VMAdmin"]);
});
{
  const m = bob.top();
  m.querySelector("#sh-user").value = eveId;
  m.querySelector("#sh-role").value = "VMUser";
  bob.click(m.querySelector("#sh-add"));
}
await bob.wait(2000);
check("the share is listed", () => assert.match(bob.top()?.textContent ?? "", new RegExp(`${EVE[0]}[\\s\\S]*Use`)));

const eveList = (await eve.json(`/proxy/${conn}/domains`)).body;
const evePower = (await eve.json(`/proxy/${conn}/domains/share-vm/state`)).status;
const eveXml = await eve.w.fetch(`/proxy/${conn}/domains/share-vm/xml`, { method: "PUT", body: vmXml });
const eveDel = (await eve.json(`/proxy/${conn}/domains/share-vm`, { method: "DELETE" })).status;
const eveReshare = await eve.json("/shares", { method: "PUT", headers: { "content-type": "application/json" },
  body: JSON.stringify({ conn, vm: "share-vm", user: bobId, role: "VMUser" }) });
check("eve now sees it and can use it, but not change, delete or re-share it", () => {
  assert.ok(eveList.some((d) => d.name === "share-vm" && !d.mine), "listed, not hers");
  assert.equal(evePower, 200, "can read its state");
  assert.equal(eveXml.status, 403, "cannot reconfigure as VMUser");
  assert.equal(eveDel, 403, "cannot delete");
  assert.equal(eveReshare.status, 403, "cannot share onward");
});

// Stop sharing through the UI.
bob.click(bob.top()?.querySelector(`[data-unshare="${eveId}"]`));
await bob.wait(2000);
const eveAfter = (await eve.json(`/proxy/${conn}/domains`)).body;
check("stopping the share removes her access", () => {
  assert.ok(!eveAfter.some((d) => d.name === "share-vm"), "gone from her list");
});
bob.top()?.remove();

// ---- bob's profile shows his limits.
bob.click(bob.$("#profile-btn"));
await bob.wait(2500);
check("the profile shows the user's resource limits, each its own meter", () => {
  const box = bob.$("#pf-limits");
  assert.ok(box, "limits section");
  assert.match(box.textContent, /Resource Limits/);
  const specs = [...box.querySelectorAll(".spec b")].map((b) => b.textContent.trim());
  assert.deepEqual(specs.slice(0, 5), ["VMs", "vCPUs", "Memory", "Storage", "Networks"]);
  assert.match(box.textContent, /1 \/ 2/, "vCPUs used against the limit (share-vm has 1 of 2)");
  assert.ok(box.querySelectorAll(".limit-spec .meter").length >= 1, "meters for set limits");
});
bob.top()?.remove();

admin.click(admin.$("#admin-btn"));
await admin.wait(800);
admin.click(admin.$("#profile-btn"));
await admin.wait(2000);
check("an administrator's profile says they have no limits", () => {
  assert.match(admin.$("#pf-limits")?.textContent ?? "", /Administrators have no resource limits/);
});

// ---- cleanup
await bob.w.fetch(`/proxy/${conn}/domains/share-vm`, { method: "DELETE" });
await bob.w.fetch(`/proxy/${conn}/networks/share-net`, { method: "DELETE" });

for (const [s, m] of results) console.log(`${s === "pass" ? "✔" : "✖"} ${m}`);
const fails = results.filter(([s]) => s === "FAIL");
if (fails.length) { console.log(`\n${fails.length} check(s) failed`); process.exit(1); }
console.log("\nall share checks passed");
process.exit(0);
