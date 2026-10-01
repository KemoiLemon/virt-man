// VNC console: a canvas fed raw pixels by the daemon.
//
// Ported from vm-man's viewer. The RFB client itself lives in the daemon
// (daemon/src/rfb.rs): it holds the guest framebuffer and hands out only the
// rectangle that changed, as base64 RGBA. So there is no VNC client in the
// browser at all — no noVNC, no WebSocket, just putImageData.
//
// RGBA, not BGRA: the daemon converts the guest's BGRX wire bytes once, so the
// payload is already in the order ImageData wants and must be passed straight
// through. Converting a second time here swaps red and blue — blue rendered as
// red, which is what a viewer doing its own "BGRA to RGBA" will always do.
//
// Scaling contract (also from vm-man): the displayed size is computed from the
// LOCAL stage box and devicePixelRatio, using the guest resolution purely as an
// aspect ratio. The guest is never asked to resize, and guest pixels never set
// layout size.
//
// SPICE is not RFB and has no usable browser client, so those guests are handed
// a virt-viewer file instead of being rendered here.

import { api } from "./api";
import { speed } from "./console-speed";
import {
  LONG_PRESS_MS, MOVE_SLOP_PX, clampZoom, distance, isCoarsePointer,
  isTwoFingerTap, midpoint, pinchFactor, type Pt,
} from "./touch";

export interface ConsoleHandle {
  kind: string;
  /** Local zoom: 0 means fit-to-stage, otherwise a scale factor. */
  getZoom(): number;
  setZoom(z: number): void;
  refit(): void;
  sendCtrlAltDel(): Promise<void>;
  /** Type a string into the guest as synthetic key events. */
  typeText(text: string): Promise<void>;
  /** Stop streaming and drop the daemon-side session. */
  stop(): void;
}

/**
 * One pointer report, in viewport (client) coordinates. Converted to guest
 * pixels only at the moment it is sent, so the touch state machine below is
 * pure arithmetic on plain numbers.
 *
 * `buttons` is the RFB mask: 1 left, 2 middle, 4 right, 8/16 wheel.
 */
export interface TouchPacket { x: number; y: number; buttons: number }

/**
 * A one-finger touch gesture.
 *
 * Deferred press: `touchDown` sends NOTHING. A press is only sent once the
 * gesture is known — a tap, a drag, or (after LONG_PRESS_MS) a right-click.
 * Pressing at `pointerdown` instead would put a button-1 down in the guest
 * before we know which it is, and every wrong guess is a stray click in
 * someone's desktop.
 */
export interface TouchPhase {
  phase: "idle" | "held" | "dragging" | "done";
  /** Where the finger landed. A drag's press is reported from here, not from
   *  wherever the finger had already travelled to when the slop was crossed. */
  start: Pt | null;
  /** The last point seen, so a long press knows where to right-click. */
  last: Pt | null;
}

export function idleTouch(): TouchPhase {
  return { phase: "idle", start: null, last: null };
}

/** Finger down. Deliberately sends nothing — see `TouchPhase`. */
export function touchDown(p: Pt): TouchPhase {
  return { phase: "held", start: p, last: p };
}

export function touchMove(
  s: TouchPhase,
  p: Pt,
): { state: TouchPhase; sends: TouchPacket[] } {
  if (s.phase === "held" && s.start) {
    const from = s.start;
    if (distance(from, p) <= MOVE_SLOP_PX) return { state: { ...s, last: p }, sends: [] };
    return {
      state: { ...s, phase: "dragging", last: p },
      sends: [
        { x: from.x, y: from.y, buttons: 1 },
        { x: p.x, y: p.y, buttons: 1 },
      ],
    };
  }
  if (s.phase === "dragging") {
    return { state: { ...s, last: p }, sends: [{ x: p.x, y: p.y, buttons: 1 }] };
  }
  return { state: s, sends: [] };
}

export function touchUp(
  s: TouchPhase,
  p: Pt,
): { state: TouchPhase; sends: TouchPacket[]; tap: boolean } {
  if (s.phase === "held") {
    // Never left the slop: a click, pressed and released at the same point.
    return {
      state: idleTouch(),
      sends: [
        { x: p.x, y: p.y, buttons: 1 },
        { x: p.x, y: p.y, buttons: 0 },
      ],
      tap: true,
    };
  }
  if (s.phase === "dragging") {
    return { state: idleTouch(), sends: [{ x: p.x, y: p.y, buttons: 0 }], tap: false };
  }
  // "done": this gesture was already resolved as a long press.
  return { state: idleTouch(), sends: [], tap: false };
}

/** The long-press timer fired: this gesture is a right-click, and it is over. */
export function touchLongPress(s: TouchPhase): { state: TouchPhase; sends: TouchPacket[] } {
  const p = s.last ?? s.start;
  if (s.phase !== "held" || !p) return { state: s, sends: [] };
  return {
    state: { ...s, phase: "done" },
    sends: [
      { x: p.x, y: p.y, buttons: 4 },
      { x: p.x, y: p.y, buttons: 0 },
    ],
  };
}

/** One frame update as the daemon serialises it. */
interface FrameUpdate {
  seq: number;
  width: number;
  height: number;
  x: number;
  y: number;
  w: number;
  h: number;
  pixels: string;
}

let current: ConsoleHandle | null = null;

export function currentConsole(): ConsoleHandle | null {
  return current;
}

export function detachConsole() {
  current?.stop();
  current = null;
}

/// X11 keysyms for the keys that are not plain ASCII.
const KEYSYM: Record<string, number> = {
  Backspace: 0xff08, Tab: 0xff09, Enter: 0xff0d, Escape: 0xff1b, Delete: 0xffff,
  Home: 0xff50, ArrowLeft: 0xff51, ArrowUp: 0xff52, ArrowRight: 0xff53,
  ArrowDown: 0xff54, PageUp: 0xff55, PageDown: 0xff56, End: 0xff57, Insert: 0xff63,
  F1: 0xffbe, F2: 0xffbf, F3: 0xffc0, F4: 0xffc1, F5: 0xffc2, F6: 0xffc3,
  F7: 0xffc4, F8: 0xffc5, F9: 0xffc6, F10: 0xffc7, F11: 0xffc8, F12: 0xffc9,
  Shift: 0xffe1, Control: 0xffe3, Alt: 0xffe9, Meta: 0xffeb, CapsLock: 0xffe5,
  " ": 0x20,
};

export function keysymFor(key: string): number {
  if (KEYSYM[key]) return KEYSYM[key];
  return key.length === 1 ? key.charCodeAt(0) : 0;
}

/// Pure: largest scale that fits the guest in the stage, never upscaling past
/// 1:1 so a small guest on a big screen stays crisp instead of being blown up.
export function fitScale(guestW: number, guestH: number, stageW: number, stageH: number): number {
  if (!guestW || !guestH || stageW <= 0 || stageH <= 0) return 1;
  return Math.min(stageW / guestW, stageH / guestH, 1);
}

/// Decode one frame rectangle into RGBA for putImageData.
///
/// The bytes are *already* RGBA. The daemon asks the guest for 32bpp BGRX and
/// converts it once, in `raw_to_rgba` (`daemon/src/rfb.rs`), then copies that
/// framebuffer out verbatim — so there is nothing left to convert here, and
/// swapping the channels again renders blue as red. That was the bug: two
/// correct-looking conversions in a row, one on each side of the wire. The name
/// records what the bytes are, precisely so nobody "fixes" the channels back.
///
// The ArrayBuffer parameter is load-bearing: TypeScript otherwise infers
// ArrayBufferLike, which ImageData refuses because it could be shared.
export function frameToRgba(
  bin: string,
  w: number,
  h: number,
): Uint8ClampedArray<ArrayBuffer> | null {
  const need = w * h * 4;
  if (bin.length < need) return null;
  const px = new Uint8ClampedArray(new ArrayBuffer(need));
  for (let i = 0; i < need; i += 4) {
    px[i] = bin.charCodeAt(i);
    px[i + 1] = bin.charCodeAt(i + 1);
    px[i + 2] = bin.charCodeAt(i + 2);
    // The daemon forces alpha opaque; forced again here because the guest's
    // 4th byte is BGRX padding, and a transparent screen is worse than a
    // wasted store.
    px[i + 3] = 255;
  }
  return px;
}

/**
 * Attach the viewer to `stage` for one VM.
 *
 * `base` is the console endpoint prefix, e.g.
 * `/proxy/{conn}/domains/{name}/console`. Returns null when the guest is not
 * VNC or the daemon refused to attach; `onStatus` carries the reason.
 */
export async function attachConsole(
  stage: HTMLElement,
  kind: string,
  base: string,
  onStatus: (msg: string | null) => void,
  /** Called whenever the local zoom changes, including by a pinch, so the
   *  caller's zoom label does not go stale. */
  onZoom?: (zoom: number) => void,
): Promise<ConsoleHandle | null> {
  detachConsole();

  if (kind !== "vnc") {
    onStatus(`${kind.toUpperCase()} consoles open in virt-viewer.`);
    return null;
  }

  let info: { width: number; height: number };
  try {
    info = await api.post<{ width: number; height: number }>(`${base}/connect`);
  } catch (e) {
    onStatus((e as Error).message);
    return null;
  }

  const canvas = document.createElement("canvas");
  canvas.className = "vnc-screen";
  canvas.setAttribute("aria-label", "guest console");
  stage.textContent = "";
  stage.appendChild(canvas);
  if (!stage.hasAttribute("tabindex")) stage.tabIndex = 0;

  let ctx: CanvasRenderingContext2D | null = null;
  const state = {
    w: info.width, h: info.height,
    seq: -1, zoom: 0, buttons: 0,
    live: true,
  };

  const applyScale = () => {
    if (!state.w || !state.h) return;
    if (canvas.width !== state.w || canvas.height !== state.h) {
      canvas.width = state.w;
      canvas.height = state.h;
    }
    // clientWidth/Height exclude any scrollbar gutter; a bounding rect does
    // not, and fitting to that leaves the image off-centre when it nearly
    // fills the stage.
    const scale = state.zoom === 0
      ? fitScale(state.w, state.h, stage.clientWidth, stage.clientHeight)
      : state.zoom;
    // Round to whole device pixels so scaling never resamples.
    const dpr = window.devicePixelRatio || 1;
    const w = Math.max(1, Math.round(state.w * scale * dpr) / dpr);
    const h = Math.max(1, Math.round(state.h * scale * dpr) / dpr);
    canvas.style.width = `${w}px`;
    canvas.style.height = `${h}px`;
    // Crisp when magnifying, smooth when minifying.
    canvas.style.imageRendering = scale >= 1 ? "pixelated" : "auto";
  };

  /// Map a viewport point to guest pixels (the inverse of the scale).
  /// Clamped, so a touch that drags past the edge of the canvas keeps reporting
  /// the nearest edge pixel instead of running off into negative coordinates —
  /// which is what a guest drag expects when the pointer leaves the window.
  const toGuestXY = (cx: number, cy: number) => {
    const r = canvas.getBoundingClientRect();
    const x = ((cx - r.left) / (r.width || 1)) * state.w;
    const y = ((cy - r.top) / (r.height || 1)) * state.h;
    return {
      x: Math.max(0, Math.min(state.w - 1, Math.round(x))),
      y: Math.max(0, Math.min(state.h - 1, Math.round(y))),
    };
  };
  const toGuest = (ev: { clientX: number; clientY: number }) => toGuestXY(ev.clientX, ev.clientY);

  const sendPointer = (x: number, y: number, buttons: number) =>
    api.post(`${base}/pointer`, { x, y, buttons }).catch(() => {});
  const postKey = (keysym: number, down: boolean) =>
    api.post(`${base}/key`, { keysym, down }).catch(() => {});

  const paint = (u: FrameUpdate) => {
    if (canvas.width !== u.width || canvas.height !== u.height) {
      canvas.width = u.width;
      canvas.height = u.height;
      ctx = null; // resizing a canvas clears its context state
    }
    if (u.width !== state.w || u.height !== state.h) {
      state.w = u.width;
      state.h = u.height;
      applyScale();
    }
    if (u.w > 0 && u.h > 0 && u.pixels) {
      const px = frameToRgba(atob(u.pixels), u.w, u.h);
      if (px) {
        ctx ??= canvas.getContext("2d", { alpha: false });
        ctx?.putImageData(new ImageData(px, u.w, u.h), u.x, u.y);
      }
    }
    state.seq = u.seq;
    onStatus(null);
  };

  /// Continuous stream. Each request parks in the daemon until a frame newer
  /// than `seq` exists, so pixels are already on the way when the guest paints:
  /// one hop per frame, no polling interval.
  ///
  /// `settle_ms` rides on every request rather than being set once on the
  /// session: the preset can change mid-session (the toolbar picker does), and
  /// a value the daemon cached at connect time would leave the change
  /// mysteriously inert until the tab was reopened.
  const stream = async () => {
    while (state.live) {
      let u: FrameUpdate | null = null;
      const settle = speed().settleMs;
      try {
        u = await api.get<FrameUpdate>(
          `${base}/frame?since=${Math.max(0, state.seq)}&settle_ms=${settle}`,
        );
      } catch {
        if (!state.live) break;
        // A dropped session or a transient error: back off, then surface it.
        await new Promise((r) => setTimeout(r, 400));
        try {
          const st = await api.get<{ connected: boolean; error?: string }>(`${base}/status`);
          if (!st.connected) {
            onStatus(st.error || "Console disconnected.");
            return;
          }
        } catch { /* keep trying */ }
        continue;
      }
      if (!state.live) break;
      if (!u || !u.seq) continue; // nothing new before the daemon's timeout
      paint(u);
    }
  };

  // --- input forwarding
  //
  // Pointer events rather than mouse events: a finger, a stylus and a mouse
  // all arrive here, and `setPointerCapture` keeps a drag reporting after it
  // leaves the canvas — which is precisely what dragging a window between two
  // displays looks like.
  //
  // Moves fire far faster than the guest consumes them and each one is a
  // request, so they are coalesced to ONE PER ANIMATION FRAME. Presses,
  // releases, keys and gesture outcomes always go out immediately.
  let movePending: { x: number; y: number } | null = null;
  const flushMove = (x: number, y: number) => {
    const had = movePending !== null;
    movePending = { x, y };
    if (had) return;
    requestAnimationFrame(() => {
      const p = movePending;
      movePending = null;
      if (p && state.live) void sendPointer(p.x, p.y, state.buttons);
    });
  };

  /// Send a batch of touch packets. Each one carries the mask the guest should
  /// hold *after* it, so `state.buttons` stays honest for the mouse path too.
  const sendPackets = (packets: TouchPacket[]) => {
    for (const pk of packets) {
      state.buttons = pk.buttons;
      // A press must not be reordered behind a queued move.
      movePending = null;
      const g = toGuestXY(pk.x, pk.y);
      void sendPointer(g.x, g.y, pk.buttons);
    }
  };

  /// One wheel notch, reusing the mouse wheel path so a two-finger drag and a
  /// real wheel cannot drift apart.
  const wheelAt = (pt: Pt, up: boolean) => {
    const p = toGuestXY(pt.x, pt.y);
    const bit = up ? 8 : 16; // buttons 4/5 = wheel up/down
    void sendPointer(p.x, p.y, state.buttons | bit)
      .then(() => sendPointer(p.x, p.y, state.buttons));
  };

  // ---- touch state
  //
  // A finger's gesture is DEFERRED: `touchDown` sends nothing, and a press
  // only goes out once the gesture is known. Pressing at pointerdown instead
  // would drop a button-1 into the guest before we know whether the finger is
  // a click, a drag or the beginning of a right-click, and every wrong guess
  // is a stray click on someone's desktop.
  let touch = idleTouch();
  let longTimer = 0;
  const clearLong = () => {
    if (!longTimer) return;
    window.clearTimeout(longTimer);
    longTimer = 0;
  };
  /// Live touch points, by pointer id — two of them mean a pinch or a pan.
  const touchPts = new Map<number, Pt>();
  let twoFinger: { startDist: number; startMid: Pt; startedAt: number; base: number; used: boolean } | null = null;
  let lastMid: Pt | null = null;
  /// Wheel steps owed from a two-finger drag, in px of travel.
  let wheelAcc = 0;

  /// Zoom set directly (by a pinch) rather than through the toolbar.
  const applyPinch = (factor: number) => {
    if (!twoFinger) return;
    const next = clampZoom(twoFinger.base, factor);
    if (next === state.zoom) return;
    state.zoom = next;
    applyScale();
    // The caller owns the zoom label; telling it beats having it go stale
    // until the next toolbar click.
    onZoom?.(next);
  };

  /// Two fingers moving together: pan the stage when the (zoomed) screen
  /// overflows it, otherwise scroll the guest with wheel events. A pinch turns
  /// into local zoom instead, which is the only gesture the stage must claim
  /// for itself.
  const twoFingerMove = (mid: Pt, dist: number) => {
    if (!twoFinger) return;
    const prev = lastMid ?? mid;
    lastMid = mid;
    if (Math.abs(pinchFactor(twoFinger.startDist, dist) - 1) > 0.02) {
      // Second finger moved relative to the first: a pinch, so a drag that
      // mixes both must not also scroll the guest.
      twoFinger.used = true;
      applyPinch(pinchFactor(twoFinger.startDist, dist));
      return;
    }
    const canPan =
      stage.scrollWidth > stage.clientWidth + 1 || stage.scrollHeight > stage.clientHeight + 1;
    if (canPan) {
      stage.scrollLeft -= mid.x - prev.x;
      stage.scrollTop -= mid.y - prev.y;
      twoFinger.used = true;
      return;
    }
    // Nothing to pan to, so the gesture scrolls the guest. Wheel notches are
    // emitted per 24px of travel rather than per pixel, or one flick would
    // queue hundreds of requests.
    wheelAcc += mid.y - prev.y;
    while (Math.abs(wheelAcc) >= 24) {
      const up = wheelAcc < 0;
      wheelAcc += up ? 24 : -24;
      wheelAt(mid, up);
      twoFinger.used = true;
    }
  };

  /// The field that raises the iOS software keyboard.
  ///
  /// iOS shows its keyboard only for a focused element it considers editable,
  /// and only if that element is really rendered: `display: none` and
  /// `visibility: hidden` both suppress it, so this is 1px and transparent
  /// instead. It sits INSIDE the stage on purpose — the Tab guard in focus.ts
  /// and the Ctrl+K guard in main.ts both test `closest(".console-stage")` to
  /// leave the keyboard to the guest, and that test has to keep matching.
  ///
  /// Keys are NOT read from it. They are still reported by the keydown/keyup
  /// listeners at the bottom of this function, which already call
  /// preventDefault() — so the field never accumulates text, needs no
  /// input/beforeinput handler, and there is no second reporting path to keep
  /// in sync with the first.
  const kbd = isCoarsePointer() ? document.createElement("textarea") : null;
  if (kbd) {
    kbd.className = "console-kbd";
    kbd.setAttribute("aria-label", "Guest keyboard input");
    kbd.setAttribute("autocapitalize", "off");
    kbd.setAttribute("autocorrect", "off");
    kbd.setAttribute("autocomplete", "off");
    kbd.setAttribute("spellcheck", "false");
    stage.appendChild(kbd);
  }

  // ---- on-screen keys
  //
  // A touch keyboard has no Escape, Tab or arrows, and without those a guest
  // shell is unusable, so these are not extras.
  //
  // They live here rather than in the caller's console toolbar because tapping
  // a button moves DOM focus to that button, which dismisses the software
  // keyboard — the tap has to put focus back, and this is the only place that
  // knows where the keyboard's focus belongs. They report through the same
  // `postKey` path as a real keyboard, so a tapped key and a typed one cannot
  // drift apart.
  //
  // `.mobile-only` hides the row on a desktop, where a real keyboard has these
  // keys already.
  const KEY_ROW: [string, string][] = [
    ["Esc", "Escape"], ["Tab", "Tab"], ["Ctrl", "Control"], ["Alt", "Alt"],
    ["\u2190", "ArrowLeft"], ["\u2191", "ArrowUp"], ["\u2193", "ArrowDown"], ["\u2192", "ArrowRight"],
  ];
  /// Modifiers currently held. A tap has no up event, so Ctrl and Alt latch:
  /// tap to press, tap again to release.
  const stickyKeys = new Set<string>();
  const keys = isCoarsePointer() ? document.createElement("div") : null;
  if (keys) {
    keys.className = "console-keys mobile-only";
    for (const [label, name] of KEY_ROW) {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "btn console-key";
      b.textContent = label;
      const sticky = name === "Control" || name === "Alt";
      b.onclick = () => {
        const k = keysymFor(name);
        if (!k) return;
        if (sticky) {
          const on = !stickyKeys.has(name);
          if (on) stickyKeys.add(name);
          else stickyKeys.delete(name);
          b.classList.toggle("on", on);
          void postKey(k, on);
        } else {
          // Chained, not fired together: two independent requests race, and a
          // release that overtakes its press is a key the guest never sees.
          void postKey(k, true).then(() => postKey(k, false));
        }
        (kbd ?? stage).focus();
      };
      keys.append(b);
    }
    stage.insertAdjacentElement("afterend", keys);
  }

  const onPointerDown = (e: PointerEvent) => {
    e.preventDefault();
    // Any press belongs to the console, and the console is what has focus —
    // whether that focus raises a keyboard or not.
    (kbd ?? stage).focus();
    if (e.pointerType !== "touch") {
      // Mouse and pen: press on the way down, with the real button number.
      state.buttons |= 1 << e.button;
      movePending = null;
      const p = toGuest(e);
      void sendPointer(p.x, p.y, state.buttons);
      return;
    }
    canvas.setPointerCapture(e.pointerId);
    const p = { x: e.clientX, y: e.clientY };
    touchPts.set(e.pointerId, p);

    if (touchPts.size === 1) {
      touch = touchDown(p);
      longTimer = window.setTimeout(() => {
        longTimer = 0;
        const r = touchLongPress(touch);
        touch = r.state;
        sendPackets(r.sends);
      }, LONG_PRESS_MS);
      return;
    }
    if (touchPts.size === 2) {
      // A two-finger gesture is never a long press, and it abandons the
      // one-finger gesture silently — which is the whole point of deferring
      // the press: nothing was ever sent, so there is nothing to take back.
      clearLong();
      touch = idleTouch();
      const [a, b] = [...touchPts.values()];
      twoFinger = {
        startDist: distance(a, b),
        startMid: midpoint(a, b),
        startedAt: performance.now(),
        base: state.zoom,
        used: false,
      };
      lastMid = midpoint(a, b);
      wheelAcc = 0;
    }
  };

  const onPointerMove = (e: PointerEvent) => {
    if (e.pointerType !== "touch") {
      flushMove(toGuest(e).x, toGuest(e).y);
      return;
    }
    const p = { x: e.clientX, y: e.clientY };
    touchPts.set(e.pointerId, p);
    if (touchPts.size >= 2) {
      const [a, b] = [...touchPts.values()];
      twoFingerMove(midpoint(a, b), distance(a, b));
      return;
    }
    if (touch.phase !== "held" && touch.phase !== "dragging") return;
    if (touch.phase === "held" && touch.start && distance(touch.start, p) > MOVE_SLOP_PX) {
      clearLong(); // it is a drag, not a long press
    }
    const r = touchMove(touch, p);
    touch = r.state;
    sendPackets(r.sends);
  };

  const onPointerUp = (e: PointerEvent) => {
    if (e.pointerType !== "touch") {
      state.buttons &= ~(1 << e.button);
      movePending = null;
      const p = toGuest(e);
      void sendPointer(p.x, p.y, state.buttons);
      return;
    }
    clearLong();
    const p = { x: e.clientX, y: e.clientY };
    touchPts.delete(e.pointerId);
    if (touchPts.size === 0) {
      if (twoFinger) {
        // Two fingers down and up quickly, without zooming or panning: the
        // trackpad gesture everyone already knows as a right-click.
        const el = performance.now() - twoFinger.startedAt;
        const moved = lastMid ? distance(twoFinger.startMid, lastMid) : 0;
        if (!twoFinger.used && isTwoFingerTap(el, moved)) {
          sendPackets([{ x: twoFinger.startMid.x, y: twoFinger.startMid.y, buttons: 4 }]);
          sendPackets([{ x: twoFinger.startMid.x, y: twoFinger.startMid.y, buttons: 0 }]);
        }
        twoFinger = null;
        lastMid = null;
        return;
      }
      const r = touchUp(touch, p);
      touch = r.state;
      sendPackets(r.sends);
      return;
    }
    // One finger of two lifted: the gesture is over either way, rather than
    // continuing as a drag from wherever the remaining finger happens to be.
    twoFinger = null;
    lastMid = null;
    touch = idleTouch();
  };

  const onContext = (e: Event) => e.preventDefault();
  const onWheel = (e: WheelEvent) => {
    if (e.ctrlKey) return; // Ctrl+wheel is local zoom, handled by the caller
    e.preventDefault();
    wheelAt(toGuest(e), e.deltaY < 0);
  };
  const onKeyDown = (e: KeyboardEvent) => {
    e.preventDefault();
    const k = keysymFor(e.key);
    if (k) void postKey(k, true);
  };
  const onKeyUp = (e: KeyboardEvent) => {
    e.preventDefault();
    const k = keysymFor(e.key);
    if (k) void postKey(k, false);
  };

  canvas.addEventListener("pointerdown", onPointerDown);
  canvas.addEventListener("pointermove", onPointerMove);
  canvas.addEventListener("pointerup", onPointerUp);
  canvas.addEventListener("pointercancel", onPointerUp);
  canvas.addEventListener("contextmenu", onContext);
  canvas.addEventListener("wheel", onWheel, { passive: false });
  stage.addEventListener("keydown", onKeyDown);
  stage.addEventListener("keyup", onKeyUp);

  applyScale();
  void stream();

  const handle: ConsoleHandle = {
    kind,
    getZoom: () => state.zoom,
    setZoom: (z) => { state.zoom = z; applyScale(); },
    refit: () => { if (state.zoom === 0) applyScale(); },
    sendCtrlAltDel: async () => {
      const seq = [0xffe3, 0xffe9, 0xffff]; // Ctrl, Alt, Del
      for (const k of seq) await postKey(k, true);
      for (const k of [...seq].reverse()) await postKey(k, false);
    },
    // One request, not one per character: each keystroke is a pair of RFB
    // messages, and N independent requests let a release overtake its press.
    // The daemon writes them to the guest's socket in order.
    typeText: async (text) => {
      await api.post(`${base}/type`, { text });
    },
    stop: () => {
      state.live = false;
      clearLong();
      touchPts.clear();
      canvas.removeEventListener("pointerdown", onPointerDown);
      canvas.removeEventListener("pointermove", onPointerMove);
      canvas.removeEventListener("pointerup", onPointerUp);
      canvas.removeEventListener("pointercancel", onPointerUp);
      canvas.removeEventListener("contextmenu", onContext);
      canvas.removeEventListener("wheel", onWheel);
      stage.removeEventListener("keydown", onKeyDown);
      stage.removeEventListener("keyup", onKeyUp);
      kbd?.remove();
      keys?.remove();
      // Drop the daemon-side session so the framebuffer thread stops.
      api.post(`${base}/disconnect`).catch(() => {});
    },
  };
  current = handle;
  return handle;
}
