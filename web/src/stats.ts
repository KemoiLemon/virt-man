// Live CPU/memory series for the dashboard sparklines.
//
// The daemon reports `cpu_time_ns`, a monotonic counter, so a single reading
// cannot express "CPU %". We keep the previous sample per domain and divide
// the delta by elapsed wall time and the vCPU count.
//
// Each series starts pre-filled with zero samples so the chart spans the full
// window from the first paint; new samples are appended and the oldest
// dropped, which makes the trace advance right-to-left with the newest value
// pinned to the right edge (same behaviour as vm-man).

export const SERIES_LEN = 60;

export interface DomainStats {
  name: string;
  state: string;
  cpu_time_ns: number;
  vcpus: number;
  memory_kib: number;
  max_memory_kib: number;
  rss_kib: number | null;
  usable_kib: number | null;
  host: { cpus: number; memory_kib: number; mhz: number; model: string };
  sampled_at_ms: number;
}

/// Host-wide metrics from `GET /host/stats`, for the daemon dashboard.
export interface HostStats {
  cpu_total: number;
  cpu_idle: number;
  mem_total_kib: number;
  mem_available_kib: number;
  cpus: number;
  sampled_at_ms: number;
}

interface Series {
  cpu: number[];
  mem: number[];
  prev?: { cpu_time_ns: number; sampled_at_ms: number };
}

const series = new Map<string, Series>();

function blank(): Series {
  return {
    cpu: Array.from({ length: SERIES_LEN }, () => 0),
    mem: Array.from({ length: SERIES_LEN }, () => 0),
  };
}

function push(list: number[], value: number) {
  list.push(value);
  while (list.length > SERIES_LEN) list.shift();
}

/** Memory used as a percentage of the domain's maximum. */
function memPercent(s: DomainStats): number {
  const max = s.max_memory_kib || s.memory_kib;
  if (!max) return 0;
  // `usable` (balloon) is the most accurate when present; RSS is the
  // host-side fallback; otherwise fall back to the allocation itself.
  const used =
    s.usable_kib != null ? Math.max(0, s.memory_kib - s.usable_kib)
    : s.rss_kib != null ? s.rss_kib
    : s.memory_kib;
  return clampPercent((used / max) * 100);
}

function clampPercent(n: number): number {
  if (!Number.isFinite(n)) return 0;
  return Math.max(0, Math.min(100, n));
}

/** Fold one sample into a domain's series and return the current values. */
export function addSample(s: DomainStats): { cpu: number[]; mem: number[]; cpuNow: number; memNow: number } {
  const key = s.name;
  let entry = series.get(key);
  if (!entry) {
    entry = blank();
    series.set(key, entry);
  }

  // A sample no newer than the last one is a repeat (a cached answer painted
  // before the fresh read): folding it in would push a false 0% point.
  if (entry.prev && s.sampled_at_ms <= entry.prev.sampled_at_ms) {
    return { cpu: entry.cpu.slice(), mem: entry.mem.slice(), cpuNow: entry.cpu.at(-1) ?? 0, memNow: entry.mem.at(-1) ?? 0 };
  }

  let cpuNow = 0;
  if (entry.prev) {
    const dtMs = s.sampled_at_ms - entry.prev.sampled_at_ms;
    const dCpuNs = s.cpu_time_ns - entry.prev.cpu_time_ns;
    // A restarted domain resets the counter; treat a negative delta as idle.
    if (dtMs > 0 && dCpuNs >= 0) {
      const vcpus = Math.max(1, s.vcpus);
      cpuNow = clampPercent((dCpuNs / 1e6 / dtMs / vcpus) * 100);
    }
  }
  entry.prev = { cpu_time_ns: s.cpu_time_ns, sampled_at_ms: s.sampled_at_ms };

  const memNow = s.state === "running" ? memPercent(s) : 0;
  if (s.state !== "running") cpuNow = 0;

  push(entry.cpu, cpuNow);
  push(entry.mem, memNow);
  return { cpu: entry.cpu.slice(), mem: entry.mem.slice(), cpuNow, memNow };
}

/** Drop a domain's history (e.g. after deletion). */
export function forgetSeries(name: string) {
  series.delete(name);
}

/// Host CPU/memory history, keyed by connection id so each daemon dashboard
/// keeps its own series. CPU is a jiffy-counter delta; memory is instantaneous.
interface HostSeries {
  cpu: number[];
  mem: number[];
  prev?: { cpu_total: number; cpu_idle: number; sampled_at_ms: number };
}

const hostSeries = new Map<string, HostSeries>();

/** Fold one host sample into its series and return the current values. */
export function addHostSample(
  key: string,
  s: HostStats,
): { cpu: number[]; mem: number[]; cpuNow: number; memNow: number } {
  let entry = hostSeries.get(key);
  if (!entry) {
    entry = { cpu: blank().cpu, mem: blank().mem };
    hostSeries.set(key, entry);
  }

  // Same repeat guard as addSample.
  if (entry.prev && s.sampled_at_ms <= entry.prev.sampled_at_ms) {
    return { cpu: entry.cpu.slice(), mem: entry.mem.slice(), cpuNow: entry.cpu.at(-1) ?? 0, memNow: entry.mem.at(-1) ?? 0 };
  }

  let cpuNow = 0;
  if (entry.prev) {
    const dtMs = s.sampled_at_ms - entry.prev.sampled_at_ms;
    const dTotal = s.cpu_total - entry.prev.cpu_total;
    const dIdle = s.cpu_idle - entry.prev.cpu_idle;
    if (dtMs > 0 && dTotal > 0) {
      cpuNow = clampPercent(100 * (1 - dIdle / dTotal));
    }
  }
  entry.prev = { cpu_total: s.cpu_total, cpu_idle: s.cpu_idle, sampled_at_ms: s.sampled_at_ms };

  const memNow = s.mem_total_kib
    ? clampPercent(100 * (1 - s.mem_available_kib / s.mem_total_kib))
    : 0;

  push(entry.cpu, cpuNow);
  push(entry.mem, memNow);
  return { cpu: entry.cpu.slice(), mem: entry.mem.slice(), cpuNow, memNow };
}

/// Per-key disk-usage history (the Datacenter dashboard's Total Disk graph).
/// Disk has no host-stats source: it is derived from storage-pool allocation,
/// which is only known client-side, so it gets its own series.
const diskSeries = new Map<string, number[]>();

export function addDiskSample(key: string, pctValue: number): number[] {
  let entry = diskSeries.get(key);
  if (!entry) {
    entry = blank().cpu;
    diskSeries.set(key, entry);
  }
  push(entry, clampPercent(pctValue));
  return entry.slice();
}

/// Per-NIC byte-rate history for the VM summary's Network card. Byte counters
/// are cumulative, so each sample folds a delta into KiB/s and pushes it onto
/// rx/tx series of raw rates.
interface NetSeries {
  rx: number[];
  tx: number[];
  prev?: { rx_bytes: number; tx_bytes: number; sampled_at_ms: number };
}

const netSeries = new Map<string, NetSeries>();

export function addNetSample(
  key: string,
  rxBytes: number,
  txBytes: number,
  sampledAtMs: number,
): { rx: number[]; tx: number[]; rxNow: number; txNow: number } {
  let entry = netSeries.get(key);
  if (!entry) {
    entry = { rx: blank().cpu, tx: blank().mem };
    netSeries.set(key, entry);
  }
  // Same repeat guard as addSample.
  if (entry.prev && sampledAtMs <= entry.prev.sampled_at_ms) {
    return { rx: entry.rx.slice(), tx: entry.tx.slice(), rxNow: entry.rx.at(-1) ?? 0, txNow: entry.tx.at(-1) ?? 0 };
  }
  let rxNow = 0;
  let txNow = 0;
  if (entry.prev) {
    const dtS = (sampledAtMs - entry.prev.sampled_at_ms) / 1000;
    const dRx = rxBytes - entry.prev.rx_bytes;
    const dTx = txBytes - entry.prev.tx_bytes;
    // A restarted guest resets the counters; treat a negative delta as idle.
    if (dtS > 0 && dRx >= 0) rxNow = (dRx / 1024) / dtS;
    if (dtS > 0 && dTx >= 0) txNow = (dTx / 1024) / dtS;
  }
  entry.prev = { rx_bytes: rxBytes, tx_bytes: txBytes, sampled_at_ms: sampledAtMs };
  push(entry.rx, rxNow);
  push(entry.tx, txNow);
  return { rx: entry.rx.slice(), tx: entry.tx.slice(), rxNow, txNow };
}

/** Drop a NIC's rate history. */
export function forgetNetSeries(key: string) {
  netSeries.delete(key);
}

/** Current series without recording a new sample. */
export function peekSeries(name: string): { cpu: number[]; mem: number[] } {
  const entry = series.get(name) ?? blank();
  return { cpu: entry.cpu.slice(), mem: entry.mem.slice() };
}

// ------------------------------------------------------------- rendering

const W = 240;
const H = 74;

/// When a chart's points were sampled: the first point's time and the gap
/// between points. Carried on the SVG so the hover readout can name a time.
export interface Timing { t0: number; step: number }

/**
 * Sparkline as inline SVG on a 0..max scale (default 0..100), with
 * gridlines, a filled area and the trace. `viewBox` +
 * `preserveAspectRatio=none` lets it stretch to the panel width while the
 * stroke stays 1px via `vector-effect` in CSS.
 *
 * A `null` value is a gap (no sample: host unreachable, web server down), and
 * breaks the line rather than drawing a false zero. The values, their timing
 * and unit ride along as data attributes for the hover readout.
 */
export function sparkline(values: (number | null)[], variant = "", max = 100, timing?: Timing): string {
  const pts = values.length ? values : Array.from({ length: SERIES_LEN }, () => 0);
  const ceil = Math.max(1, max);
  const step = W / Math.max(1, pts.length - 1);
  const y = (v: number) => H - (Math.max(0, Math.min(ceil, v)) / ceil) * H;
  let line = "";
  let area = "";
  let run: string[] = [];
  const flush = () => {
    if (!run.length) return;
    const first = run[0].split(",")[0];
    const last = run[run.length - 1].split(",")[0];
    line += `M${run.join(" L")}`;
    area += `M${first},${H} L${run.join(" L")} L${last},${H} Z`;
    run = [];
  };
  pts.forEach((v, i) => {
    if (v == null) flush();
    else run.push(`${(i * step).toFixed(2)},${y(v).toFixed(2)}`);
  });
  flush();

  const grid = [0.25, 0.5, 0.75]
    .map((f) => `<line class="spark-grid" x1="0" y1="${(H * f).toFixed(1)}" x2="${W}" y2="${(H * f).toFixed(1)}"/>`)
    .join("");
  const unit = variant === "rx" || variant === "tx" ? "rate" : "pct";
  // Live series are one poll apart and end now.
  const when = timing ?? { t0: Date.now() - (pts.length - 1) * 1000, step: 1000 };
  const data = pts.map((v) => (v == null ? "" : +v.toFixed(2))).join(",");

  return `<svg class="spark${variant ? ` ${variant}` : ""}" viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" aria-hidden="true"
    data-v="${data}" data-t0="${Math.round(when.t0)}" data-step="${Math.round(when.step)}" data-unit="${unit}">
    ${grid}
    ${area ? `<path class="spark-fill" d="${area}"/>` : ""}
    ${line ? `<path class="spark-line" d="${line}"/>` : ""}
  </svg>`;
}

/// The point under a pointer `fraction` (0..1) of the way across a chart of
/// `n` points: the nearest one, since points sit on the edges, not in cells.
export function pointAt(n: number, fraction: number): number {
  if (n <= 1) return 0;
  return Math.max(0, Math.min(n - 1, Math.round(fraction * (n - 1))));
}

/// Hover readout for one point: "14:32:05 · 23.4%", or "no data" in a gap.
export function readout(value: number | null, unit: string, t: number, spanMs: number): string {
  const d = new Date(t);
  const two = (n: number) => String(n).padStart(2, "0");
  const clock = `${two(d.getHours())}:${two(d.getMinutes())}${spanMs < 6 * 3600_000 ? `:${two(d.getSeconds())}` : ""}`;
  // Past a day the time of day alone is ambiguous.
  const when = spanMs > 24 * 3600_000 ? `${d.getMonth() + 1}/${d.getDate()} ${clock}` : clock;
  if (value == null) return `${when} · no data`;
  return `${when} · ${unit === "rate" ? fmtRate(value) : pct(value)}`;
}

/// Lay history buckets onto a complete time grid, so the x axis is time and a
/// bucket with no samples is a gap rather than being silently skipped (which
/// would stitch the hours either side of an outage together).
///
/// The server stamps each bucket at its middle, aligned to multiples of
/// `step` since the epoch; the grid uses the same alignment.
export function gridSeries(
  rows: [number, number | null, number | null][],
  sinceMs: number,
  nowMs: number,
  step: number,
): { a: (number | null)[]; b: (number | null)[]; t0: number } {
  const byT = new Map(rows.map((r) => [r[0], r] as const));
  const start = Math.floor(sinceMs / step) * step + step / 2;
  const a: (number | null)[] = [];
  const b: (number | null)[] = [];
  for (let t = start; t <= nowMs; t += step) {
    const r = byT.get(t);
    a.push(r?.[1] ?? null);
    b.push(r?.[2] ?? null);
  }
  return { a, b, t0: start };
}

/** Percentage formatted for a metric headline. */
export function pct(n: number): string {
  return `${clampPercent(n).toFixed(n >= 10 ? 0 : 1)}%`;
}

/** Bytes-per-second (KiB/s based input) formatted for a metric footer. */
export function fmtRate(kibPerSec: number): string {
  if (kibPerSec < 1024) return `${kibPerSec.toFixed(kibPerSec >= 10 ? 0 : 1)} KiB/s`;
  return `${(kibPerSec / 1024).toFixed(1)} MiB/s`;
}

/// Shortest window over which a transfer rate is considered meaningful. The
/// first progress event arrives only milliseconds into a transfer, so a rate
/// computed from it is a spike rather than a measurement.
const MIN_RATE_MS = 250;

/// Average transfer rate in KiB/s, or null while the figure is meaningless.
///
/// Averaged over the whole transfer on purpose: a per-tick rate swings with
/// buffer sizes and poll timing, so it reads as noise. Null is returned rather
/// than 0 for a transfer too young to measure, and nothing is reported under
/// `MIN_RATE_MS` — the first progress event arrives a few milliseconds in, so a
/// rate computed from it is a meaningless spike (megabytes sent in 20 ms reads
/// as gigabytes per second). A transfer that finishes inside that window is
/// over before a speed would have been useful anyway.
export function avgRateKib(
  bytes: number,
  startedMs: number,
  nowMs: number,
  minMs = MIN_RATE_MS,
): number | null {
  const secs = (nowMs - startedMs) / 1000;
  if (!(secs > 0) || !(bytes > 0) || nowMs - startedMs < minMs) return null;
  return bytes / 1024 / secs;
}

/** Human-readable bytes; KiB-valued inputs should be scaled by the caller. */
export function fmtBytes(n: number | null | undefined): string {
  if (n == null) return "—";
  if (n === 0) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(1)} ${units[i]}`;
}
