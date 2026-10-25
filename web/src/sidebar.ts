// The resizable, collapsible sidebar: its width glide, the saved width, and the
// hover tip that names a row while only icons show. Its state is private to
// this module. Importing it also installs the saved width and the tip handlers.

/// Desktop only (the handle is hidden on a phone and the CSS is gated on
/// width). Dragging the sidebar's edge resizes it; dragged narrow enough it
/// snaps to icons only, and dragging back out restores it. Double-click toggles.
const SIDE_MIN = 220, SIDE_MAX = 600, SIDE_SNAP = 150, SIDE_ICONS = 52;
let sideW: number | null = null; // expanded width; null until chosen (CSS default)
let sideCur = 0; // width on screen right now
let sideTarget = 0;
let sideRaf = 0;
let sideLast = 0;
export const reduced = () => matchMedia("(prefers-reduced-motion: reduce)").matches;
const sideNow = () => document.querySelector<HTMLElement>(".list-pane")?.getBoundingClientRect().width ?? 0;
function paintSide() {
  document.body.classList.add("side-sized");
  document.body.style.setProperty("--side-w", `${sideCur}px`);
}
/// The width eases toward its target every frame, so a drag trails the mouse
/// a little and a snap to or from icons glides. A CSS transition on the grid
/// cannot do this: it would fight the drag, and the default width was not an
/// interpolable value.
function glideSide() {
  if (sideRaf) return;
  sideLast = performance.now();
  const tick = (now: number) => {
    // A frame's timestamp is its start, so it can precede the performance.now()
    // taken when the loop began; a negative step ran the ease backwards.
    const dt = Math.max(0, now - sideLast);
    sideLast = now;
    sideCur += (sideTarget - sideCur) * (reduced() ? 1 : 1 - Math.exp(-dt / 45));
    if (Math.abs(sideTarget - sideCur) < 0.5) sideCur = sideTarget;
    paintSide();
    sideRaf = sideCur === sideTarget ? 0 : requestAnimationFrame(tick);
  };
  sideRaf = requestAnimationFrame(tick);
}
function setSide(w: number | null, collapsed: boolean, instant = false) {
  if (w !== null) sideW = w;
  document.body.classList.toggle("side-collapsed", collapsed);
  hideSideTip();
  if (!sideCur) sideCur = sideNow() || SIDE_MAX / 2;
  sideTarget = collapsed ? SIDE_ICONS : sideW ?? Math.max(300, innerWidth * 0.21);
  if (instant) { sideCur = sideTarget; paintSide(); } else glideSide();
}
// Not JSON in the button-based version ("collapsed"/"open"), which is still in
// some browsers' storage; ignore anything that does not parse.
let savedSide: { w: number; collapsed: boolean } | null = null;
try { savedSide = JSON.parse(localStorage.getItem("virt-man-side") ?? "null"); } catch { /* old value */ }
if (savedSide) setSide(savedSide.w, savedSide.collapsed, true);
export function bindSideResize(h: HTMLElement | null) {
  if (!h) return;
  const save = () => localStorage.setItem("virt-man-side", JSON.stringify({ w: sideW ?? sideNow(), collapsed: document.body.classList.contains("side-collapsed") }));
  h.ondblclick = () => { setSide(sideW ?? sideNow(), !document.body.classList.contains("side-collapsed")); save(); };
  h.onpointerdown = (e) => {
    h.setPointerCapture(e.pointerId);
    document.body.classList.add("side-dragging");
    if (!sideCur) sideCur = sideNow();
    h.onpointermove = (m) => setSide(Math.min(SIDE_MAX, Math.max(SIDE_MIN, m.clientX)), m.clientX < SIDE_SNAP);
    h.onpointerup = () => {
      h.onpointermove = h.onpointerup = null;
      document.body.classList.remove("side-dragging");
      save();
    };
  };
}

let sideTip: HTMLElement | null = null;
let sideTipRow: Element | null = null;
function hideSideTip() { sideTip?.remove(); sideTip = sideTipRow = null; }
// One tip for every row: the labels are visually hidden while collapsed, so the
// row's own text (plus its state) is what gets shown beside it.
document.addEventListener("mouseover", (e) => {
  const row = document.body.classList.contains("side-collapsed")
    ? (e.target as Element).closest?.(".list-pane .tree-row, .list-pane .side-profile")
    : null;
  if (!row) return hideSideTip();
  // mouseover also fires moving between a row's children; rebuilding then made
  // the label flicker.
  if (row === sideTipRow) return;
  const text = [".tree-label", ".who b"].map((q) => row.querySelector(q)?.textContent).find(Boolean);
  if (!text) return hideSideTip();
  const state = row.querySelector(".tree-state")?.textContent?.trim();
  hideSideTip();
  const tip = (sideTip = document.createElement("div"));
  tip.className = "side-tip";
  sideTipRow = row;
  tip.textContent = state ? `${text} · ${state}` : text;
  document.body.appendChild(tip);
  const r = row.getBoundingClientRect();
  tip.style.left = `${r.right + 6}px`;
  tip.style.top = `${r.top + r.height / 2 - tip.offsetHeight / 2}px`;
});
document.addEventListener("mouseleave", hideSideTip);
document.addEventListener("scroll", hideSideTip, true);
