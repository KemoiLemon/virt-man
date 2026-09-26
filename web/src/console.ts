// VNC console: a canvas fed raw pixels by the daemon.
//
// Ported from vm-man, which drives the same viewer over Tauri IPC. The RFB
// client itself lives in the daemon (daemon/src/rfb.rs): it holds the guest
// framebuffer and hands out only the rectangle that changed, as base64 BGRA.
// So there is no VNC client in the browser at all — no noVNC, no WebSocket,
// just putImageData.
//
// Scaling contract (also from vm-man): the displayed size is computed from the
// LOCAL stage box and devicePixelRatio, using the guest resolution purely as an
// aspect ratio. The guest is never asked to resize, and guest pixels never set
// layout size.
//
// SPICE is not RFB and has no usable browser client, so those guests are handed
// a virt-viewer file instead of being rendered here.

import { api } from "./api";

export interface ConsoleHandle {
  kind: string;
  /** Local zoom: 0 means fit-to-stage, otherwise a scale factor. */
  getZoom(): number;
  setZoom(z: number): void;
  refit(): void;
  sendCtrlAltDel(): Promise<void>;
  /** Stop streaming and drop the daemon-side session. */
  stop(): void;
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

/// Decode one base64 BGRA rectangle into RGBA for putImageData.
/// The guest sends BGRX, so alpha is forced opaque.
// The ArrayBuffer parameter is load-bearing: TypeScript otherwise infers
// ArrayBufferLike, which ImageData refuses because it could be shared.
export function bgraToRgba(
  bin: string,
  w: number,
  h: number,
): Uint8ClampedArray<ArrayBuffer> | null {
  const need = w * h * 4;
  if (bin.length < need) return null;
  const px = new Uint8ClampedArray(new ArrayBuffer(need));
  for (let i = 0; i < need; i += 4) {
    px[i] = bin.charCodeAt(i + 2) & 255;
    px[i + 1] = bin.charCodeAt(i + 1) & 255;
    px[i + 2] = bin.charCodeAt(i) & 255;
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

  /// Map a local mouse event to guest pixels (the inverse of the scale).
  const toGuest = (ev: MouseEvent) => {
    const r = canvas.getBoundingClientRect();
    const x = ((ev.clientX - r.left) / (r.width || 1)) * state.w;
    const y = ((ev.clientY - r.top) / (r.height || 1)) * state.h;
    return {
      x: Math.max(0, Math.min(state.w - 1, Math.round(x))),
      y: Math.max(0, Math.min(state.h - 1, Math.round(y))),
    };
  };

  const sendPointer = (x: number, y: number, buttons: number) =>
    api.post(`${base}/pointer`, { x, y, buttons }).catch(() => {});
  const sendKey = (keysym: number, down: boolean) =>
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
      const px = bgraToRgba(atob(u.pixels), u.w, u.h);
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
  const stream = async () => {
    while (state.live) {
      let u: FrameUpdate | null = null;
      try {
        u = await api.get<FrameUpdate>(`${base}/frame?since=${Math.max(0, state.seq)}`);
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
  // Mouse moves fire far faster than the guest consumes them and each one is a
  // request, so coalesce to one per animation frame. Buttons and keys always go
  // immediately.
  let movePending: { x: number; y: number } | null = null;
  const onMove = (e: MouseEvent) => {
    const had = movePending !== null;
    movePending = toGuest(e);
    if (had) return;
    requestAnimationFrame(() => {
      const p = movePending;
      movePending = null;
      if (p && state.live) void sendPointer(p.x, p.y, state.buttons);
    });
  };
  const onDown = (e: MouseEvent) => {
    e.preventDefault();
    stage.focus();
    state.buttons |= 1 << e.button;
    movePending = null; // a click must not be reordered behind a move
    const p = toGuest(e);
    void sendPointer(p.x, p.y, state.buttons);
  };
  const onUp = (e: MouseEvent) => {
    state.buttons &= ~(1 << e.button);
    movePending = null;
    const p = toGuest(e);
    void sendPointer(p.x, p.y, state.buttons);
  };
  const onContext = (e: Event) => e.preventDefault();
  const onWheel = (e: WheelEvent) => {
    if (e.ctrlKey) return; // Ctrl+wheel is local zoom, handled by the caller
    e.preventDefault();
    const p = toGuest(e);
    const bit = e.deltaY < 0 ? 8 : 16; // buttons 4/5 = wheel up/down
    void sendPointer(p.x, p.y, state.buttons | bit)
      .then(() => sendPointer(p.x, p.y, state.buttons));
  };
  const onKeyDown = (e: KeyboardEvent) => {
    e.preventDefault();
    const k = keysymFor(e.key);
    if (k) void sendKey(k, true);
  };
  const onKeyUp = (e: KeyboardEvent) => {
    e.preventDefault();
    const k = keysymFor(e.key);
    if (k) void sendKey(k, false);
  };

  canvas.addEventListener("mousemove", onMove);
  canvas.addEventListener("mousedown", onDown);
  window.addEventListener("mouseup", onUp);
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
      for (const k of seq) await sendKey(k, true);
      for (const k of [...seq].reverse()) await sendKey(k, false);
    },
    stop: () => {
      state.live = false;
      canvas.removeEventListener("mousemove", onMove);
      canvas.removeEventListener("mousedown", onDown);
      window.removeEventListener("mouseup", onUp);
      canvas.removeEventListener("contextmenu", onContext);
      canvas.removeEventListener("wheel", onWheel);
      stage.removeEventListener("keydown", onKeyDown);
      stage.removeEventListener("keyup", onKeyUp);
      // Drop the daemon-side session so the framebuffer thread stops.
      api.post(`${base}/disconnect`).catch(() => {});
    },
  };
  current = handle;
  return handle;
}

export function isTauri(): boolean {
  return typeof (window as any).__TAURI__ !== "undefined";
}

export function tauriWindow(): any | null {
  return (window as any).__TAURI__?.window?.getCurrentWindow?.() ?? null;
}
