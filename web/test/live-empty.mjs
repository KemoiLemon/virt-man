// Empty lists: each says there is nothing and offers one small create button,
// with no create *card* in any grid. Servers are a list, not cards.
//
// Needs no guest and touches no real host: it starts its own throwaway web
// server (own data dir, auth disabled) and points it at a stub daemon that
// answers every list with []. Requires the built dist/ and the server binary.
//
//   node test/live-empty.mjs [server-binary]   (default /usr/local/bin/virt-man-web)

import { execFileSync, spawn } from "node:child_process";
import { mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import http from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { JSDOM, VirtualConsole } from "jsdom";

const bin = process.argv[2] ?? "/usr/local/bin/virt-man-web";
execFileSync("npx", ["vite", "build", "--logLevel", "error", "--outDir", "test/_live"],
  { stdio: "ignore", env: { ...process.env, VM_IIFE: "1" } });
const code = readFileSync("test/_live/app.iife.js", "utf8");
// The empty state is a CSS contract, so the checks below read the stylesheet
// that ships (the same source the theme and mobile audits use) rather than
// jsdom's computed styles. jsdom lays nothing out and never substitutes a
// var(), so a computed value proves neither the rule nor the centring.
const css = readFileSync(`dist/assets/${readdirSync("dist/assets").find((f) => f.endsWith(".css"))}`, "utf8")
  .replace(/\s+/g, " ");

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
  // The topmost dialog: the admin window itself is a modal underneath it.
  const top = (d) => [...d.querySelectorAll(".modal-back.open")].at(-1);
  const modalTitle = (d) => top(d)?.querySelector(".modal-head, h3, .modal-title")?.textContent.trim() ?? "";

  // No servers at all. Servers are managed on the admin Daemons page now (an
  // old /servers link lands there). The sidebar tree shows one ghost row where
  // the server rows would be, so an empty tree says why it is empty.
  let d = await open("/servers");
  check("no servers: the Daemons page says so", /No daemons yet/.test(d.querySelector("#admin-area")?.textContent ?? ""),
    d.querySelector("#admin-area")?.textContent);
  d.querySelector("#add-daemon")?.click();
  await new Promise((r) => setTimeout(r, 200));
  check("…and Add daemon opens the Add Server dialog", /Server/i.test(modalTitle(d)), modalTitle(d));
  // Adding a daemon is the admin page's job, and there is no "generate new
  // token" checkbox: a blank token field does that on create, and means
  // "keep it" on edit, so a checkbox beside it was two controls for one value.
  const connDialog = top(d);
  check("…with no generate-token checkbox, only the token field",
    !!connDialog?.querySelector("#conn-token") && !connDialog?.querySelector("#conn-gen")
      && !/generate new token/i.test(connDialog?.textContent ?? ""),
    connDialog?.textContent?.replace(/\s+/g, " ").slice(0, 140));
  connDialog?.remove();

  d = await open("/");
  const ghost = d.querySelector("#tree .tree-ghost");
  check("no servers: the sidebar tree shows one ghost server row",
    /No servers/.test(ghost?.textContent ?? "") && !!ghost?.querySelector(".tree-ic svg"),
    d.querySelector("#tree")?.textContent?.slice(0, 120));
  check("…which is not a button and not focusable",
    ghost?.tagName === "DIV" && !ghost?.hasAttribute("tabindex") && !ghost?.hasAttribute("data-conn"));
  // A ghost row stands in for a real one, so it must be indented like one. The
  // indent for networks and pools rides on `.tree-res`, so a ghost that omits
  // that class sits against the pane edge. jsdom resolves no var() and lays
  // nothing out, so this asserts the shipped rule and the class list the ghost
  // is built with -- a computed padding on this page would only measure the
  // server ghost, which is not the row that regressed.
  check("…and carries the real row's indent class, not just its kind's",
    /\.tree-res\s*\{[^}]*padding-left/.test(css)
      && /ghostRow\("tree-res tree-net"/.test(readFileSync("src/app/tree.ts", "utf8"))
      && /ghostRow\("tree-res tree-pool"/.test(readFileSync("src/app/tree.ts", "utf8")));
  let empty;

  d = await open("/");
  const serverTable = [...d.querySelectorAll(".dash-section")]
    .find((s) => /Servers/.test(s.querySelector(".section-title")?.textContent ?? ""));
  const serverEmpty = serverTable?.querySelector("table.grid td.empty");
  check("no servers: the dashboard's Servers table says so, like Networking",
    /No servers\./.test(serverEmpty?.textContent ?? "") && serverEmpty?.getAttribute("colspan") === "4",
    serverTable?.textContent?.slice(0, 120));
  const netEmpty = [...d.querySelectorAll(".dash-section")]
    .find((s) => /Networking/.test(s.querySelector(".section-title")?.textContent ?? ""))
    ?.querySelector("table.grid td.empty");
  check("…in the same shape as the Networking table's empty row",
    !!netEmpty && netEmpty.getAttribute("colspan") === serverEmpty?.getAttribute("colspan"),
    `networking colspan ${netEmpty?.getAttribute("colspan")}`);

  d = await open("/guests");
  empty = d.querySelector(".empty-create");
  check("no guests: says so, with Create Guest", /No guests available/.test(empty?.textContent ?? "") && empty?.querySelector("#dc-guest-create"), empty?.textContent);
  check("…and no create card", noCards(d));

  // The dashboard's Storage Pools section keeps the shape of a pool card on a
  // host with none, rather than collapsing to a bare line: an inert card that
  // says so, with no meter fill and nothing to open.
  d = await open("/");
  const poolSection = [...d.querySelectorAll(".dash-section")]
    .find((s) => /Storage Pools/.test(s.textContent));
  const shadow = poolSection?.querySelector(".storage-card-none");
  check("no pools: the section holds a card saying so",
    /No storage pools/.test(shadow?.textContent ?? ""), poolSection?.textContent?.slice(0, 160));
  check("…shaped like a real pool card, minus the numbers",
    !!shadow?.querySelector(".storage-card-icon svg") && !!shadow?.querySelector(".meter")
    && !shadow?.querySelector(".meter-fill") && !shadow?.querySelector(".metric-value")?.textContent.includes("%"));
  check("…and inert: no pool to open, hover or key through",
    !shadow?.hasAttribute("data-open") && !shadow?.hasAttribute("tabindex") && !shadow?.querySelector("[data-open]"));

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
    // Stacked and centred: the button is under the message, and the pane is a
    // column flexbox so the pair sits in the middle of it. jsdom computes no
    // var() and lays nothing out, so this is the rule, not the geometry.
    check(`…stacked: the button is under the message, not beside it`,
      empty?.firstElementChild?.classList.contains("empty-msg") && empty?.querySelector("button")?.previousElementSibling === empty?.firstElementChild);
    // The built stylesheet is minified, so match declarations rather than a
    // literal rule: order and spacing are the bundler's, not ours.
    check(`…a centred column, in the shipped stylesheet`,
      /\.empty-create\{[^}]*flex-direction:column/.test(css) && /\.empty-create\{[^}]*align-items:center/.test(css)
      && /\.detail:has\(>\.empty-create:only-child\)\{[^}]*justify-content:center/.test(css));
    check(`…with the message larger than the list text`,
      /\.empty-create \.empty-msg\{font-size:16px\}/.test(css));
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
