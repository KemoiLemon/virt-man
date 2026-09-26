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

/** Current series without recording a new sample. */
export function peekSeries(name: string): { cpu: number[]; mem: number[] } {
  const entry = series.get(name) ?? blank();
  return { cpu: entry.cpu.slice(), mem: entry.mem.slice() };
}

// ------------------------------------------------------------- rendering

const W = 240;
const H = 74;

/**
 * Sparkline as inline SVG on a fixed 0..100 scale, with gridlines, a filled
 * area, the trace, and a marker on the newest value at the right edge.
 * `viewBox` + `preserveAspectRatio=none` lets it stretch to the panel width
 * while the stroke stays 1px via `vector-effect` in CSS.
 */
export function sparkline(values: number[], variant = ""): string {
  const pts = values.length ? values : Array.from({ length: SERIES_LEN }, () => 0);
  const step = W / Math.max(1, pts.length - 1);
  const y = (v: number) => H - (clampPercent(v) / 100) * H;
  const coords = pts.map((v, i) => `${(i * step).toFixed(2)},${y(v).toFixed(2)}`);
  const line = `M${coords.join(" L")}`;
  const area = `${line} L${W},${H} L0,${H} Z`;
  const lastY = y(pts[pts.length - 1] ?? 0);

  const grid = [0.25, 0.5, 0.75]
    .map((f) => `<line class="spark-grid" x1="0" y1="${(H * f).toFixed(1)}" x2="${W}" y2="${(H * f).toFixed(1)}"/>`)
    .join("");

  return `<svg class="spark ${variant}" viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" aria-hidden="true">
    ${grid}
    <path class="spark-fill" d="${area}"/>
    <path class="spark-line" d="${line}"/>
    <circle class="spark-dot" cx="${W}" cy="${lastY.toFixed(2)}" r="2.2"/>
  </svg>`;
}

/** Percentage formatted for a metric headline. */
export function pct(n: number): string {
  return `${clampPercent(n).toFixed(n >= 10 ? 0 : 1)}%`;
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
