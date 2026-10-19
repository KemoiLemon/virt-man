// The live-suite pool fixture, against a stub backend (no daemon needed).
//
// Pools are the daemon's own directories now, so every storage suite has to
// provision one: reuse an active pool it did not make, or create one at a
// throwaway directory and delete only that. The reuse/cleanup rules are what
// keep a live run from touching the host's own storage, so they are pinned
// here rather than only in suites that need a running daemon.
//
// Run: pnpm test

import assert from "node:assert/strict";
import { createServer } from "node:http";
import { existsSync, rmdirSync } from "node:fs";
import { join } from "node:path";
import test, { after } from "node:test";
import { ensureTestPool, removeTestPool } from "./_fixture.mjs";

const pools = [];
const deleted = [];
const createdBodies = [];

const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

const json = (res, code, body) => {
  res.writeHead(code, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
};

const server = createServer((req, res) => {
  const url = new URL(req.url, "http://x");
  const path = url.pathname;
  if (path === "/connections") return json(res, 200, [{ id: "c1", name: "host" }]);
  if (path === "/proxy/c1/pools" && req.method === "GET") return json(res, 200, pools);
  if (path === "/proxy/c1/pools" && req.method === "POST") {
    let body = "";
    req.on("data", (c) => { body += c; });
    req.on("end", () => {
      createdBodies.push({ body, title: url.searchParams.get("title") });
      const name = body.match(/<name>([^<]+)<\/name>/)?.[1];
      const dir = body.match(/<path>([^<]+)<\/path>/)?.[1];
      pools.push({ name, uuid: name, title: url.searchParams.get("title"), active: true, dir });
      json(res, 200, { name, uuid: name, started: true, dir });
    });
    return;
  }
  const poolPath = path.match(/^\/proxy\/c1\/pools\/([^/]+)(\/xml)?$/);
  if (poolPath) {
    const name = decodeURIComponent(poolPath[1]);
    const p = pools.find((x) => x.name === name);
    if (!p) return json(res, 404, { error: "no pool" });
    if (poolPath[2] && req.method === "GET") {
      return json(res, 200, {
        name,
        xml: `<pool type='dir'><name>${name}</name><uuid>${name}</uuid>` +
          `<target><path>${p.dir}</path></target></pool>`,
      });
    }
    if (req.method === "DELETE") {
      deleted.push(name);
      pools.splice(pools.indexOf(p), 1);
      return json(res, 200, { name, undefined: true });
    }
  }
  json(res, 404, { error: `unexpected ${req.method} ${path}` });
});

await new Promise((r) => server.listen(0, "127.0.0.1", r));
const base = `http://127.0.0.1:${server.address().port}`;
const testRoot = join(process.cwd(), "test", "_pools");

after(() => {
  server.close();
  // Non-recursive on purpose: another suite's run may own a pool under this
  // parent, and only an empty one is this test's to remove.
  try { rmdirSync(testRoot); } catch { /* not empty, or already gone */ }
});

test("a host with no pool gets one created at a directory the daemon allows", async () => {
  const pool = await ensureTestPool(base);
  assert.equal(pool.created, true);
  assert.match(pool.name, uuid, "the pool is named by a canonical UUID");
  assert.equal(pool.title, "vmtest-pool");
  assert.equal(createdBodies.length, 1);
  assert.match(createdBodies[0].body, /<pool type='dir'>/, "a native directory pool");
  assert.equal(createdBodies[0].title, "vmtest-pool", "the display name arrives as ?title=");
  assert.ok(pool.path.startsWith(testRoot), `inside the suite's own directory: ${pool.path}`);
  assert.ok(!/^\/(var\/)?tmp\//.test(pool.path), "never a directory the daemon reserves");
  assert.ok(existsSync(pool.path), "the directory exists before the daemon is asked to use it");
  // The list carries no directory, so a reused pool is read back from its XML.
  const again = await ensureTestPool(base);
  assert.equal(again.created, false);
  assert.equal(again.path, pool.path);
  await removeTestPool(base, pool);
  assert.deepEqual(deleted, [pool.name], "the definition it created is deleted");
  assert.ok(!existsSync(pool.path), "and its throwaway directory is gone");
});

test("a pool the suite did not make is reused, never deleted", async () => {
  pools.push({ name: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", uuid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", title: "images", active: true, dir: "/srv/images" });
  const before = deleted.length;
  const pool = await ensureTestPool(base);
  assert.equal(pool.created, false);
  assert.equal(pool.title, "images");
  assert.equal(pool.path, "/srv/images");
  await removeTestPool(base, pool);
  assert.equal(deleted.length, before, "a reused pool is left alone");
  assert.equal(pools.length, 1);
});
