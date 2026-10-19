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
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { JSDOM, VirtualConsole } from "jsdom";
import { ensureTestPool, removeTestPool } from "./_fixture.mjs";

const base = process.argv[2] ?? "http://127.0.0.1:8799";
// Where the backend stages bodies. Must be the instance the base URL points at,
// since this test inspects that directory directly.
const dataDir = process.argv[3] ?? process.env.VM_DATA_DIR ?? "/home/kemoi/.local/share/virt-man";
const uploadsDir = join(dataDir, "uploads");

const results = [];
// Everything this run creates, so teardown can remove all of it. Tracking the
// volume names matters as much as the jobs: a check that fails part-way through
// must not leave a stray ISO in the pool for the next run to trip over.
const completedJobIds = [];
const createdVolumes = [];
const trackJob = (id) => {
  completedJobIds.push(id);
  return id;
};
const trackVolume = (vol) => {
  createdVolumes.push(vol);
  return vol;
};
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
const testPool = await ensureTestPool(base, { conn: conns[0].id });
const conn = testPool.conn;
const pool = testPool.name;
const poolUrl = `/proxy/${conn}/pools/${encodeURIComponent(pool)}`;

// Deterministic bytes, so a failure reproduces and the expected hash can be
// computed rather than merely echoed back by the server under test.
const dir = mkdtempSync(join(tmpdir(), "vm-upload-"));
const size = 400_000;
const bytes = Buffer.alloc(size);
for (let i = 0; i < size; i++) bytes[i] = (i * 7) % 256;
writeFileSync(join(dir, "e2e.iso"), bytes);
const want = createHash("sha256").update(bytes).digest("hex");

const name = `vmtest-upload-${Date.now()}.iso`;
trackVolume(name);
const body = { bytes, headers: { "Content-Type": "application/octet-stream" } };

async function stageUpload(target) {
  return classify(
    `/uploads?conn=${conn}&pool=${encodeURIComponent(pool)}&name=${encodeURIComponent(target)}`,
    { method: "POST", headers: body.headers, body: bytes },
  );
}

console.log(`staging ${size} bytes as ${name}`);

let staged = null;

await check("phase 1 stages the body on the web server", async () => {
  const res = await stageUpload(name);
  assert.equal(res.status, 200, `stage failed: ${JSON.stringify(res.body)}`);
  staged = res.body;
  trackJob(staged.id);
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

await check("both legs report a measured speed", async () => {
  // The daemon leg's rate comes from the server's own byte counter...
  assert.ok(staged.speed_bps > 0, `no push rate reported: ${staged.speed_bps}`);
  // ...and the daemon reports its receive and write rates independently.
  assert.ok(staged.daemon_recv_bps > 0, `no daemon receive rate: ${staged.daemon_recv_bps}`);
  assert.ok(staged.daemon_write_bps > 0, `no daemon write rate: ${staged.daemon_write_bps}`);
  // A local transfer cannot exceed a generous ceiling; a unit slip (bytes as
  // KiB, or per-millisecond) would show up as an absurd number here.
  assert.ok(staged.speed_bps < 10e9, `implausible rate: ${staged.speed_bps} B/s`);
});

await check("the finished transfer's speed is recorded in the activity log", async () => {
  const log = await json("/activity");
  const mine = log.entries.find((e) => e.label === `Upload ${name}`);
  assert.ok(mine, "activity entry exists");
  assert.match(mine.message, /(KiB|MiB)\/s/, `no rate in the log message: ${mine.message}`);
});

await check("a transfer finishes with no client watching it", async () => {
  // The daemon leg must not depend on the browser: closing the tab or reloading
  // mid-transfer cannot strand a multi-GB upload. Between starting the push and
  // reading the result, nothing here polls it.
  const solo = `vmtest-solo-${Date.now()}.iso`;
trackVolume(solo);
  const st = await json(`/uploads?conn=${conn}&pool=${encodeURIComponent(pool)}&name=${encodeURIComponent(solo)}`, {
    method: "POST",
    headers: { "Content-Type": "application/octet-stream" },
    body: bytes,
  });
  trackJob(st.id);
  await json(`/uploads/${st.id}/push`, { method: "POST" });
  await new Promise((r) => setTimeout(r, 4000));
  const job = await json(`/uploads/${st.id}`);
  assert.equal(job.phase, "done", `stranded without a client: ${job.phase} ${job.error ?? ""}`);
  assert.equal(job.sha256, want, "and still verified");
  await fetch(new URL(`/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes/${encodeURIComponent(solo)}`, base),
    { method: "DELETE" }).catch(() => undefined);
  await fetch(new URL(`/uploads/${st.id}`, base), { method: "DELETE" }).catch(() => undefined);
});

await check("the daemon really holds those exact bytes", async () => {
  const remote = await json(
    `${poolUrl}/volumes/${encodeURIComponent(name)}/hash`,
  );
  assert.equal(remote.sha256, want, "daemon content matches the source byte for byte");
  assert.equal(remote.size, size, "and the full length");

  // A native pool stores a volume as a regular file, so the list's own path can
  // be measured on disk as well as hashed through the API.
  const vols = await json(`${poolUrl}/volumes`);
  const v = vols.find((x) => x.name === name);
  assert.ok(v?.path, `volume ${name} is listed with its path`);
  assert.equal(statSync(v.path).size, size, "the file on disk has the full length");
});

const stagedParts = () => new Set(readdirSync(uploadsDir).filter((f) => f.endsWith(".part")));

await check("a name that already exists is refused before the body is sent", async () => {
  // Re-uploading under an existing name must fail fast with a readable message
  // rather than after streaming gigabytes into a raw libvirt error.
  const before = stagedParts();
  const res = await stageUpload(name);
  assert.equal(res.status, 409, `expected 409, got ${res.status}`);
  assert.match(res.body.error, /already exists/, `unhelpful message: ${res.body.error}`);

  // A 409 must not stage anything. Compared against a snapshot rather than
  // asserting an empty directory: a transfer that genuinely fails keeps its
  // staged copy on purpose (it is the evidence, and the retry needs it), so
  // leftovers from elsewhere are not this check's business.
  const added = [...stagedParts()].filter((f) => !before.has(f));
  assert.deepEqual(added, [], `the refused upload staged: ${added.join(", ")}`);
});

await check("the transfer is recorded in the activity log", async () => {
  // The browser may be gone by the time a transfer ends, so the outcome has to
  // be recorded server-side or a failed upload leaves no trace.
  const log = await json("/activity");
  const mine = log.entries.find((e) => e.label === `Upload ${name}`);
  assert.ok(mine, `no activity entry for Upload ${name}`);
  assert.equal(mine.status, "done", `unexpected status ${mine.status}: ${mine.message}`);
});

// ------------------------------------------------- deletions are audited
//
// The proxy records every destructive daemon call, which is what answers "who
// deleted this volume, and did it work". The label mapping is unit-tested;
// what these checks cover is that the proxy actually calls it — deleting that
// block would leave every unit test green while the audit trail went silent.
const auditName = `vmtest-audit-${Date.now()}.raw`;

await check("a volume deletion through the proxy is recorded", async () => {
  await json(`/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      xml: `<volume><name>${auditName}</name><capacity>1048576</capacity>` +
        `<target><format type="raw"/></target></volume>`,
    }),
  });
  const del = await fetch(
    new URL(`/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes/${encodeURIComponent(auditName)}`, base),
    { method: "DELETE" },
  );
  assert.ok(del.ok, `delete failed: HTTP ${del.status}`);

  const log = await json("/activity");
  const mine = log.entries.find((e) => e.label.includes(auditName));
  assert.ok(mine, `no audit entry mentioning ${auditName}`);
  assert.equal(mine.status, "done", `unexpected status: ${mine.status}`);
  // The label must name pool and volume: a bare volume name is ambiguous once
  // several pools hold files with the same name.
  assert.match(mine.label, /Delete volume/, `unhelpful label: ${mine.label}`);
  assert.ok(mine.label.includes(pool), `pool missing from label: ${mine.label}`);
});

await check("a refused deletion is recorded as failed with the daemon's reason", async () => {
  // A deletion that did *not* happen is the more important of the two to be
  // able to find afterwards.
  const missing = `vmtest-absent-${Date.now()}.raw`;
  const res = await fetch(
    new URL(`/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes/${encodeURIComponent(missing)}`, base),
    { method: "DELETE" },
  );
  assert.ok(!res.ok, "deleting a missing volume should not succeed");

  const log = await json("/activity");
  const mine = log.entries.find((e) => e.label.includes(missing));
  assert.ok(mine, `no audit entry for the refused delete of ${missing}`);
  assert.equal(mine.status, "failed", "a refused delete must not read as done");
  assert.match(mine.message, /not found|no storage vol/, `reason lost: ${mine.message}`);
});

// Leave the daemon as we found it.
await fetch(
  new URL(`/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes/${encodeURIComponent(name)}`, base),
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

/// Boot a fresh SPA instance. Calling this twice is how a browser reload is
/// simulated: the second page starts with no client-side memory of the first,
/// so anything it knows must have come from the server.
const newPage = async () => {
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
  window.fetch = (input, init) => {
    const url = typeof input === "string" ? new URL(input, base).toString() : input;
    return fetch(url, init);
  };
  window.ResizeObserver ??= class { observe() {} unobserve() {} disconnect() {} };
  window.eval(code);
  const waitFor = (ms) => new Promise((r) => window.setTimeout(r, ms));
  await waitFor(3000);
  return { window, document: window.document, wait: waitFor };
};

/// Click the pool row for the pool this suite uses.
///
/// Deliberately not `document.querySelector(".tree-pool")`: that returns
/// whichever pool sorts first, so a scratch or test pool appearing ahead of the
/// one under test sends this suite's UI checks to the wrong page. Clicking the
/// row by name makes the suite independent of what else is on the host.
const openPoolPage = (doc, win) =>
  doc.querySelector(`.tree-pool[data-pool="${pool}"]`)
    ?.dispatchEvent(new win.MouseEvent("click", { bubbles: true }));

const page = await newPage();
const { window, document, wait } = page;

// Open a storage pool page and check the prompt carries a progress bar.
const uiName = `vmtest-ui-${Date.now()}.iso`;
trackVolume(uiName);
let uiStaged = null;
openPoolPage(document, window);
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
uiStaged = await json(`/uploads?conn=${conn}&pool=${encodeURIComponent(pool)}&name=${encodeURIComponent(uiName)}`, {
  method: "POST",
  headers: { "Content-Type": "application/octet-stream" },
  body: bytes,
});
trackJob(uiStaged.id);
document.querySelector(".tree-vm")?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
await wait(600);
openPoolPage(document, window);
await wait(1500);

await check("the pool window shows a background progress bar for the transfer", () => {
  const section = document.querySelector("#pool-uploads");
  assert.ok(section, "transfers section present");
  assert.match(section.textContent, new RegExp(uiName.replace(/[.]/g, "\\.")), "names the transfer");
  assert.ok(section.querySelector(".meter-fill"), "draws a progress bar");
  assert.match(section.textContent, /Ready/, "and its phase");
});

await check("a staged upload offers to send itself, not just cancel", () => {
  // This is the resume path. A transfer recovered after a restart comes back in
  // exactly this state, so the button under test is the same one that makes a
  // recovered upload recoverable without re-sending the bytes.
  //
  // Scoped to this suite's own job: the section lists every job the server
  // holds, and an earlier run's leftover is what a bare `[data-up-push]` finds.
  const send = document.querySelector(`#pool-uploads [data-up-id="${uiStaged.id}"] [data-up-push]`);
  assert.ok(send, "staged transfer has a Send action");
  assert.match(send.textContent, /Send to daemon/, `unhelpful label: ${send.textContent}`);
});

await check("the pool window shows the daemon leg's speed", async () => {
  // Started by clicking Send, which is the path a recovered transfer takes; the
  // card is then read while the transfer is still in flight, since the speed
  // has to appear live and not only in the final state.
  const send = document.querySelector(`#pool-uploads [data-up-id="${uiStaged.id}"] [data-up-push]`);
  assert.ok(send, "Send button present");
  send.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(500);
  const started = await json(`/uploads/${uiStaged.id}`);
  // A small test body can finish before this read, so "no longer staged" is the
  // claim: the click started it. Requiring "uploading" would be a race.
  assert.notEqual(started.phase, "staged", "Send did not start the transfer");
  let sawSpeed = false;
  for (let i = 0; i < 60; i++) {
    await wait(100);
    const card = document.querySelector(`#pool-uploads [data-up-id="${uiStaged.id}"]`);
    if (card && /(KiB|MiB)\/s/.test(card.textContent)) { sawSpeed = true; break; }
  }
  assert.ok(sawSpeed, "no speed rendered on the transfer card");
});

await check("the bar tracks the transfer through to verified", async () => {
  // The push was already started by the speed check above; pushing again is a
  // 409 once it has completed.
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
  const card = document.querySelector(`#pool-uploads [data-up-id="${uiStaged.id}"]`);
  assert.ok(card, "this transfer's card is still present");
  assert.match(card.textContent, /Verified/, `expected Verified, saw: ${card.textContent}`);
  assert.match(card.textContent, /sha256/, "shows the verified checksum");
});

await check("a freshly loaded page rediscovers work already in progress", async () => {
  // A reload starts a new SPA with no memory of the first, so anything it knows
  // must come from the server. This is what makes a mid-transfer reload safe:
  // the job is listed because the server still holds it, not because the old
  // page remembered it.
  const pending = `vmtest-pending-${Date.now()}.iso`;
trackVolume(pending);
  const st = await json(`/uploads?conn=${conn}&pool=${encodeURIComponent(pool)}&name=${encodeURIComponent(pending)}`, {
    method: "POST",
    headers: { "Content-Type": "application/octet-stream" },
    body: bytes,
  });

  trackJob(st.id);
  const fresh = await newPage();
  openPoolPage(fresh.document, fresh.window);
  await fresh.wait(1800);
  const card = fresh.document.querySelector(`#pool-uploads [data-up-id="${st.id}"]`);
  assert.ok(card, "the in-flight transfer's card is present on the reloaded page");
  assert.ok(
    card.textContent.includes(pending),
    `in-flight transfer not rediscovered: ${card.textContent.slice(0, 160)}`,
  );
  fresh.window.close();

  await fetch(new URL(`/uploads/${st.id}`, base), { method: "DELETE" }).catch(() => undefined);
});

window.close();
await fetch(
  new URL(`/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes/${encodeURIComponent(uiName)}`, base),
  { method: "DELETE" },
).catch(() => undefined);

// Remove the volumes this run created, then its transfer jobs. The pool page
// lists every job the server holds, so leftovers from the API-side checks would
// accumulate there run after run.
for (const vol of createdVolumes) {
  await fetch(
    new URL(`/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes/${encodeURIComponent(vol)}`, base),
    { method: "DELETE" },
  ).catch(() => undefined);
}

// Discard the transfer jobs this run created. They live in the server's memory
// and the pool page lists every job it holds, so leftovers from the API-side
// checks would accumulate there run after run.
for (const id of completedJobIds) {
  await fetch(new URL(`/uploads/${id}`, base), { method: "DELETE" }).catch(() => undefined);
}

await removeTestPool(base, testPool);

for (const [status, name] of results) {
  console.log(`${status === "pass" ? "\u2714" : "\u2716"} ${name}`);
}
const failed = results.filter(([s]) => s === "FAIL");
if (failed.length) {
  console.error(`\n${failed.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall upload checks passed");
