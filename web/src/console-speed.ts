// Console speed preset: how much latency to trade for a coherent image.
//
// The daemon keeps the guest framebuffer and hands out the changed rectangle,
// so this is the one real knob the pipeline has. `settle` is a window during
// which damage is allowed to accumulate in the daemon; `Frame::touch` merges it,
// so the browser receives one bigger rectangle instead of several partial
// paints arriving over one repaint. That is exactly what tearing is, and paying
// a few ms for it is the trade.
//
// It is NOT a pixel-quality setting. Nothing is downscaled or re-encoded on the
// way through, because the bytes are already RGBA for `putImageData` and
// re-encoding them would mean decoding a codec in the browser on every frame.
// The presets choose coherence, not fidelity.

export interface Speed {
  key: string;
  label: string;
  /** Damage-coalescing window handed to the daemon, in ms. */
  settleMs: number;
  /** What the choice actually does, for the title/tooltip. */
  hint: string;
}

export const SPEEDS: Speed[] = [
  {
    key: "fast",
    label: "Fast",
    settleMs: 0,
    hint: "Lowest latency — a frame leaves as soon as it changes, so fast motion can tear.",
  },
  {
    key: "balanced",
    label: "Balanced",
    settleMs: 33,
    hint: "Up to ~30 updates a second: small repaints are merged, so less tearing with negligible added delay.",
  },
  {
    key: "quality",
    label: "Quality",
    settleMs: 120,
    hint: "Repaints are collected before they are sent, so the screen arrives as whole images. Slowest, least tearing.",
  },
];

const STORE_KEY = "virt-man.console-speed";
// Balanced, as the default. An unknown or corrupt stored value falls back to it
// rather than leaving the picker on a preset that does not exist.
const DEFAULT = SPEEDS[1];

/// Storage, guarded: the viewer's frame loop reads this on every request, and
/// it outlives any one render. A block-storage-less context (a private-mode
/// browser, a jsdom import in a unit test) would otherwise throw from inside
/// that loop and stop the console painting entirely, for a setting that is only
/// ever an optimisation.
function read(): string | null {
  try {
    return globalThis.localStorage?.getItem(STORE_KEY) ?? null;
  } catch {
    return null;
  }
}

function write(key: string) {
  try {
    globalThis.localStorage?.setItem(STORE_KEY, key);
  } catch { /* a preference that cannot be saved is not a failure */ }
}

export function speed(): Speed {
  const key = read();
  return SPEEDS.find((s) => s.key === key) ?? DEFAULT;
}

export function setSpeed(key: string): Speed {
  const s = SPEEDS.find((x) => x.key === key) ?? DEFAULT;
  write(s.key);
  return s;
}

/// The picker, as a native `<select>` for `enhanceSelects` to turn into the
/// themed listbox. Built here so the toolbar markup stays declarative.
export function speedOptions(): string {
  const cur = speed();
  return SPEEDS.map(
    (s) =>
      `<option value="${s.key}"${s === cur ? " selected" : ""} title="${s.hint}">${s.label}</option>`,
  ).join("");
}
