// Touch gestures: the phone's input vocabulary, in one place.
//
// Three call sites have to agree on it. `menus.ts` opens the context menu from a
// long press (the same menu a right-click opens, because iOS WebKit never fires
// `contextmenu` at all), `console.ts` maps the same gestures onto guest input,
// and both `listbox.ts` and `ctxmenu.ts` need a height that accounts for a
// raised software keyboard. The geometry and the thresholds are pure so they
// can be tested without a browser; only `attachLongPress` and `visualHeight`
// touch the DOM.

export interface Pt { x: number; y: number }

/// Whether this device points with a finger. A tablet is coarse at any width,
/// so this is a device question rather than a layout one, and it is the test
/// for behaviour that must not happen on a desktop (raising the iOS software
/// keyboard, deferring a press to find out what the gesture is).
export function isCoarsePointer(): boolean {
  return window.matchMedia?.("(pointer: coarse)").matches ?? false;
}

/// A finger that rests this long without travelling is a long press: the
/// phone's right-click.
export const LONG_PRESS_MS = 500;
/// Travel that still counts as "not moved". A finger is never perfectly still,
/// and a scroll always exceeds this, which is what cancels the timer.
export const MOVE_SLOP_PX = 8;
/// Two fingers down and up quickly is a right-click too — the gesture people
/// already expect from trackpads.
export const TWO_FINGER_TAP_MS = 300;

export function distance(a: Pt, b: Pt): number {
  return Math.hypot(a.x - b.x, a.y - b.y);
}

export function midpoint(a: Pt, b: Pt): Pt {
  return { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
}

/// Whether a press that lasted `ms` and travelled `movedPx` was a long press.
export function isLongPress(ms: number, movedPx: number): boolean {
  return ms >= LONG_PRESS_MS && movedPx <= MOVE_SLOP_PX;
}

/// Whether a two-finger press that lasted `ms` and travelled `movedPx` was a
/// tap (right-click) rather than a scroll.
export function isTwoFingerTap(ms: number, movedPx: number): boolean {
  return ms <= TWO_FINGER_TAP_MS && movedPx <= MOVE_SLOP_PX;
}

/// Ratio to scale by for a pinch that started `startDist` apart. A zero
/// distance is a degenerate gesture (both fingers reported the same point),
/// and dividing by it would produce Infinity, so it means "no change".
export function pinchFactor(startDist: number, dist: number): number {
  if (!Number.isFinite(startDist) || startDist <= 0 || !Number.isFinite(dist)) return 1;
  return dist / startDist;
}

/// Zoom from a pinch, clamped to the same 0.25–4 the toolbar's zoom buttons
/// use. `base` is 0 for fit-to-stage, which zooms from 1:1 so a first pinch is
/// predictable rather than jumping from the fitted scale.
export function clampZoom(base: number, factor: number): number {
  const from = base > 0 ? base : 1;
  return Math.min(4, Math.max(0.25, +((from * factor).toFixed(2))));
}

/// One measured safe-area inset, in px. The values are declared as custom
/// properties in base.css so CSS and JS read the same number; without
/// `viewport-fit=cover` they are all 0.
///
/// Through `document.defaultView` rather than the bare `getComputedStyle`
/// global, so this works wherever it is imported (a test harness does not
/// always install the global) and cannot pick up another window's styles.
export function safeInset(side: "top" | "right" | "bottom" | "left"): number {
  const view = document.defaultView;
  if (!view) return 0;
  const v = view
    .getComputedStyle(document.documentElement)
    .getPropertyValue(`--safe-${side}`);
  const n = Number.parseFloat(v);
  return Number.isFinite(n) ? n : 0;
}

/// Height a popover may occupy.
///
/// `window.innerHeight` does not shrink while the iOS software keyboard is up,
/// so a select or menu opened near the bottom of a small screen used to be
/// clamped to a box that put it *behind* the keyboard. `visualViewport` is the
/// box that is actually visible; the safe-area floor is subtracted because the
/// home bar overlays the page rather than shortening it.
export function visualHeight(): number {
  const view = document.defaultView;
  const h = view?.visualViewport?.height ?? view?.innerHeight ?? window.innerHeight;
  return Math.max(120, h - safeInset("bottom"));
}

/// Swallow exactly one click, capture-phase so it never reaches the document
/// listeners that dismiss open layers.
///
/// This is not optional. `ctxmenu.ts` closes on any click outside the menu, and
/// a long press is a pointerdown: the finger lifting afterwards emits a click
/// that would dismiss the menu in the same frame it appeared.
///
/// The listener is also dropped after CLICK_SWALLOW_MS, because a platform that
/// does not synthesise a click after a long press (iOS often does not) would
/// otherwise leave it armed to eat the user's *next* tap — which is the tap on
/// a menu item they just opened.
const CLICK_SWALLOW_MS = 700;

function swallowNextClick() {
  let timer = 0;
  let done = false;
  const off = () => {
    if (done) return;
    done = true;
    window.clearTimeout(timer);
    document.removeEventListener("click", sink, true);
  };
  const sink = (e: Event) => {
    e.stopPropagation();
    off();
  };
  document.addEventListener("click", sink, true);
  timer = window.setTimeout(off, CLICK_SWALLOW_MS);
}

/**
 * Call `onLongPress` when a finger rests inside `root` for LONG_PRESS_MS
 * without travelling more than MOVE_SLOP_PX.
 *
 * `onLongPress` returns whether it used the press. Only a used press has its
 * trailing click swallowed, so an unhandled long press keeps behaving natively
 * — which is what preserves the copy callout in the XML and log panes.
 *
 * Returns a detach function.
 */
export function attachLongPress(
  root: EventTarget,
  onLongPress: (x: number, y: number, target: Element | null) => boolean,
): () => void {
  let timer = 0;
  let start: Pt | null = null;
  let target: Element | null = null;

  const cancel = () => {
    if (timer) {
      window.clearTimeout(timer);
      timer = 0;
    }
    start = null;
    target = null;
  };

  const down = (e: Event) => {
    const pe = e as PointerEvent;
    // Mouse and pen already have a right button; only touch needs this.
    if (pe.pointerType && pe.pointerType !== "touch") return;
    cancel();
    start = { x: pe.clientX, y: pe.clientY };
    target = (pe.target as Element | null) ?? null;
    timer = window.setTimeout(() => {
      timer = 0;
      const s = start;
      start = null;
      if (!s) return;
      if (!onLongPress(s.x, s.y, target)) return;
      swallowNextClick();
    }, LONG_PRESS_MS);
  };

  const move = (e: Event) => {
    if (!start) return;
    const pe = e as PointerEvent;
    if (distance(start, { x: pe.clientX, y: pe.clientY }) > MOVE_SLOP_PX) cancel();
  };

  root.addEventListener("pointerdown", down);
  root.addEventListener("pointermove", move, { passive: true });
  root.addEventListener("pointerup", cancel);
  root.addEventListener("pointercancel", cancel);
  // Scrolling the list under the finger must abandon the press too: the
  // pointermove above usually catches it, but a fast flick can scroll without
  // ever reporting a move past the slop.
  root.addEventListener("scroll", cancel, true);

  return () => {
    cancel();
    root.removeEventListener("pointerdown", down);
    root.removeEventListener("pointermove", move);
    root.removeEventListener("pointerup", cancel);
    root.removeEventListener("pointercancel", cancel);
    root.removeEventListener("scroll", cancel, true);
  };
}
