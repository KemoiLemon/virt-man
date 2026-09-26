// Inline Lucide icons rendered with currentColor, so they follow the theme.
// The previous text glyphs (◐ ✕ ▶ ■ …) depended on the user's font and
// didn't render on some systems. Named imports keep the bundle small.

import type { IconNode as LucideIconNode } from "lucide";
import {
  Boxes,
  Camera,
  ChevronDown,
  Circle,
  CircleDot,
  Clipboard,
  ClipboardPaste,
  Copy,
  Cpu,
  Database,
  Download,
  FileCode,
  Globe,
  HardDrive,
  KeyRound,
  LayoutDashboard,
  Lock,
  MemoryStick,
  Minus,
  Monitor,
  Keyboard,
  Maximize,
  Network,
  OctagonX,
  Pencil,
  Settings,
  Shield,
  Play,
  Plus,
  Power,
  RotateCw,
  Server,
  Square,
  SunMoon,
  Trash2,
  Upload,
  Users,
  X,
  ZoomIn,
  ZoomOut,
} from "lucide";

// The Lucide pack ships path data only. Stroke weight, caps and joins are
// defined once in CSS (`.ico > svg`), so no icon repeats them inline and the
// whole set restyles from one rule — the same arrangement vm-man uses.
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
  return `<span class="ico" style="width:${size}px;height:${size}px" aria-hidden="true">` +
    `<svg viewBox="0 0 24 24">${body}</svg></span>`;
}

const svg = (node: LucideIconNode) => (size = 16) => render(node, size);

/** Build a real <svg> element (not markup) for DOM-constructed controls.
 *  Callers place it inside an `.ico` wrapper, which supplies the stroke. */
function element(node: LucideIconNode, size: number): SVGSVGElement {
  const NS = "http://www.w3.org/2000/svg";
  const root = document.createElementNS(NS, "svg");
  root.setAttribute("width", String(size));
  root.setAttribute("height", String(size));
  root.setAttribute("viewBox", "0 0 24 24");
  root.setAttribute("fill", "none");
  root.setAttribute("stroke", "currentColor");
  root.setAttribute("stroke-width", "2");
  root.setAttribute("stroke-linecap", "round");
  root.setAttribute("stroke-linejoin", "round");
  root.setAttribute("aria-hidden", "true");
  for (const [tag, attrs] of node) {
    const child = document.createElementNS(NS, tag);
    for (const [k, v] of Object.entries(attrs)) {
      if (v !== undefined) child.setAttribute(k, String(v));
    }
    root.append(child);
  }
  return root;
}

/** Caret used by the themed listbox trigger. */
export function chevronSvg(size = 14): SVGSVGElement {
  return element(ChevronDown, size);
}

/** Close glyph for DOM-built modals that can't use the markup shorthand. */
export function closeSvg(size = 14): SVGSVGElement {
  return element(X, size);
}

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
  chevron: svg(ChevronDown),
  // Resource tree + dashboard
  datacenter: svg(LayoutDashboard),
  server: svg(Server),
  vm: svg(Monitor),
  vms: svg(Boxes),
  network: svg(Network),
  storage: svg(Database),
  disk: svg(HardDrive),
  cpu: svg(Cpu),
  memory: svg(MemoryStick),
  users: svg(Users),
  edit: svg(Pencil),
  key: svg(KeyRound),
  lock: svg(Lock),
  camera: svg(Camera),
  forceStop: svg(OctagonX),
  xml: svg(FileCode),
  options: svg(Settings),
  firewall: svg(Shield),
  // Console toolbar
  zoomIn: svg(ZoomIn),
  zoomOut: svg(ZoomOut),
  fit: svg(Maximize),
  clipboard: svg(ClipboardPaste),
  copy: svg(Copy),
  type: svg(Keyboard),
  clip: svg(Clipboard),
  download: svg(Download),
};
