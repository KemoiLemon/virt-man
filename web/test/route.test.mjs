// URL <-> view mapping. A reload must land where the user was, so every view
// must survive a round trip through its path, and a path that names something
// that no longer exists must degrade to a view that does.
//
// Run: pnpm test

import assert from "node:assert/strict";
import test, { before } from "node:test";
import { readdirSync, readFileSync } from "node:fs";

let r;
before(async () => {
  r = await import("./_build/route.js");
});

const nodes = [
  { id: "c1", name: "Main" },
  { id: "c2", name: "lab box" },
];

test("every view round-trips through its path", () => {
  const views = [
    { view: "dc", tab: "dashboard" },
    { view: "dc", tab: "servers" },
    { view: "dc", tab: "guests" },
    { view: "conn", conn: "c1", tab: "dashboard" },
    { view: "conn", conn: "c1", tab: "vms" },
    { view: "conn", conn: "c1", tab: "networks" },
    { view: "conn", conn: "c1", tab: "storage" },
    { view: "conn", conn: "c1", tab: "firewall" },
    { view: "vm", conn: "c1", vm: "web1", tab: "summary" },
    { view: "vm", conn: "c1", vm: "web1", tab: "console" },
    { view: "vm", conn: "c1", vm: "web1", tab: "hardware" },
    { view: "vm", conn: "c1", vm: "web1", tab: "options" },
    { view: "vm", conn: "c1", vm: "web1", tab: "network" },
    { view: "vm", conn: "c1", vm: "web1", tab: "xml" },
    { view: "net", conn: "c2", net: "default" },
    { view: "pool", conn: "c2", pool: "ISOs" },
  ];
  for (const v of views) {
    const path = r.routeToPath(v, nodes);
    assert.deepEqual(r.parsePath(path, nodes), v, `${path}`);
  }
});

test("paths use the node's name and read the way they were asked for", () => {
  assert.equal(r.routeToPath({ view: "dc", tab: "dashboard" }, nodes), "/");
  assert.equal(r.routeToPath({ view: "conn", conn: "c1", tab: "dashboard" }, nodes), "/Main");
  assert.equal(r.routeToPath({ view: "vm", conn: "c1", vm: "web1", tab: "summary" }, nodes), "/Main/vms/web1");
  assert.equal(r.routeToPath({ view: "vm", conn: "c1", vm: "web1", tab: "console" }, nodes), "/Main/vms/web1/console");
  assert.equal(r.routeToPath({ view: "net", conn: "c1", net: "lan" }, nodes), "/Main/networks/lan");
  assert.equal(r.routeToPath({ view: "conn", conn: "c1", tab: "storage" }, nodes), "/Main/storage");
  assert.equal(r.routeToPath({ view: "pool", conn: "c1", pool: "ISOs" }, nodes), "/Main/storage/ISOs");
});

test("names with spaces and slashes are encoded, and decode back", () => {
  const v = { view: "vm", conn: "c2", vm: "a/b c", tab: "summary" };
  const path = r.routeToPath(v, nodes);
  assert.equal(path, "/lab%20box/vms/a%2Fb%20c");
  assert.deepEqual(r.parsePath(path, nodes), v);
});

// The web server answers /users, /activity, ... itself, so a node with such a
// name would reload into JSON. It must be addressed by id instead.
test("a node named like a server route is addressed by its id", () => {
  const ns = [{ id: "c9", name: "users" }, { id: "c8", name: "Servers" }];
  assert.equal(r.routeToPath({ view: "conn", conn: "c9", tab: "vms" }, ns), "/c9/vms");
  assert.equal(r.routeToPath({ view: "conn", conn: "c8", tab: "dashboard" }, ns), "/c8");
});

test("two nodes sharing a name are addressed by id, so neither is ambiguous", () => {
  const ns = [{ id: "a", name: "host" }, { id: "b", name: "host" }];
  const pa = r.routeToPath({ view: "conn", conn: "a", tab: "dashboard" }, ns);
  const pb = r.routeToPath({ view: "conn", conn: "b", tab: "dashboard" }, ns);
  assert.deepEqual([pa, pb], ["/a", "/b"]);
  assert.equal(r.parsePath("/host", ns).view, "dc", "an ambiguous name resolves to nothing");
});

test("a node's id always works, even when a name is used in links", () => {
  assert.deepEqual(r.parsePath("/c1/networks/lan", nodes), { view: "net", conn: "c1", net: "lan" });
});

test("anything unrecognised lands on the nearest view that exists", () => {
  const dc = { view: "dc", tab: "dashboard" };
  assert.deepEqual(r.parsePath("/", nodes), dc);
  assert.deepEqual(r.parsePath("/gone-node/vms/x", nodes), dc, "deleted node");
  assert.deepEqual(r.parsePath("/%E0%A4%A", nodes), dc, "malformed escape");
  assert.deepEqual(r.parsePath("/Main/bogus", nodes), { view: "conn", conn: "c1", tab: "dashboard" });
  assert.deepEqual(r.parsePath("/Main/vms/web1/bogus", nodes), { view: "vm", conn: "c1", vm: "web1", tab: "summary" });
  assert.deepEqual(r.parsePath("/Main/firewall/x", nodes), { view: "conn", conn: "c1", tab: "firewall" });
});

// A node named like one of the web server's own top-level routes would reload
// into that route's JSON. RESERVED must cover every one of them, so read them
// straight from the server's source instead of trusting a list kept by hand.
test("every top-level server route is reserved", () => {
  const dir = "server/src/api";
  const tops = new Set();
  for (const f of readdirSync(dir).filter((f) => f.endsWith(".rs"))) {
    for (const m of readFileSync(`${dir}/${f}`, "utf8").matchAll(/\.route\(\s*"\/([a-z0-9_-]+)/g)) tops.add(m[1]);
  }
  assert.ok(tops.size >= 5, `found server routes: ${[...tops]}`);
  for (const top of tops) assert.ok(r.RESERVED.has(top), `/${top} is a server route but not reserved`);
});

// The VM Firewall tab became part of the Network tab; a bookmark to the old
// path must still land on the tab that now holds the firewall.
test("an old /vms/<vm>/firewall link opens the Network tab", () => {
  assert.deepEqual(r.parsePath("/Main/vms/web1/firewall", nodes), { view: "vm", conn: "c1", vm: "web1", tab: "network" });
  assert.equal(r.routeToPath({ view: "vm", conn: "c1", vm: "web1", tab: "network" }, nodes), "/Main/vms/web1/network");
});

// The admin page is a view of its own, with a URL per section.
test("the admin page round-trips through the URL", () => {
  for (const section of r.ADMIN_SECTIONS) {
    const route = { view: "admin", section };
    assert.deepEqual(r.parsePath(r.routeToPath(route, nodes), nodes), route, section);
  }
  assert.equal(r.routeToPath({ view: "admin", section: "users" }, nodes), "/admin");
  assert.deepEqual(r.parsePath("/admin/bogus", nodes), { view: "admin", section: "users" });
  // A server named "admin" must not shadow the admin page.
  assert.equal(r.nodeSegment("c9", [{ id: "c9", name: "admin" }]), "c9");
});
