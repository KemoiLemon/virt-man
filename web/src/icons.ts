// Inline Phosphor "fill" icons rendered with currentColor, so they follow the
// theme. Each icon is imported on its own (`?raw`), so only the glyphs the UI
// uses reach the bundle — importing the whole set would add megabytes.

import ArrowDown from "@phosphor-icons/core/assets/fill/arrow-down-fill.svg?raw";
import ArrowLeft from "@phosphor-icons/core/assets/fill/arrow-left-fill.svg?raw";
import ArrowLeftRight from "@phosphor-icons/core/assets/fill/arrows-left-right-fill.svg?raw";
import ArrowUp from "@phosphor-icons/core/assets/fill/arrow-up-fill.svg?raw";
import Boxes from "@phosphor-icons/core/assets/fill/stack-fill.svg?raw";
import Camera from "@phosphor-icons/core/assets/fill/camera-fill.svg?raw";
import ChevronDown from "@phosphor-icons/core/assets/fill/caret-down-fill.svg?raw";
import Circle from "@phosphor-icons/core/assets/fill/circle-fill.svg?raw";
import CircleDot from "@phosphor-icons/core/assets/fill/record-fill.svg?raw";
import Clipboard from "@phosphor-icons/core/assets/fill/clipboard-fill.svg?raw";
import ClipboardPaste from "@phosphor-icons/core/assets/fill/clipboard-text-fill.svg?raw";
import Copy from "@phosphor-icons/core/assets/fill/copy-fill.svg?raw";
import Cpu from "@phosphor-icons/core/assets/fill/cpu-fill.svg?raw";
import Database from "@phosphor-icons/core/assets/fill/database-fill.svg?raw";
import Download from "@phosphor-icons/core/assets/fill/download-simple-fill.svg?raw";
import FileCode from "@phosphor-icons/core/assets/fill/file-code-fill.svg?raw";
import HardDrive from "@phosphor-icons/core/assets/fill/hard-drive-fill.svg?raw";
import KeyRound from "@phosphor-icons/core/assets/fill/key-fill.svg?raw";
import Keyboard from "@phosphor-icons/core/assets/fill/keyboard-fill.svg?raw";
import LayoutDashboard from "@phosphor-icons/core/assets/fill/squares-four-fill.svg?raw";
import Lock from "@phosphor-icons/core/assets/fill/lock-fill.svg?raw";
import LogOut from "@phosphor-icons/core/assets/fill/sign-out-fill.svg?raw";
import Maximize from "@phosphor-icons/core/assets/fill/arrows-out-fill.svg?raw";
import MemoryStick from "@phosphor-icons/core/assets/fill/memory-fill.svg?raw";
import Monitor from "@phosphor-icons/core/assets/fill/desktop-fill.svg?raw";
import Network from "@phosphor-icons/core/assets/fill/network-fill.svg?raw";
import OctagonX from "@phosphor-icons/core/assets/fill/prohibit-fill.svg?raw";
import Pencil from "@phosphor-icons/core/assets/fill/pencil-simple-fill.svg?raw";
import Play from "@phosphor-icons/core/assets/fill/play-fill.svg?raw";
import Plus from "@phosphor-icons/core/assets/fill/plus-fill.svg?raw";
import Power from "@phosphor-icons/core/assets/fill/power-fill.svg?raw";
import RotateCcw from "@phosphor-icons/core/assets/fill/arrow-counter-clockwise-fill.svg?raw";
import RotateCw from "@phosphor-icons/core/assets/fill/arrow-clockwise-fill.svg?raw";
import Server from "@phosphor-icons/core/assets/fill/hard-drives-fill.svg?raw";
import Settings from "@phosphor-icons/core/assets/fill/gear-fill.svg?raw";
import Shield from "@phosphor-icons/core/assets/fill/shield-fill.svg?raw";
import ShieldUser from "@phosphor-icons/core/assets/fill/user-gear-fill.svg?raw";
import SunMoon from "@phosphor-icons/core/assets/fill/circle-half-fill.svg?raw";
import Timer from "@phosphor-icons/core/assets/fill/timer-fill.svg?raw";
import Trash2 from "@phosphor-icons/core/assets/fill/trash-fill.svg?raw";
import Upload from "@phosphor-icons/core/assets/fill/upload-simple-fill.svg?raw";
import Users from "@phosphor-icons/core/assets/fill/users-fill.svg?raw";
import X from "@phosphor-icons/core/assets/fill/x-fill.svg?raw";
import ZoomIn from "@phosphor-icons/core/assets/fill/magnifying-glass-plus-fill.svg?raw";
import ZoomOut from "@phosphor-icons/core/assets/fill/magnifying-glass-minus-fill.svg?raw";

// Every Phosphor fill icon is one <path> on a 256 grid; keep just its data.
// Colour is set once in CSS (`.ico > svg`), never per icon.
const VIEWBOX = "0 0 256 256";
type Icon = string;
const d = (raw: string): Icon => /\sd="([^"]+)"/.exec(raw)?.[1] ?? "";

function render(raw: Icon, size: number): string {
  return `<span class="ico" style="width:${size}px;height:${size}px" aria-hidden="true">` +
    `<svg viewBox="${VIEWBOX}"><path d="${d(raw)}"></path></svg></span>`;
}

const svg = (raw: Icon) => (size = 16) => render(raw, size);

/** Build a real <svg> element (not markup) for DOM-constructed controls.
 *  Callers place it inside an `.ico` wrapper, which supplies the colour. */
function element(raw: Icon, size: number): SVGSVGElement {
  const NS = "http://www.w3.org/2000/svg";
  const root = document.createElementNS(NS, "svg");
  root.setAttribute("width", String(size));
  root.setAttribute("height", String(size));
  root.setAttribute("viewBox", VIEWBOX);
  root.setAttribute("fill", "currentColor");
  root.setAttribute("aria-hidden", "true");
  const path = document.createElementNS(NS, "path");
  path.setAttribute("d", d(raw));
  root.append(path);
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
  start: svg(Play),
  stop: svg(Power),
  autostart: svg(Timer),
  plus: svg(Plus),
  trash: svg(Trash2),
  upload: svg(Upload),
  reboot: svg(RotateCw),
  reset: svg(RotateCcw),
  circle: svg(Circle),
  circleDot: svg(CircleDot),
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
  logout: svg(LogOut),
  admin: svg(ShieldUser),
  forceStop: svg(OctagonX),
  xml: svg(FileCode),
  options: svg(Settings),
  firewall: svg(Shield),
  down: svg(ArrowDown),
  up: svg(ArrowUp),
  bidir: svg(ArrowLeftRight),
  back: svg(ArrowLeft),
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
