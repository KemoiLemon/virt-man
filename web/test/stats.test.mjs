// Sparkline series + CPU derivation checks (task-7 contract).
//
// The daemon reports a monotonic cpu_time_ns counter, so "CPU %" only exists
// as a delta between samples. These tests pin that maths plus the
// pre-filled, right-to-left advancing series the charts depend on.
//
// Run: pnpm test

import assert from "node:assert/strict";
import test, { before } from "node:test";

let stats;

before(async () => {
  stats = await import("./_build/stats.js");
});

const sample = (over = {}) => ({
  name: "vm1",
  state: "running",
  cpu_time_ns: 0,
  vcpus: 1,
  memory_kib: 1024 * 1024,
  max_memory_kib: 1024 * 1024,
  rss_kib: null,
  usable_kib: null,
  host: { cpus: 8, memory_kib: 32 * 1024 * 1024, mhz: 4000, model: "x86_64" },
  sampled_at_ms: 0,
  ...over,
});

test("series starts pre-filled so the chart spans the full window", () => {
  const { cpu, mem } = stats.addSample(sample({ name: "prefill" }));
  assert.equal(cpu.length, stats.SERIES_LEN, "cpu series is full length");
  assert.equal(mem.length, stats.SERIES_LEN, "mem series is full length");
});

test("first sample cannot compute CPU% (no previous counter)", () => {
  const r = stats.addSample(sample({ name: "first", cpu_time_ns: 5e9, sampled_at_ms: 1000 }));
  assert.equal(r.cpuNow, 0, "no delta yet, so 0%");
});

test("CPU% is the counter delta over elapsed time and vCPUs", () => {
  const name = "cpu-math";
  stats.addSample(sample({ name, cpu_time_ns: 0, sampled_at_ms: 0 }));
  // 1 full second of CPU time across 1s of wall time on 1 vCPU == 100%.
  let r = stats.addSample(sample({ name, cpu_time_ns: 1e9, sampled_at_ms: 1000 }));
  assert.equal(Math.round(r.cpuNow), 100, "saturated single vCPU is 100%");

  // Half a second of CPU over 1s == 50%.
  r = stats.addSample(sample({ name, cpu_time_ns: 1.5e9, sampled_at_ms: 2000 }));
  assert.equal(Math.round(r.cpuNow), 50, "half-busy is 50%");

  // Same work spread over 2 vCPUs halves the percentage.
  r = stats.addSample(sample({ name, cpu_time_ns: 2.5e9, sampled_at_ms: 3000, vcpus: 2 }));
  assert.equal(Math.round(r.cpuNow), 50, "1s CPU / 1s wall / 2 vCPU is 50%");
});

test("a restarted domain (counter reset) does not report negative CPU", () => {
  const name = "reset";
  stats.addSample(sample({ name, cpu_time_ns: 9e9, sampled_at_ms: 0 }));
  const r = stats.addSample(sample({ name, cpu_time_ns: 1e8, sampled_at_ms: 1000 }));
  assert.ok(r.cpuNow >= 0, `cpuNow must not go negative, got ${r.cpuNow}`);
});

test("CPU% is clamped to 0..100", () => {
  const name = "clamp";
  stats.addSample(sample({ name, cpu_time_ns: 0, sampled_at_ms: 0 }));
  const r = stats.addSample(sample({ name, cpu_time_ns: 99e9, sampled_at_ms: 100 }));
  assert.ok(r.cpuNow <= 100, `clamped, got ${r.cpuNow}`);
});

test("series advances right-to-left with newest pinned to the right", () => {
  const name = "advance";
  stats.addSample(sample({ name, cpu_time_ns: 0, sampled_at_ms: 0 }));
  // Three successive fully-busy seconds.
  for (let i = 1; i <= 3; i++) {
    stats.addSample(sample({ name, cpu_time_ns: i * 1e9, sampled_at_ms: i * 1000 }));
  }
  const { cpu } = stats.peekSeries(name);
  assert.equal(cpu.length, stats.SERIES_LEN, "length stays fixed");
  assert.equal(Math.round(cpu[cpu.length - 1]), 100, "newest value is last (right edge)");
  assert.equal(cpu[0], 0, "oldest pre-filled zeros shift off the left");
});

test("a stopped domain reports zero CPU and memory", () => {
  const name = "stopped";
  stats.addSample(sample({ name, cpu_time_ns: 0, sampled_at_ms: 0 }));
  const r = stats.addSample(
    sample({ name, state: "shutoff", cpu_time_ns: 5e9, sampled_at_ms: 1000 }),
  );
  assert.equal(r.cpuNow, 0, "no CPU when not running");
  assert.equal(r.memNow, 0, "no memory when not running");
});

test("memory% prefers balloon 'usable', falling back to RSS", () => {
  // usable: 1 GiB max, 256 MiB usable -> 768 MiB used -> 75%
  let r = stats.addSample(sample({
    name: "mem-usable",
    memory_kib: 1024 * 1024,
    max_memory_kib: 1024 * 1024,
    usable_kib: 256 * 1024,
  }));
  assert.equal(Math.round(r.memNow), 75, "uses balloon usable when present");

  // rss only: 512 MiB of 1 GiB -> 50%
  r = stats.addSample(sample({
    name: "mem-rss",
    memory_kib: 1024 * 1024,
    max_memory_kib: 1024 * 1024,
    rss_kib: 512 * 1024,
  }));
  assert.equal(Math.round(r.memNow), 50, "falls back to RSS");
});

test("sparkline renders a full-width trace", () => {
  const svg = stats.sparkline([0, 50, 100]);
  assert.match(svg, /class="spark"/, "carries the .spark class");
  assert.match(svg, /preserveAspectRatio="none"/, "stretches to the panel");
  assert.match(svg, /class="spark-grid"/, "has gridlines");
  assert.match(svg, /class="spark-fill"/, "has the filled area");
  assert.match(svg, /class="spark-line"/, "has the trace");
  // 100% must sit at the top (y=0) and 0% at the bottom (y=74).
  assert.match(svg, /L240\.00,0\.00/, "max value reaches the top of the chart");
});

test("sparkline variants select the themed colour class", () => {
  assert.match(stats.sparkline([1, 2], "mem"), /class="spark mem"/);
});

test("addDiskSample tracks a clamped disk-usage series", () => {
  const s = stats.addDiskSample("dc", 42);
  assert.equal(s.length, 60, "a full 60-sample window");
  assert.equal(s[59], 42, "newest sample pinned to the right edge");
  assert.equal(stats.addDiskSample("dc", 150)[59], 100, "clamps to 100");
  assert.equal(stats.addDiskSample("dc", -5)[59], 0, "clamps to 0");
});

test("fmtBytes renders human-readable sizes", () => {
  assert.equal(stats.fmtBytes(0), "0 B");
  assert.equal(stats.fmtBytes(null), "—");
  assert.equal(stats.fmtBytes(1024), "1.0 KiB");
  assert.equal(stats.fmtBytes(1024 ** 3), "1.0 GiB");
});

test("avgRateKib averages over the whole transfer", () => {
  // 2 MiB in 2s = 1 MiB/s = 1024 KiB/s.
  assert.equal(stats.avgRateKib(2 * 1024 * 1024, 0, 2000), 1024);
  assert.equal(stats.avgRateKib(512 * 1024, 1000, 2000), 512);
});

test("avgRateKib reports nothing until a rate is meaningful", () => {
  // Zero elapsed time would otherwise read as Infinity, and no bytes yet as a
  // misleadingly authoritative 0 KiB/s.
  assert.equal(stats.avgRateKib(1000, 500, 500), null);
  assert.equal(stats.avgRateKib(1000, 500, 400), null);
  assert.equal(stats.avgRateKib(0, 0, 1000), null);
});

test("avgRateKib withholds a rate from the first, spiky progress event", () => {
  // Measured in jsdom: the first progress event lands within milliseconds of
  // the send, so 1 MiB "in 10ms" would display as ~100 GiB/s.
  const spike = stats.avgRateKib(1024 * 1024, 0, 10);
  assert.equal(spike, null, "a 10ms window must not produce a rate");
  // Past the minimum window it reports again.
  assert.equal(stats.avgRateKib(1024 * 1024, 0, 1000), 1024);
});

// A page painted from the web server's cache and then from a fresh read hands
// the series the same sample twice. The repeat must not be folded in: a zero
// time delta would push a false 0% point into the chart on every page open.
test("a repeated sample does not push a point", () => {
  stats.addSample(sample({ name: "rep", cpu_time_ns: 0, sampled_at_ms: 1000 }));
  const a = stats.addSample(sample({ name: "rep", cpu_time_ns: 5e8, sampled_at_ms: 2000 }));
  assert.ok(a.cpuNow > 0, "real delta gives real CPU");
  const again = stats.addSample(sample({ name: "rep", cpu_time_ns: 5e8, sampled_at_ms: 2000 }));
  assert.deepEqual(again.cpu, a.cpu, "series unchanged by the repeat");
  assert.equal(again.cpuNow, a.cpuNow, "current value is the last real one");
});

test("a repeated host sample does not push a point", () => {
  const h = (total, idle, at) => ({ cpu_total: total, cpu_idle: idle, mem_total_kib: 100, mem_available_kib: 50, cpus: 4, sampled_at_ms: at });
  stats.addHostSample("rep-host", h(0, 0, 1000));
  const a = stats.addHostSample("rep-host", h(100, 50, 2000));
  const again = stats.addHostSample("rep-host", h(100, 50, 2000));
  assert.deepEqual(again.cpu, a.cpu);
  assert.equal(again.cpuNow, 50);
});

test("a repeated NIC sample does not push a point", () => {
  stats.addNetSample("rep-nic", 0, 0, 1000);
  const a = stats.addNetSample("rep-nic", 2048, 1024, 2000);
  const again = stats.addNetSample("rep-nic", 2048, 1024, 2000);
  assert.deepEqual(again.rx, a.rx);
  assert.equal(again.rxNow, 2, "2 KiB over 1 s");
});
