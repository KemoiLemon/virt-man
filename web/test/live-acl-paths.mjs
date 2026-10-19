// The permission picker's labels: an ACL path is shown as the path itself, in
// forward slashes, with the daemon's name substituted for its id -- never as
// prose ("optibox > all vms") or with an origin arrow. The option's *value*
// must stay the id form the server stores, or a grant would be written to a
// path nothing matches.
//
// Drives the real admin form in jsdom against a running backend.
//
//   node test/live-acl-paths.mjs [base]   (default http://127.0.0.1:8799)

import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";

const base = process.argv[2] ?? "http://127.0.0.1:8799";

execFileSync("npx", ["vite", "build", "--logLevel", "error", "--outDir", "test/_live"],
  { stdio: "ignore", env: { ...process.env, VM_IIFE: "1" } });
const code = readFileSync("test/_live/app.iife.js", "utf8");
const css = readFileSync(`dist/assets/${readdirSync("dist/assets").find((f) => f.endsWith(".css"))}`, "utf8");

const results = [];
const check = (name, ok, detail = "") => results.push([ok, name, detail]);

const vc = new VirtualConsole();
vc.on("jsdomError", (e) => { if (!/not implemented|canvas|WebSocket/i.test(e.message)) console.error("jsdom:", e.message); });
const dom = new JSDOM(`<!doctype html><html><head><style>${css}</style></head><body><div id="app"></div></body></html>`,
  { url: base + "/admin/permissions", runScripts: "dangerously", pretendToBeVisual: true, virtualConsole: vc });
const w = dom.window;
w.fetch = (i, init) => fetch(new URL(typeof i === "string" ? i : i.url, base).toString(), init);
w.ResizeObserver ??= class { observe() {} unobserve() {} disconnect() {} };
w.eval(code);
await new Promise((r) => w.setTimeout(r, 2500));

const conn = (await (await fetch(base + "/connections")).json())[0];
const click = (el) => el?.dispatchEvent(new w.MouseEvent("click", { bubbles: true }));

// Add permission opens the same form an edit does, so one inspection covers both.
click(w.document.querySelector("#add-acl"));
await new Promise((r) => w.setTimeout(r, 1200));

const sel = w.document.querySelector("#a-path");
const opts = [...(sel?.options ?? [])].map((o) => ({ value: o.value, label: o.textContent }));
const labels = opts.map((o) => o.label);
const values = opts.map((o) => o.value);

check("the picker rendered options", opts.length > 2, `${opts.length} options`);

// Every label is a slash path: no origin arrow, no em dash separator.
check("no arrow or dash separators in labels", !labels.some((l) => /[›→]|\s[—-]\s/.test(l)),
  labels.filter((l) => /[›→]|\s[—-]\s/.test(l)).join(" | "));

check("labels are absolute forward-slash paths", labels.every((l) => l.startsWith("/")),
  labels.filter((l) => !l.startsWith("/")).join(" | "));

// The top of the tree, and the daemon node named rather than id-addressed.
check("root is /", labels.includes("/"), labels.slice(0, 4).join(" | "));
check("daemons are /nodes", labels.includes("/nodes"), labels.slice(0, 4).join(" | "));
check("a daemon is /nodes/<name>", labels.includes(`/nodes/${conn.name}`),
  labels.filter((l) => l.startsWith("/nodes/")).slice(0, 3).join(" | "));

// The id must not leak into a label, but must remain the value.
check("no daemon id in any label", !labels.some((l) => l.includes(conn.id)),
  labels.filter((l) => l.includes(conn.id)).join(" | "));

// The collection paths a daemon always offers, under its own name.
for (const k of ["vms", "networks", "storage", "bridges"]) {
  check(`/nodes/<name>/${k} is offered`, labels.includes(`/nodes/${conn.name}/${k}`));
}

// A leaf, if this host has one: /nodes/<name>/vms/<name>.
const vms = await (await fetch(`${base}/proxy/${conn.id}/domains`)).json();
if (vms[0]) {
  check("a VM is /nodes/<name>/vms/<name>",
    labels.includes(`/nodes/${conn.name}/vms/${vms[0].name}`),
    labels.filter((l) => l.includes("/vms/")).slice(0, 3).join(" | "));
}

// The value is what the server matches on: the id-addressed path.
check("values keep the id-addressed path", values.includes(`/nodes/${conn.id}`) && values.includes("/nodes"),
  values.slice(0, 4).join(" | "));

let bad = 0;
for (const [ok, name, detail] of results) {
  if (!ok) bad++;
  console.log(`${ok ? "ok  " : "FAIL"}  ${name}${state(ok, detail)}`);
}
function state(ok, detail) { return !ok && detail ? `  [${detail}]` : ""; }
console.log(bad ? `\n${bad} failed of ${results.length}` : `\nall ${results.length} passed`);
process.exit(bad ? 1 : 0);
