// The touch gesture vocabulary: the thresholds and geometry that `menus.ts`
// (long press -> context menu) and `console.ts` (tap / drag / pinch) both read.
//
// They live in one module precisely so the two cannot disagree; if they do, a
// gesture opens a menu in one place and drags a window in the other. The
// numbers are tested at their boundaries because every one of them is a
// judgement call: too small and a scroll becomes a click, too large and a click
// becomes a scroll.
//
// Run: pnpm test

import assert from "node:assert/strict";
import test, { before } from "node:test";
import { JSDOM } from "jsdom";

let t;

before(async () => {
  const dom = new JSDOM("<!doctype html><body></body>", {
    url: "http://localhost/",
    pretendToBeVisual: true,
  });
  globalThis.window = dom.window;
  globalThis.document = dom.window.document;
  globalThis.getComputedStyle = dom.window.getComputedStyle.bind(dom.window);
  t = await import("./_build/touch.js");
});

// ---- long press ------------------------------------------------------------

test("a resting finger is a long press", () => {
  assert.equal(t.isLongPress(t.LONG_PRESS_MS, 0), true);
  assert.equal(t.isLongPress(t.LONG_PRESS_MS + 500, 0), true);
});

test("a long press needs both the time and the stillness", () => {
  // Not long enough yet...
  assert.equal(t.isLongPress(t.LONG_PRESS_MS - 1, 0), false);
  // ...or it moved, which makes it a drag.
  assert.equal(t.isLongPress(t.LONG_PRESS_MS, t.MOVE_SLOP_PX + 1), false);
  assert.equal(t.isLongPress(t.LONG_PRESS_MS, t.MOVE_SLOP_PX), true,
    "the slop itself is still a press: a finger is never perfectly still");
});

test("the slop is small enough to be crossed by a deliberate drag", () => {
  // A drag of a window is tens of pixels; if the slop were anywhere near that,
  // dragging would start with a click in the guest.
  assert.ok(t.MOVE_SLOP_PX <= 16, `slop is ${t.MOVE_SLOP_PX}px`);
});

// ---- two-finger tap -------------------------------------------------------

test("a quick, still two-finger press is a tap", () => {
  assert.equal(t.isTwoFingerTap(t.TWO_FINGER_TAP_MS, 0), true);
});

test("a two-finger scroll is not a tap", () => {
  assert.equal(t.isTwoFingerTap(t.TWO_FINGER_TAP_MS + 1, 0), false, "too slow");
  assert.equal(t.isTwoFingerTap(t.TWO_FINGER_TAP_MS, t.MOVE_SLOP_PX + 1), false,
    "the fingers travelled, so it was a scroll");
});

// ---- geometry --------------------------------------------------------------

test("distance and midpoint are the plain Euclidean ones", () => {
  assert.equal(t.distance({ x: 0, y: 0 }, { x: 3, y: 4 }), 5);
  assert.deepEqual(t.midpoint({ x: 0, y: 0 }, { x: 10, y: 4 }), { x: 5, y: 2 });
});

// ---- pinch -----------------------------------------------------------------

test("pinching apart zooms in and together zooms out", () => {
  assert.equal(t.pinchFactor(100, 200), 2);
  assert.equal(t.pinchFactor(200, 100), 0.5);
});

test("a degenerate pinch means no change, not infinity", () => {
  // Both fingers reporting the same point happens on the first move, and
  // Infinity there would blow the zoom up irrecoverably.
  for (const [from, to] of [[0, 100], [0, 0], [-1, 50], [Number.NaN, 50], [100, Number.NaN]]) {
    const f = t.pinchFactor(from, to);
    assert.ok(Number.isFinite(f), `finite for ${from} -> ${to}: ${f}`);
  }
  assert.equal(t.pinchFactor(0, 100), 1);
});

test("pinch zoom clamps to the same range as the toolbar", () => {
  // The zoom buttons clamp to 0.25-4; a pinch that could exceed that would
  // produce a scale the buttons cannot get back from.
  assert.equal(t.clampZoom(1, 100), 4, "clamps at 400%");
  assert.equal(t.clampZoom(1, 0.001), 0.25, "clamps at 25%");
  assert.equal(t.clampZoom(2, 1), 2, "no change is no change");
});

test("a pinch starting from Fit scales from 1:1, not from zero", () => {
  // Zoom 0 means fit-to-stage. Treating it as a literal zero would collapse the
  // guest to nothing on the first pinch.
  assert.equal(t.clampZoom(0, 2), 2);
  assert.equal(t.clampZoom(0, 0.25), 0.25);
});

// ---- popover height --------------------------------------------------------

test("popover height follows the visual viewport, not innerHeight", () => {
  // innerHeight does not shrink while the iOS keyboard is up, so a select
  // positioned against it lands behind the keyboard.
  const { window } = globalThis;
  const real = window.innerHeight;
  window.visualViewport = { height: 320 };
  assert.equal(t.visualHeight(), 320 - t.safeInset("bottom"));

  // Without a visualViewport (any browser that does not have one) it must fall
  // back rather than throw or return NaN.
  delete window.visualViewport;
  assert.equal(t.visualHeight(), Math.max(120, real - t.safeInset("bottom")));
});

test("an unusable viewport still leaves a popover somewhere", () => {
  // A keyboard taller than the screen, or a layout that has not happened yet:
  // the floor keeps the popover on screen instead of off the top.
  globalThis.window.visualViewport = { height: 0 };
  assert.ok(t.visualHeight() >= 120, `floor held: ${t.visualHeight()}`);
});

test("safe insets are numbers even when nothing declares them", () => {
  for (const side of ["top", "right", "bottom", "left"]) {
    const n = t.safeInset(side);
    assert.ok(Number.isFinite(n), `${side} is a finite number, got ${n}`);
  }
});

// ---- the long-press attachment --------------------------------------------

/** A pointerdown carrying a type, since jsdom has no PointerEvent. */
function pointerdown(win, target, { pointerType = "touch", x = 5, y = 5 } = {}) {
  const e = new win.Event("pointerdown", { bubbles: true });
  Object.assign(e, { pointerType, clientX: x, clientY: y });
  target.dispatchEvent(e);
  return e;
}

test("a press that stays put fires once, and swallows the trailing click", () => {
  const { window, document } = globalThis;
  const el = document.createElement("button");
  document.body.append(el);

  let fired = 0;
  const detach = t.attachLongPress(document, () => { fired++; return true; });

  let reachedDocument = 0;
  document.addEventListener("click", () => { reachedDocument++; });
  pointerdown(window, el);

  return new Promise((resolve) => {
    window.setTimeout(() => {
      assert.equal(fired, 1, "the long press fired exactly once");
      // The click the platform emits when the finger lifts. ctxmenu closes on
      // any click outside the menu, so without swallowing, the menu would
      // vanish in the frame it appeared. Dispatched AFTER the press, because
      // that is when a real one arrives -- arming the swallow is part of what
      // the long press does.
      document.body.dispatchEvent(new window.Event("click", { bubbles: true }));
      assert.equal(reachedDocument, 0, "the trailing click never reached the page");
      detach();
      resolve();
    }, t.LONG_PRESS_MS + 60);
  });
});

test("an unhandled press keeps the native behaviour", () => {
  const { window, document } = globalThis;
  const el = document.createElement("p");
  document.body.append(el);

  let fired = 0;
  const detach = t.attachLongPress(document, () => { fired++; return false; });

  pointerdown(window, el);
  return new Promise((resolve) => {
    window.setTimeout(() => {
      assert.equal(fired, 1, "the handler was asked");
      // Returning false must NOT swallow the click, or a long press on plain
      // text would kill the copy callout that iOS shows.
      let reached = 0;
      document.addEventListener("click", () => { reached++; });
      document.body.dispatchEvent(new window.Event("click", { bubbles: true }));
      assert.equal(reached, 1, "the click still got through");
      detach();
      resolve();
    }, t.LONG_PRESS_MS + 60);
  });
});

test("a moving finger is never a long press", () => {
  const { window, document } = globalThis;
  const el = document.createElement("div");
  document.body.append(el);

  let fired = 0;
  const detach = t.attachLongPress(document, () => { fired++; return true; });

  pointerdown(window, el);
  const move = new window.Event("pointermove", { bubbles: true });
  Object.assign(move, { pointerType: "touch", clientX: 5, clientY: 5 + t.MOVE_SLOP_PX * 4 });
  document.body.dispatchEvent(move);

  return new Promise((resolve) => {
    window.setTimeout(() => {
      assert.equal(fired, 0, "scrolling a list must not open a menu");
      detach();
      resolve();
    }, t.LONG_PRESS_MS + 60);
  });
});

test("a mouse is left to its own right button", () => {
  const { window, document } = globalThis;
  const el = document.createElement("div");
  document.body.append(el);

  let fired = 0;
  const detach = t.attachLongPress(document, () => { fired++; return true; });
  pointerdown(window, el, { pointerType: "mouse" });

  return new Promise((resolve) => {
    window.setTimeout(() => {
      assert.equal(fired, 0, "mouse users right-click, they do not wait");
      detach();
      resolve();
    }, t.LONG_PRESS_MS + 60);
  });
});
