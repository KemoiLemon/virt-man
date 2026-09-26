// Profile popover + full-profile modal, against the real auth-enabled backend.
//
// The system service serves HTTPS with a self-signed cert, so this one runs
// with NODE_TLS_REJECT_UNAUTHORIZED=0 (Node's undici accepts the cert):
//   NODE_TLS_REJECT_UNAUTHORIZED=0 node test/live-profile.mjs [baseUrl]
//
// jsdom cannot do file input or canvas, so the crop dialog itself is not
// exercised here — crop math is covered by crop.test.mjs.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";

const base = process.argv[2] ?? "https://127.0.0.1:8006";

execFileSync(
  "npx",
  ["vite", "build", "--logLevel", "error", "--outDir", "test/_live",
   "--assetsDir", ".", "--target", "es2020"],
  { cwd: process.cwd(), stdio: "inherit", env: { ...process.env, VM_IIFE: "1" } },
);

const code = readFileSync("test/_live/app.iife.js", "utf8");
const cssFile = readdirSync("dist/assets").find((f) => f.endsWith(".css"));
assert.ok(cssFile, "dist/assets contains a built stylesheet (run: pnpm build)");
const css = readFileSync(`dist/assets/${cssFile}`, "utf8");

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

// undici has no cookie jar, so carry the session cookie from login responses
// back into subsequent requests (the app's API relies on the session cookie).
let sessionCookie = "";
window.fetch = (input, init) => {
  const url = typeof input === "string" ? new URL(input, base).toString() : input;
  init = init ?? {};
  if (sessionCookie) {
    init.headers = { ...(init.headers ?? {}), Cookie: sessionCookie };
  }
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

// Sign in as the seeded admin (admin / password123).
document.querySelector("#lu").value = "admin";
document.querySelector("#lp").value = "password123";
document.querySelector("#login-btn").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1600);

check("profile button renders in the sidebar after login", () => {
  assert.ok(document.querySelector("#profile-btn"), "profile button present");
  assert.ok(document.querySelector("#profile-btn .avatar"), "avatar shown in the button");
});

// Open the popover.
document.querySelector("#profile-btn").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(300);

check("profile popover opens with avatar, username and password", () => {
  const menu = document.querySelector(".pop-menu");
  assert.ok(menu, "popover present");
  assert.ok(menu.querySelector("#pm-avatar .avatar"), "avatar in the popover");
  assert.ok(menu.querySelector("#pm-user"), "username field");
  assert.ok(menu.querySelector("#pm-pass"), "current-password field");
  assert.ok(menu.querySelector("#pm-pass2"), "new-password field");
  assert.ok(menu.querySelector("#pm-user-save"), "save username button");
  assert.ok(menu.querySelector("#pm-pass-save"), "change password button");
});

check("full-profile button opens the modal with nickname and API keys", () => {
  document.querySelector("#pm-full").dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  const modal = document.querySelector(".modal-back.open");
  assert.ok(modal, "full profile modal open");
  assert.ok(!document.querySelector(".pop-menu"), "popover closed when the modal opened");
  assert.ok(modal.querySelector("#pf-nick"), "nickname field");
  assert.ok(modal.querySelector("#pf-avatar-btn .avatar"), "avatar change button");
  assert.ok(modal.querySelector("#pf-keys"), "API key list");
  assert.ok(modal.querySelector("#pf-newkey"), "new API key button");
});

const fails = results.filter(([s]) => s === "FAIL");
for (const [s, msg] of results) console.log(`${s === "pass" ? "✔" : "✖"} ${msg}`);
window.close();
if (fails.length) {
  console.log(`\n${fails.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall profile checks passed");
