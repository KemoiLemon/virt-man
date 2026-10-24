// Chart time range: live (the last minute, from the 1s poll) or a stored
// range read from the web server's history (`GET /history`), plus the hover
// readout every chart shares.
//
// Ranges are read from the server, not accumulated in the page: the server
// samples every host whether or not a browser is open, so "the last day"
// covers hours nobody was looking.

import { gridSeries, pointAt, readout, type Timing } from "./stats";

export interface Range { key: string; label: string; ms: number; axis: string }

export const RANGES: Range[] = [
  { key: "live", label: "Live", ms: 60_000, axis: "-1m" },
  { key: "1h", label: "1h", ms: 3_600_000, axis: "-1h" },
  { key: "6h", label: "6h", ms: 6 * 3_600_000, axis: "-6h" },
  { key: "1d", label: "1d", ms: 24 * 3_600_000, axis: "-1d" },
  { key: "7d", label: "7d", ms: 7 * 24 * 3_600_000, axis: "-7d" },
  { key: "14d", label: "14d", ms: 14 * 24 * 3_600_000, axis: "-14d" },
];

const STORE_KEY = "virt-man.range";
let current: Range = RANGES.find((r) => r.key === localStorage.getItem(STORE_KEY)) ?? RANGES[0];

export function range(): Range { return current; }
export function isLive(): boolean { return current.key === "live"; }

export function setRange(key: string): boolean {
  const r = RANGES.find((x) => x.key === key);
  if (!r || r === current) return false;
  current = r;
  localStorage.setItem(STORE_KEY, key);
  return true;
}

/// The range buttons, for a dashboard or summary header.
export function rangeBar(): string {
  return `<div class="range-bar" role="group" aria-label="Chart Range">${RANGES.map((r) =>
    `<button class="btn range-btn ${r === current ? "on" : ""}" data-range="${r.key}" aria-pressed="${r === current}">${r.label}</button>`).join("")}</div>`;
}

export interface HistorySeries { a: (number | null)[]; b: (number | null)[]; timing: Timing }

/// One stored series over the current range, on a complete time grid.
/// `params` picks the series: {kind, conn?, name?, sub?}.
export async function loadHistory(
  get: <T>(url: string) => Promise<T>,
  params: Record<string, string>,
): Promise<HistorySeries> {
  const since = Date.now() - current.ms;
  const qs = new URLSearchParams({ ...params, since_ms: String(since), points: "240" });
  const r = await get<{ points: [number, number | null, number | null][]; step_ms: number; now_ms: number }>(
    `/history?${qs}`);
  const g = gridSeries(r.points, r.now_ms - current.ms, r.now_ms, r.step_ms);
  return { a: g.a, b: g.b, timing: { t0: g.t0, step: r.step_ms } };
}

// ------------------------------------------------------------ hover readout

/// One document-level handler, so it survives the 1s re-render of every
/// chart without re-binding: the chart's values ride on its data attributes.
export function bindChartHover(doc: Document = document) {
  let tip: HTMLElement | null = null;
  let mark: HTMLElement | null = null;
  const hide = () => { tip?.remove(); mark?.remove(); tip = mark = null; };
  const show = (e: PointerEvent) => {
    const svg = (e.target as Element | null)?.closest?.("svg.spark") as SVGElement | null;
    if (!svg) return hide();
    const values = (svg.dataset.v ?? "").split(",").map((s) => (s === "" ? null : Number(s)));
    const box = svg.getBoundingClientRect();
    if (!box.width || !values.length) return hide();
    const i = pointAt(values.length, (e.clientX - box.left) / box.width);
    const step = Number(svg.dataset.step);
    const t = Number(svg.dataset.t0) + i * step;
    if (!tip) {
      tip = doc.createElement("div");
      tip.className = "spark-tip";
      mark = doc.createElement("div");
      mark.className = "spark-mark";
      doc.body.append(tip, mark);
    }
    tip.textContent = readout(values[i], svg.dataset.unit ?? "pct", t, step * values.length);
    const x = box.left + (values.length > 1 ? (i / (values.length - 1)) * box.width : 0);
    if (mark) {
      mark.style.left = `${x}px`;
      mark.style.top = `${box.top}px`;
      mark.style.height = `${box.height}px`;
    }
    // Above the chart, kept inside the viewport.
    const w = tip.offsetWidth;
    const left = Math.max(4, Math.min(doc.documentElement.clientWidth - w - 4, x - w / 2));
    tip.style.left = `${left}px`;
    tip.style.top = `${Math.max(4, box.top - tip.offsetHeight - 6)}px`;
  };
  // pointerdown as well as pointermove: a touch device has no hover, so a tap
  // is the only way to read a point. Tapping away (the `!svg` branch above)
  // dismisses it again.
  doc.addEventListener("pointermove", show);
  doc.addEventListener("pointerdown", show);
  doc.addEventListener("pointerleave", hide);
  doc.addEventListener("scroll", hide, true);
}
