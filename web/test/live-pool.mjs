// A storage pool is a directory, and a read is a scan of it.
//
// Nothing here talks to libvirt's storage driver: files arrive by `cp`, aria2 or
// a sync without going through the volume API, and a pool page polls every
// second, so a cached answer would repeat forever rather than fix itself. The
// suite provisions its own scratch pool through the daemon, writes into it with
// plain filesystem calls (which is what an external downloader does), and never
// calls the refresh endpoint — reading the directory is the behaviour under
// test. `allocation` is the blocks the files in that directory hold, not the
// filesystem's used space.
//
// Usage: node test/live-pool.mjs [baseUrl]

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import {
  lstatSync, readFileSync, readdirSync, rmSync, statfsSync, writeFileSync,
} from "node:fs";
import { join } from "node:path";
import { JSDOM, VirtualConsole } from "jsdom";
import { ensureTestPool, removeTestPool } from "./_fixture.mjs";

const base = process.argv[2] ?? "http://127.0.0.1:8799";

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

const conns = await json("/connections");
if (!Array.isArray(conns) || !conns.length) {
  throw new Error("no saved server on this backend — add one before running live suites");
}
const conn = conns[0].id;
const pool = await ensureTestPool(base, { conn });
const poolDir = pool.path;
if (!poolDir) throw new Error(`pool ${pool.name} did not report a target directory`);
const proxy = (p) => `/proxy/${conn}${p}`;
const volumesPath = proxy(`/pools/${encodeURIComponent(pool.name)}/volumes`);
const poolPath = proxy(`/pools/${encodeURIComponent(pool.name)}`);

const apiVolume = `vmtest-api-${Date.now()}.raw`;
const outsideFile = "arrived-from-outside.iso";
// Small on purpose: allocation is now the blocks these files hold, so the
// assertion below does not need a write large enough to beat filesystem churn
// (the old libvirt-backed check compared against the whole filesystem).
const outsideBytes = 1024 * 1024;
const outsideBlob = Buffer.alloc(outsideBytes);
for (let i = 0; i < outsideBlob.length; i++) outsideBlob[i] = (i * 7) % 256;

/// What the daemon reports as the pool's allocation: every regular file's
/// blocks, dot-prefixed temporaries included (`pools.rs` `allocation_of`).
function blocksInPool() {
  return readdirSync(poolDir)
    .map((name) => lstatSync(join(poolDir, name)))
    .filter((m) => m.isFile())
    .reduce((sum, m) => sum + m.blocks * 512, 0);
}

function fsUsed() {
  const s = statfsSync(poolDir);
  return (s.blocks - s.bfree) * s.bsize;
}

try {
  const names = async () => (await json(volumesPath)).map((v) => v.name);

  await check("a volume created through the API is a file in the pool directory", async () => {
    const made = await json(volumesPath, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        xml: `<volume><name>${apiVolume}</name><capacity unit="MiB">1</capacity>` +
          `<target><format type="raw"/></target></volume>`,
      }),
    });
    assert.equal(made.path, join(poolDir, apiVolume), "the API reports the file it wrote");
    const vols = await json(volumesPath);
    const v = vols.find((x) => x.name === apiVolume);
    assert.ok(v, `not listed: ${JSON.stringify(vols.map((x) => x.name))}`);
    assert.equal(v.capacity, 1024 * 1024, "capacity is the file's own length");
  });

  // ------------------------------------------------- the filesystem action
  // Written directly, never through the volume API: exactly what an aria2 or scp
  // download into the pool directory looks like.
  writeFileSync(join(poolDir, outsideFile), outsideBlob);
  execFileSync("sync");

  await check("a file written straight into the pool is listed without a refresh", async () => {
    // No call to POST /pools/{name}/refresh anywhere in this suite: the read
    // itself scans the directory.
    const listed = await names();
    assert.ok(
      listed.includes(outsideFile),
      `file invisible to the pool: ${JSON.stringify(listed)}`,
    );
  });

  await check("the pool's allocation is the sum of its files' blocks", async () => {
    const expected = blocksInPool();
    const reported = (await json(poolPath)).allocation;
    assert.equal(
      reported, expected,
      `allocation is not the files' st_blocks: reported=${reported} blocks=${expected}`,
    );
    // A `dir` pool's allocation used to be the filesystem's used space. It is
    // not any more, and on a host with an OS installed the two differ hugely.
    const used = fsUsed();
    if (used > expected + 64 * 1024 * 1024) {
      assert.ok(reported < used, `allocation looks like filesystem used: ${reported} vs ${used}`);
    }
  });

  await check("the extra volume reports its own size", async () => {
    const vols = await json(volumesPath);
    const v = vols.find((x) => x.name === outsideFile);
    assert.ok(v, "volume listed");
    assert.equal(v.capacity, outsideBytes, `capacity is the file length: ${v.capacity}`);
  });

  await check("removing the file outside the volume API is reflected too", async () => {
    rmSync(join(poolDir, outsideFile));
    const listed = await names();
    assert.ok(!listed.includes(outsideFile), `stale entry still listed: ${JSON.stringify(listed)}`);
  });

  await check("deleting a volume through the API removes its file", async () => {
    await json(`${volumesPath}/${encodeURIComponent(apiVolume)}`, { method: "DELETE" });
    assert.ok(!(await names()).includes(apiVolume), "still listed after delete");
    assert.throws(() => lstatSync(join(poolDir, apiVolume)), "file still on disk");
  });

  // ------------------------------------------------------- the user-visible half
  // The pool page must render what the scanning read returns, otherwise the
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
  writeFileSync(join(poolDir, lateFile), Buffer.alloc(1024));

  document.querySelector(`.tree-pool[data-pool="${pool.name}"]`)
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

  rmSync(join(poolDir, lateFile), { force: true });
  window.close();
} finally {
  await removeTestPool(base, pool);
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
