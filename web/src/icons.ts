// Inline Lucide icons rendered with currentColor, so they follow the theme.
// The previous text glyphs (◐ ✕ ▶ ■ …) depended on the user's font and
// didn't render on some systems. Named imports keep the bundle small.

import type { IconNode as LucideIconNode } from "lucide";
import { Circle, CircleDot, Globe, Minus, Play, Plus, Power, RotateCw, Square, SunMoon, Trash2, Upload, X } from "lucide";

function render(node: LucideIconNode, size: number): string {
  const body = node
    .map(([tag, attrs]) => {
      const a = Object.entries(attrs)
        .filter(([, v]) => v !== undefined)
        .map(([k, v]) => `${k}="${String(v)}"`)
        .join(" ");
      return `<${tag} ${a}></${tag}>`;
    })
    .join("");
  return `<svg class="icon" width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`;
}

const svg = (node: LucideIconNode) => (size = 16) => render(node, size);

// Shorthands used throughout the UI.
export const ic = {
  theme: svg(SunMoon),
  close: svg(X),
  min: svg(Minus),
  max: svg(Square),
  start: svg(Play),
  stop: svg(Power),
  plus: svg(Plus),
  trash: svg(Trash2),
  upload: svg(Upload),
  reboot: svg(RotateCw),
  circle: svg(Circle),
  circleDot: svg(CircleDot),
  webui: svg(Globe),
};
