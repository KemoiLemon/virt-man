// Update pace: the poll's reads are delivered together, at most once a gap.
//
// The poll used to repaint as each answer landed, so answers a few ms apart
// (or a second read chained behind the first) were several visible updates.
// Reads that are not the poll's (a click's) must not wait for a slot.
//
// Run: pnpm test

import assert from "node:assert/strict";
import test from "node:test";

const { api, setUpdateGap, pollWindow, pendingReads } = await import("./_build/api.js");
setUpdateGap(120);

const answer = (ms, status = 200) => new Promise((r) =>
  setTimeout(() => r(new Response(JSON.stringify(status === 200 ? {} : { error: "boom" }), { status })), ms));
const wait = (ms) => new Promise((r) => setTimeout(r, ms));

test("answers that arrive apart are delivered together", async () => {
  globalThis.fetch = (url) => answer(String(url).endsWith("a") ? 5 : 40);
  await wait(130); // let any earlier slot pass
  const t0 = Date.now();
  const at = {};
  await new Promise((done) => pollWindow(() => {
    Promise.all([
      api.get("/a").then(() => { at.a = Date.now() - t0; }),
      api.get("/b").then(() => { at.b = Date.now() - t0; }),
    ]).then(done);
  }));
  assert.ok(Math.abs(at.a - at.b) < 10, `one wave (${at.a} vs ${at.b})`);
});

test("a read chained behind the first lands a full gap later", async () => {
  globalThis.fetch = () => answer(2);
  await wait(130);
  const stamps = [];
  await new Promise((done) => pollWindow(() => {
    api.get("/one").then(() => {
      stamps.push(Date.now());
      // A loader asking for what it needs next, from inside the delivery.
      return api.get("/two");
    }).then(() => { stamps.push(Date.now()); done(); });
  }));
  assert.ok(stamps[1] - stamps[0] >= 110, `at least a gap apart (${stamps[1] - stamps[0]} ms)`);
});

test("a read outside the poll is not held", async () => {
  globalThis.fetch = () => answer(2);
  await new Promise((done) => pollWindow(() => { api.get("/spend").then(done); }));
  await wait(5); // a person's click is its own task, not a continuation of the poll
  const t0 = Date.now();
  await api.get("/click");
  assert.ok(Date.now() - t0 < 60, `answered at once (${Date.now() - t0} ms)`);
});

test("only reads are paced", async () => {
  globalThis.fetch = () => answer(2);
  let t0 = 0;
  await new Promise((done) => pollWindow(() => {
    t0 = Date.now();
    api.post("/write", {}).then(done);
  }));
  assert.ok(Date.now() - t0 < 60, "a write goes straight through");
});

test("a read counts as pending until it is delivered, not just until it answers", async () => {
  globalThis.fetch = () => answer(10);
  // Spend the slot, so the next answer has to wait for one.
  await new Promise((done) => pollWindow(() => { api.get("/spend").then(done); }));
  await wait(5);
  let p;
  pollWindow(() => { p = api.get("/x"); });
  assert.equal(pendingReads(), 1, "on the network");
  await wait(40);
  assert.equal(pendingReads(), 1, "answered but held for its slot: the next round must not start");
  await p;
  assert.equal(pendingReads(), 0, "delivered");
});

test("a failed read still reaches its caller, in the same wave", async () => {
  globalThis.fetch = (url) => answer(2, String(url).endsWith("bad") ? 500 : 200);
  await wait(130);
  let failed;
  await new Promise((done) => pollWindow(() => {
    Promise.all([
      api.get("/ok"),
      api.get("/bad").catch((e) => { failed = e.message; }),
    ]).then(done);
  }));
  assert.equal(failed, "boom");
});
