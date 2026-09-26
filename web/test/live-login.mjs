// Login must show saved servers immediately — no refresh required.
//
// The bug this guards: the login handler rendered the app without calling
// loadConnections(), so the sidebar tree and Datacenter table were empty
// until a reload ran boot() (which does load them). Proves the SPA login path
// populates connections itself.
//
// Auth-enabled suite, so it needs credentials and an instance with no
// valuable state (it creates a connection):
//   NODE_TLS_REJECT_UNAUTHORIZED=0 VM_USER=admin VM_PASS=... \
//     node test/live-login.mjs [baseUrl]

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";

const base = process.argv[2] ?? "https://127.0.0.1:8006";
const USER = process.env.VM_USER ?? "admin";
const PASS = process.env.VM_PASS ?? "password123";

execFileSync(
  "npx",
  ["vite", "build", "--logLevel", "error", "--outDir", "test/_live",
   "--assetsDir", ".", "--target", "es2020"],
  { cwd: process.cwd(), stdio: "ignore", env: { ...process.env, VM_IIFE: "1" } },
);

const code = readFileSync("test/_live/app.iife.js", "utf8");
const cssFile = readdirSync("dist/assets").find((f) => f.endsWith(".css"));
assert.ok(cssFile, "dist/assets contains a built stylesheet (run: pnpm build)");
const css = readFileSync(`dist/assets/${cssFile}`, "utf8");

// Seed a connection so there is a server to appear. Direct API: fetch is used
// with a cookie carried by hand (undici has no jar), same as live-profile.
let cookie = "";
async function api(path, method = "GET", body) {
  const res = await fetch(`${base}${path}`, {
    method,
    headers: {
      ...(cookie ? { Cookie: cookie } : {}),
      ...(body ? { "Content-Type": "application/json" } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
  });
  const sc = res.headers.get("set-cookie");
  const m = sc?.match(/virt-man-session=[^;]+/);
  if (m) cookie = m[0];
  return res;
}

const loginRes = await api("/auth/login", "POST", { username: USER, password: PASS });
assert.equal(loginRes.status, 200, `seeded login as ${USER} failed`);
const addRes = await api("/connections", "POST", {
  name: "login-regression-box",
  url: "https://127.0.0.1:8443",
  token: "vm_placeholder",
  cert: "",
});
assert.equal(addRes.status, 200, `seeding a connection failed: ${await addRes.text()}`);

// Drop the seed cookie so the SPA boots to the login screen, as a fresh visit.
cookie = "";

const virtualConsole = new VirtualConsole();
virtualConsole.on("jsdomError", (e) => {
  if (!/not implemented|canvas|WebSocket/i.test(String(e.message))) {
    console.error("jsdom error:", e.message);
  }
});
const dom = new JSDOM(
  `<!doctype html><html data-theme="dark"><head><style>${css}</style></head>` +
  `<body><div id="app"></div></body></html>`,
  { url: `${base}/`, runScripts: "dangerously", pretendToBeVisual: true, virtualConsole },
);
const { window } = dom;
const { document } = window;

let sessionCookie = "";
window.fetch = (input, init) => {
  const url = typeof input === "string" ? new URL(input, base).toString() : input;
  init = init ?? {};
  if (sessionCookie) init.headers = { ...(init.headers ?? {}), Cookie: sessionCookie };
  return fetch(url, init).then((res) => {
    const sc = res.headers.get("set-cookie");
    const m = sc?.match(/virt-man-session=[^;]+/);
    if (m) sessionCookie = m[0];
    return res;
  });
};
window.eval(code);

const wait = (ms) => new Promise((r) => window.setTimeout(r, ms));
const results = [];
const check = (name, fn) => {
  try {
    fn();
    results.push(["pass", name]);
  } catch (e) {
    results.push(["FAIL", `${name} — ${e.message}`]);
  }
};

await wait(2000);
check("login screen appears", () => {
  assert.ok(document.querySelector("#lu"), "username field");
  assert.ok(document.querySelector("#lp"), "password field");
});

document.querySelector("#lu").value = USER;
document.querySelector("#lp").value = PASS;
document.querySelector("#login-btn").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(2000);

check("the saved server appears in the sidebar without a refresh", () => {
  const treeText = document.querySelector("#tree")?.textContent ?? "";
  assert.ok(treeText.includes("login-regression-box"), `sidebar shows it (got: ${treeText})`);
});
check("the saved server appears in the Datacenter table without a refresh", () => {
  const contentText = document.querySelector("#content")?.textContent ?? "";
  assert.ok(contentText.includes("login-regression-box"), `datacenter shows it (got: ${contentText})`);
});

for (const [status, msg] of results) {
  console.log(`${status === "pass" ? "✔" : "✖"} ${msg}`);
}
window.close();
const failed = results.filter(([s]) => s === "FAIL").length;
if (failed) {
  console.error(`${failed} check(s) failed`);
  process.exit(1);
}
console.log("all login checks passed");
