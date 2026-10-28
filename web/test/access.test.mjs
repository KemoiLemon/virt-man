// Access-log view helpers: the query a filter becomes, and how sorting,
// grouping and drilling into a group change the filter.
//
// Run: pnpm test

import assert from "node:assert/strict";
import test, { before } from "node:test";

let a;
before(async () => {
  a = await import(new URL("./_build/access.js", import.meta.url).href);
});

test("a filter becomes the server's query", () => {
  const f = { ...a.defaultFilter("web"), proto: "tcp", q: " github.com ", port: "443", group: "domain", kind: "connections", direction: "in", page: 2 };
  const p = new URLSearchParams(a.logQuery("c1", f, 1_000_000_000));
  assert.equal(p.get("conn"), "c1");
  assert.equal(p.get("vm"), "web");
  assert.equal(p.get("proto"), "tcp");
  assert.equal(p.get("q"), "github.com", "trimmed");
  assert.equal(p.get("port"), "443");
  assert.equal(p.get("group"), "domain");
  assert.equal(p.get("kind"), "connections");
  assert.equal(p.get("direction"), "in");
  assert.equal(p.get("offset"), String(2 * a.PAGE));
  assert.equal(p.get("since_ms"), String(1_000_000_000 - 86_400_000), "last 24 h by default");
  const bare = new URLSearchParams(a.logQuery("c1", { ...a.defaultFilter(), port: "abc" }, 0));
  assert.equal(bare.get("port"), null, "a non-numeric port is not sent");
  assert.equal(bare.get("vm"), null);
  assert.equal(bare.get("kind"), null, "both is the default and is not sent");
  assert.equal(bare.get("direction"), null, "in and out is the default and is not sent");
});

test("clicking a column flips it; a new column starts at its natural order", () => {
  let f = a.defaultFilter();
  assert.equal(f.sort, "time");
  f = a.toggleSort(f, "time");
  assert.equal(f.dir, "asc", "same column: oldest first now");
  f = a.toggleSort(f, "time");
  assert.equal(f.dir, "desc");
  f = a.toggleSort({ ...f, page: 3 }, "bytes");
  assert.deepEqual([f.sort, f.dir, f.page], ["bytes", "desc", 0], "biggest first, back to page one");
  f = a.toggleSort(f, "domain");
  assert.deepEqual([f.sort, f.dir], ["domain", "asc"], "names A to Z");
});

test("grouping sorts by count, and a group drills into its connections", () => {
  let f = a.withGroup(a.defaultFilter(), "domain");
  assert.deepEqual([f.group, f.sort, f.dir], ["domain", "count", "desc"], "most accessed first");
  assert.deepEqual([a.drillDown(f, "github.com").group, a.drillDown(f, "github.com").q], ["", "github.com"]);
  assert.equal(a.drillDown(a.withGroup(f, "proto"), "udp").proto, "udp");
  assert.equal(a.drillDown(a.withGroup(f, "port"), "443").port, "443");
  assert.equal(a.drillDown(a.withGroup(f, "vm"), "db").vm, "db");
  f = a.withGroup(f, "");
  assert.equal(f.sort, "time", "back to connections, newest first");
});

test("the direction filter narrows to one way, and the sort marker reads naturally", () => {
  assert.deepEqual(Object.keys(a.DIR_LABEL), ["", "out", "in"], "both, then each way");
  const f = { ...a.defaultFilter(), sort: "bytes", dir: "asc", direction: "out" };
  assert.equal(new URLSearchParams(a.logQuery("c1", f, 0)).get("direction"), "out");
  assert.equal(a.sortMark(f, "bytes"), " ▲");
  assert.equal(a.sortMark(f, "time"), "");
});

test("live tail applies only to the newest, ungrouped first page", () => {
  const f = a.defaultFilter();
  assert.ok(a.liveApplies(f));
  assert.ok(!a.liveApplies({ ...f, group: "domain" }));
  assert.ok(!a.liveApplies({ ...f, dir: "asc" }));
  assert.ok(!a.liveApplies({ ...f, page: 1 }));
  assert.ok(!a.liveApplies({ ...f, sort: "bytes" }));
  const back = a.forLive({ ...f, group: "ip", sort: "count", dir: "asc", page: 3, vm: "web", kind: "dns" });
  assert.ok(a.liveApplies(back), "switching to live gives the newest-first view");
  assert.deepEqual([back.vm, back.kind], ["web", "dns"], "…keeping the filters");
});
