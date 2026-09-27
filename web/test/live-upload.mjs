// Two-phase upload: stage on the web server, push to the daemon, verify both
// sides by hash, and only then delete the staged copy.
//
// This is the only test that measures the *whole* pipeline rather than either
// half, and it is deliberately filesystem-aware: "the staged copy is gone" is
// the requirement, and nothing proves it as directly as existsSync.
//
// Usage: node test/live-upload.mjs [baseUrl] [dataDir]

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { JSDOM, VirtualConsole } from "jsdom";

const base = process.argv[2] ?? "http://127.0.0.1:8799";
// Where the backend stages bodies. Must be the instance the base URL points at,
// since this test inspects that directory directly.
const dataDir = process.argv[3] ?? process.env.VM_DATA_DIR ?? "/home/kemoi/.local/share/virt-man";
const uploadsDir = join(dataDir, "uploads");

const results = [];
const check = async (name, fn) => {
  try {
    await fn();
    results.push(["pass", name]);
  } catch (e) {
    results.push(["FAIL", `${name} — ${e.message}`]);
  }
};

async function json(path, init) {
  const res = await fetch(new URL(path, base).toString(), init);
  const text = await res.text();
  if (!res.ok) throw new Error(`${path}: HTTP ${res.status} ${text.slice(0, 300)}`);
  return JSON.parse(text);
}

async function classify(path, init) {
  const res = await fetch(new URL(path, base).toString(), init);
  const text = await res.text();
  return { status: res.status, body: text ? JSON.parse(text) : null };
}

const conns = await json("/connections");
if (!Array.isArray(conns) || !conns.length) {
  throw new Error("no saved server on this backend — add one before running live suites");
}
const conn = conns[0].id;
const pool = "default";

// Deterministic bytes, so a failure reproduces and the expected hash can be
// computed rather than merely echoed back by the server under test.
const dir = mkdtempSync(join(tmpdir(), "vm-upload-"));
const size = 400_000;
const bytes = Buffer.alloc(size);
for (let i = 0; i < size; i++) bytes[i] = (i * 7) % 256;
writeFileSync(join(dir, "e2e.iso"), bytes);
const want = createHash("sha256").update(bytes).digest("hex");

const name = `vmtest-upload-${Date.now()}.iso`;
const body = { bytes, headers: { "Content-Type": "application/octet-stream" } };

async function stageUpload(target) {
  return classify(
    `/uploads?conn=${conn}&pool=${pool}&name=${encodeURIComponent(target)}`,
    { method: "POST", headers: body.headers, body: bytes },
  );
}

console.log(`staging ${size} bytes as ${name}`);

let staged = null;

await check("phase 1 stages the body on the web server", async () => {
  const res = await stageUpload(name);
  assert.equal(res.status, 200, `stage failed: ${JSON.stringify(res.body)}`);
  staged = res.body;
  assert.equal(staged.size, size, "server recorded the full size");
  assert.equal(staged.phase, "staged", "not yet pushed");
  assert.ok(existsSync(join(uploadsDir, `${staged.id}.part`)), "body is on disk");
});

await check("phase 2 pushes to the daemon and verifies by hash", async () => {
  await json(`/uploads/${staged.id}/push`, { method: "POST" });
  let job = null;
  for (let i = 0; i < 100; i++) {
    job = await json(`/uploads/${staged.id}`);
    if (job.phase === "done" || job.phase === "failed") break;
    await new Promise((r) => setTimeout(r, 200));
  }
  assert.equal(job.phase, "done", `expected done, got ${job.phase}: ${job.error ?? ""}`);
  assert.equal(job.sent, size, "all bytes were handed to the daemon");
  assert.equal(job.sha256, want, "the verified hash is the source hash");
  staged = job;
});

await check("the staged copy is deleted once both hashes match", () => {
  // The requirement, measured on the filesystem rather than inferred.
  assert.ok(!existsSync(join(uploadsDir, `${staged.id}.part`)), "staged file removed");
});

await check("the daemon really holds those exact bytes", async () => {
  const remote = await json(
    `/proxy/${conn}/pools/${pool}/volumes/${encodeURIComponent(name)}/hash`,
  );
  assert.equal(remote.sha256, want, "daemon content matches the source byte for byte");
  assert.equal(remote.size, size, "and the full length");
});

await check("a name that already exists is refused before the body is sent", async () => {
  // Re-uploading under an existing name must fail fast with a readable message
  // rather than after streaming gigabytes into a raw libvirt error.
  const res = await stageUpload(name);
  assert.equal(res.status, 409, `expected 409, got ${res.status}`);
  assert.match(res.body.error, /already exists/, `unhelpful message: ${res.body.error}`);
});

await check("a refused upload leaves nothing staged behind", () => {
  // A 409 must not leave a multi-GB body in the uploads directory.
  const leftovers = readdirSync(uploadsDir).filter((f) => f.endsWith(".part"));
  assert.deepEqual(leftovers, [], `unexpected staged files: ${leftovers.join(", ")}`);
});

await check("the transfer is recorded in the activity log", async () => {
  // The browser may be gone by the time a transfer ends, so the outcome has to
  // be recorded server-side or a failed upload leaves no trace.
  const log = await json("/activity");
  const mine = log.entries.find((e) => e.label === `Upload ${name}`);
  assert.ok(mine, `no activity entry for Upload ${name}`);
  assert.equal(mine.status, "done", `unexpected status ${mine.status}: ${mine.message}`);
});

// Leave the daemon as we found it.
await fetch(
  new URL(`/proxy/${conn}/pools/${pool}/volumes/${encodeURIComponent(name)}`, base),
  { method: "DELETE" },
).catch(() => undefined);
rmSync(dir, { recursive: true, force: true });


// ------------------------------------------------------------------ the UI
//
// The API checks above prove the pipeline; these prove the *user-visible* half
// you asked for: a progress bar in the prompt while the browser uploads, then a
// background bar on the storage pool window for the daemon leg.

execFileSync("npx", ["vite", "build", "--logLevel", "error", "--outDir", "test/_live"], {
  cwd: process.cwd(), stdio: "inherit", env: { ...process.env, VM_IIFE: "1" },
});
const code = readFileSync("test/_live/app.iife.js", "utf8");
const css = readFileSync(
  `dist/assets/${readdirSync("dist/assets").find((f) => f.endsWith(".css"))}`, "utf8");
const virtualConsole = new VirtualConsole();
virtualConsole.on("jsdomError", (e) => {
  if (!/not implemented|canvas|WebSocket|XMLHttpRequest/i.test(String(e.message))) {
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
window.fetch = (input, init) => {
  const url = typeof input === "string" ? new URL(input, base).toString() : input;
  return fetch(url, init);
};
window.ResizeObserver ??= class { observe() {} unobserve() {} disconnect() {} };
window.eval(code);
const wait = (ms) => new Promise((r) => window.setTimeout(r, ms));
await wait(3000);

// Open a storage pool page and check the prompt carries a progress bar.
const uiName = `vmtest-ui-${Date.now()}.iso`;
let uiStaged = null;
document.querySelector(".tree-pool")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1500);

await check("the pool page offers an Upload ISO action", () => {
  assert.ok(document.querySelector("#pool-upload"), "upload button present on the pool page");
});

document.querySelector("#pool-upload")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(500);
await check("the upload prompt has a progress bar, hidden until it starts", () => {
  const bar = document.querySelector("#up-bar");
  assert.ok(bar, "prompt has a progress bar");
  assert.ok(bar.hidden, "hidden before the upload begins");
  assert.ok(document.querySelector("#up-fill"), "and a fill element to grow");
  assert.ok(document.querySelector("#up-cancel")?.hidden, "cancel hidden until it starts");
});
document.querySelector(".modal-back.open .x")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(200);

// Stage a body with the API, then let the SPA render it: this is exactly what
// the pool window shows for a transfer the browser has already handed off.
uiStaged = await json(`/uploads?conn=${conn}&pool=${pool}&name=${encodeURIComponent(uiName)}`, {
  method: "POST",
  headers: { "Content-Type": "application/octet-stream" },
  body: bytes,
});
document.querySelector(".tree-vm")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(600);
document.querySelector(".tree-pool")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(1500);

await check("the pool window shows a background progress bar for the transfer", () => {
  const section = document.querySelector("#pool-uploads");
  assert.ok(section, "transfers section present");
  assert.match(section.textContent, new RegExp(uiName.replace(/[.]/g, "\\.")), "names the transfer");
  assert.ok(section.querySelector(".meter-fill"), "draws a progress bar");
  assert.match(section.textContent, /Ready/, "and its phase");
});

await check("the bar tracks the transfer through to verified", async () => {
  await json(`/uploads/${uiStaged.id}/push`, { method: "POST" });
  let phase = "";
  for (let i = 0; i < 100; i++) {
    const job = await json(`/uploads/${uiStaged.id}`);
    phase = job.phase;
    if (phase === "done" || phase === "failed") break;
    await new Promise((r) => setTimeout(r, 200));
  }
  assert.equal(phase, "done", `daemon leg did not finish: ${phase}`);
  // Let the page's own poller pick it up, then read what the user would see.
  await wait(1200);
  const section = document.querySelector("#pool-uploads");
  assert.ok(section, "transfers section still present");
  assert.match(section.textContent, /Verified/, `expected Verified, saw: ${section.textContent}`);
  assert.match(section.textContent, /sha256/, "shows the verified checksum");
});

window.close();
await fetch(
  new URL(`/proxy/${conn}/pools/${pool}/volumes/${encodeURIComponent(uiName)}`, base),
  { method: "DELETE" },
).catch(() => undefined);

for (const [status, name] of results) {
  console.log(`${status === "pass" ? "\u2714" : "\u2716"} ${name}`);
}
const failed = results.filter(([s]) => s === "FAIL");
if (failed.length) {
  console.error(`\n${failed.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall upload checks passed");
