// A storage pool must report the filesystem as it is, not as libvirt last
// cached it.
//
// This is the gap that matters on this host: the ISO pool directory is fed by
// aria2 and friends, so files appear without going through the volume API, and
// libvirt will not notice them until something refreshes the pool. A pool page
// polls every second, so the wrong answer would repeat forever rather than fix
// itself.
//
// The suite provisions its own scratch pool through the daemon API, writes into
// it with plain filesystem calls (which is what an external downloader does),
// and never calls the refresh endpoint — refreshing on read is the behaviour
// under test.
//
// Usage: node test/live-pool.mjs [baseUrl]

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdirSync, readFileSync, readdirSync, rmSync, statfsSync, writeFileSync } from "node:fs";
import { JSDOM, VirtualConsole } from "jsdom";

const base = process.argv[2] ?? "http://127.0.0.1:8799";
const poolName = `vmtest-pool-${Date.now()}`;
// Disk-backed, not tmpfs: the pool's space figures come from the filesystem, and
// this keeps them meaningful.
const poolDir = `/var/tmp/${poolName}`;

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
  return text ? JSON.parse(text) : null;
}

/// Used bytes of the filesystem holding the pool, which is exactly what a `dir`
/// pool reports as its own `allocation`. Comparing against this rather than a
/// before/after delta is deliberate: the pool sits on the root filesystem, so
/// unrelated churn can free more than a test writes and a delta would flake.
function fsUsed() {
  const s = statfsSync(poolDir);
  return (s.blocks - s.bfree) * s.bsize;
}

const conns = await json("/connections");
if (!Array.isArray(conns) || !conns.length) {
  throw new Error("no saved server on this backend — add one before running live suites");
}
const conn = conns[0].id;
const proxy = (p) => `/proxy/${conn}${p}`;
const poolPath = proxy(`/pools/${encodeURIComponent(poolName)}`);

const outsideFile = "arrived-from-outside.iso";
// Size matters twice over. A `dir` pool reports its *filesystem's* used space,
// and the pool sits on the root filesystem, so the assertion below survives only
// if the write is far bigger than unrelated churn; and btrfs commits that figure
// lazily, hence the sync. Measured without a refresh-on-read this leaves the
// pool reporting a value ~445 MB short, so the check is sharp at this size.
const outsideBytes = 256 * 1024 * 1024;

function cleanup() {
  rmSync(poolDir, { recursive: true, force: true });
}

/// Delete any scratch pool a previous run left behind.
///
/// `vmtest-pool-*` is this suite's own namespace, so this can only match its own
/// leaks — but those matter: a stale pool sorts ahead of `default` in the tree,
/// and a suite that clicks the first pool row then inspects the wrong page.
/// Leaving one behind broke live-upload exactly that way.
async function sweepStalePools() {
  const pools = await json(proxy("/pools")).catch(() => []);
  for (const p of pools) {
    if (String(p.name).startsWith("vmtest-pool-")) {
      await fetch(new URL(proxy(`/pools/${encodeURIComponent(p.name)}`), base), {
        method: "DELETE",
      }).catch(() => undefined);
      rmSync(`/var/tmp/${p.name}`, { recursive: true, force: true });
    }
  }
}

await sweepStalePools();
cleanup();
mkdirSync(poolDir, { recursive: true });

// Everything below is inside try/finally: a failure part-way through must
// still delete the scratch pool, or the next suite inherits a pool it did
// not create. That is not hypothetical — a leaked pool here broke
// live-upload, whose pool-page checks then inspected the wrong pool.
try {
  // A dir pool whose target is our scratch directory. `build` creates the
  // directory and `create` starts it, both of which the daemon does for us.
  await json(proxy("/pools"), {
    method: "POST",
    headers: { "Content-Type": "application/xml" },
    body: `<pool type='dir'><name>${poolName}</name><target><path>${poolDir}</path></target></pool>`,
  });
  console.log(`scratch pool ${poolName} at ${poolDir}`);

  const names = async () => (await json(proxy(`/pools/${poolName}/volumes`))).map((v) => v.name);

  await check("a new pool starts empty", async () => {
    assert.deepEqual(await names(), [], "expected no volumes in a fresh pool");
  });

  // ------------------------------------------------- the filesystem action
  // Written directly, never through the volume API: exactly what an aria2 or scp
  // download into the pool directory looks like to libvirt.
  const blob = Buffer.alloc(outsideBytes);
  for (let i = 0; i < blob.length; i++) blob[i] = (i * 7) % 256;
  writeFileSync(`${poolDir}/${outsideFile}`, blob);
  execFileSync("sync");

  await check("a file written straight into the pool is listed without a refresh", async () => {
    // No call to POST /pools/{name}/refresh anywhere in this suite: the read
    // itself has to bring the pool up to date.
    const listed = await names();
    assert.ok(
      listed.includes(outsideFile),
      `file invisible to the pool: ${JSON.stringify(listed)}`,
    );
  });

  await check("the pool's space figures match the filesystem, not a cached value", async () => {
    const onDisk = fsUsed();
    const reported = (await json(poolPath)).allocation;
    // A stale answer is the value cached when the pool was created, which is now
    // short by the file written above; a fresh one is the filesystem's own used
    // space, give or take whatever else touched it between these two reads.
    const gap = Math.abs(reported - onDisk);
    // Read within ~100 ms of each other, so churn between them is negligible
    // while a cached answer is out by the whole file.
    assert.ok(
      gap <= 16 * 1024 * 1024,
      `pool allocation looks cached: reported=${reported} filesystem=${onDisk} ` +
        `(gap ${gap} bytes, and this test just wrote ${outsideBytes})`,
    );
  });

  await check("the extra volume reports its own size", async () => {
    const vols = await json(proxy(`/pools/${poolName}/volumes`));
    const v = vols.find((x) => x.name === outsideFile);
    assert.ok(v, "volume listed");
    assert.ok(v.capacity >= outsideBytes, `capacity too small: ${v.capacity}`);
  });

  await check("removing the file outside the volume API is reflected too", async () => {
    rmSync(`${poolDir}/${outsideFile}`);
    const listed = await names();
    assert.ok(!listed.includes(outsideFile), `stale entry still listed: ${JSON.stringify(listed)}`);
  });

  // ------------------------------------------------------- the user-visible half
  // The pool page must render what the refreshing read returns, otherwise the
  // daemon is right and the user still sees a stale table.
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

  // Write a second file, then let the page's own polling pick it up. This is the
  // real sequence: something lands in the pool while the page is open.
  const lateFile = "arrived-while-watching.raw";
  writeFileSync(`${poolDir}/${lateFile}`, Buffer.alloc(1024));

  document.querySelector(`.tree-pool[data-pool="${poolName}"]`)
    ?.dispatchEvent(new window.MouseEvent("click", { bubbles: true }));
  await wait(2500);

  await check("the open pool page lists a file that appeared while it was open", () => {
    const table = document.querySelector("#content table.grid");
    assert.ok(table, "volume table rendered");
    assert.match(
      table.textContent,
      new RegExp(lateFile.replace(/[.]/g, "\\.")),
      `late file missing from the page: ${table.textContent.slice(0, 200)}`,
    );
  });

  window.close();
} finally {
  await fetch(new URL(poolPath, base), { method: "DELETE" }).catch(() => undefined);
  cleanup();
}

for (const [status, name] of results) {
  console.log(`${status === "pass" ? "\u2714" : "\u2716"} ${name}`);
}
const failed = results.filter(([s]) => s === "FAIL");
if (failed.length) {
  console.error(`\n${failed.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall storage pool checks passed");
