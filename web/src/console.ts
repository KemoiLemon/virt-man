// Console tab: VNC via noVNC, SPICE via spice-html5. Both talk to the
// backend's TLS-pinned WebSocket proxy, so only the session cookie is needed.
//
// noVNC and spice-html5 are imported dynamically: they are only needed when a
// console opens, it keeps them out of the main bundle, and noVNC's top-level
// await cannot be bundled into a classic script.

/** Encoding trade-off presets, mapped onto noVNC's 0..9 scales. */
export type Preset = "fast" | "balanced" | "quality";

/**
 * noVNC exposes two independent knobs:
 *  - qualityLevel 0..9: JPEG quality for Tight encoding (9 = lossless-ish)
 *  - compressionLevel 0..9: zlib effort (9 = smallest, most host CPU)
 *
 * "fast" trades visual fidelity for latency on a slow link; "quality" sends
 * near-lossless frames and leans on a fast link; "balanced" is noVNC's own
 * default pairing.
 */
export const PRESETS: Record<Preset, { quality: number; compression: number; label: string; hint: string }> = {
  fast: {
    quality: 3, compression: 9,
    label: "Fast",
    hint: "Lowest latency: lossy frames, smallest payloads. Best over slow links.",
  },
  balanced: {
    quality: 6, compression: 2,
    label: "Balanced",
    hint: "noVNC's defaults: good fidelity without much host CPU.",
  },
  quality: {
    quality: 9, compression: 0,
    label: "Quality",
    hint: "Near-lossless frames and no compression effort. Best on a LAN.",
  },
};

export interface ConsoleHandle {
  stop: () => void;
  /** Guest -> host clipboard text, updated as the guest copies. */
  onGuestClipboard: (fn: (text: string) => void) => void;
  /** Host -> guest: paste text into the guest's clipboard. */
  pasteToGuest: (text: string) => void;
  /** Type text as key events, for guests without a clipboard agent. */
  typeToGuest: (text: string) => Promise<void>;
  setPreset: (p: Preset) => void;
  /** 0 = fit to the stage; otherwise an explicit scale factor. */
  setZoom: (factor: number) => void;
  getZoom: () => number;
  /** Re-fit after the stage changes size. */
  refit: () => void;
  sendCtrlAltDel: () => void;
  kind: "vnc" | "spice";
  /** Which controls this backend can actually honour. */
  supports: { clipboard: boolean; zoom: boolean; preset: boolean; type: boolean };
}

let active: ConsoleHandle | null = null;

export function detachConsole() {
  if (active) {
    try {
      active.stop();
    } catch {
      /* ignore */
    }
    active = null;
  }
}

export function currentConsole(): ConsoleHandle | null {
  return active;
}

export async function attachConsole(
  stage: HTMLElement,
  kind: string,
  wsUrl: string,
  onStatus: (msg: string | null) => void,
  preset: Preset = "balanced",
): Promise<ConsoleHandle | null> {
  detachConsole();
  stage.textContent = "";

  active = kind === "spice"
    ? await attachSpice(stage, wsUrl, onStatus)
    : await attachVnc(stage, wsUrl, onStatus, preset);
  return active;
}

async function attachVnc(
  stage: HTMLElement,
  wsUrl: string,
  onStatus: (m: string | null) => void,
  preset: Preset,
): Promise<ConsoleHandle | null> {
  onStatus("connecting…");
  try {
    const RFB = (await import("@novnc/novnc")).default;
    const rfb = new RFB(stage, wsUrl, { credentials: {} });

    // Fit-to-stage by default; an explicit zoom switches to clipped scrolling.
    let zoom = 0;
    rfb.scaleViewport = true;
    rfb.clipViewport = false;
    // Never ask the guest to change its resolution: scaling is local, which
    // keeps a small guest crisp and avoids fighting the display driver.
    rfb.resizeSession = false;
    rfb.background = "#000";

    const applyPreset = (p: Preset) => {
      const { quality, compression } = PRESETS[p];
      rfb.qualityLevel = quality;
      rfb.compressionLevel = compression;
    };
    applyPreset(preset);

    let clipboardSink: ((t: string) => void) | null = null;
    rfb.addEventListener("clipboard", (e: any) => {
      const text = e?.detail?.text;
      if (typeof text === "string") clipboardSink?.(text);
    });
    rfb.addEventListener("connect", () => onStatus(null));
    rfb.addEventListener("disconnect", () => onStatus("disconnected"));
    rfb.addEventListener("credentialsrequired", () => onStatus("credentials required"));

    const applyZoom = () => {
      if (zoom === 0) {
        // Fit: noVNC scales the canvas to the stage box.
        rfb.scaleViewport = true;
        rfb.clipViewport = false;
      } else {
        // Explicit zoom: clip and scroll, so overflow stays reachable.
        rfb.scaleViewport = false;
        rfb.clipViewport = true;
        const canvas = stage.querySelector("canvas");
        if (canvas) {
          canvas.style.transformOrigin = "center center";
          canvas.style.transform = `scale(${zoom})`;
        }
      }
    };

    const handle: ConsoleHandle = {
      kind: "vnc",
      supports: { clipboard: true, zoom: true, preset: true, type: true },
      stop: () => {
        try {
          rfb.disconnect();
        } catch {
          /* ignore */
        }
      },
      onGuestClipboard: (fn) => { clipboardSink = fn; },
      pasteToGuest: (text) => rfb.clipboardPasteFrom(text),
      typeToGuest: (text) => typeText(rfb, text),
      setPreset: applyPreset,
      setZoom: (factor) => {
        zoom = factor;
        applyZoom();
      },
      getZoom: () => zoom,
      refit: () => applyZoom(),
      sendCtrlAltDel: () => rfb.sendCtrlAltDel(),
    };
    return handle;
  } catch (e) {
    onStatus(`noVNC error: ${e instanceof Error ? e.message : e}`);
    return null;
  }
}

/**
 * Send text as individual key events.
 *
 * Needed when the guest has no clipboard channel (no agent, or a display
 * server that ignores RFB clipboard messages) — the characters still arrive
 * because they look like ordinary typing.
 */
async function typeText(rfb: any, text: string): Promise<void> {
  for (const ch of text) {
    // Enter must be the keysym, not the literal newline codepoint.
    const keysym = ch === "\n" || ch === "\r" ? 0xff0d : ch.codePointAt(0);
    if (keysym === undefined) continue;
    rfb.sendKey(keysym, null, true);
    rfb.sendKey(keysym, null, false);
    // A short gap: bursting faster than the guest's input layer drops keys.
    await new Promise((r) => setTimeout(r, 8));
  }
}

/**
 * Map a character to a `KeyboardEvent.code`, which is what spice-html5's
 * key handler translates into a scancode. Only the unshifted ASCII range is
 * covered; anything else is skipped rather than sent as the wrong key.
 */
function spiceCodeFor(ch: string): { code: string; shift: boolean } | null {
  if (ch >= "a" && ch <= "z") return { code: `Key${ch.toUpperCase()}`, shift: false };
  if (ch >= "A" && ch <= "Z") return { code: `Key${ch}`, shift: true };
  if (ch >= "0" && ch <= "9") return { code: `Digit${ch}`, shift: false };
  const plain: Record<string, string> = {
    " ": "Space", "\n": "Enter", "\r": "Enter", "\t": "Tab",
    "-": "Minus", "=": "Equal", "[": "BracketLeft", "]": "BracketRight",
    "\\": "Backslash", ";": "Semicolon", "'": "Quote", ",": "Comma",
    ".": "Period", "/": "Slash", "`": "Backquote",
  };
  if (plain[ch]) return { code: plain[ch], shift: false };
  const shifted: Record<string, string> = {
    "!": "Digit1", "@": "Digit2", "#": "Digit3", "$": "Digit4", "%": "Digit5",
    "^": "Digit6", "&": "Digit7", "*": "Digit8", "(": "Digit9", ")": "Digit0",
    "_": "Minus", "+": "Equal", "{": "BracketLeft", "}": "BracketRight",
    "|": "Backslash", ":": "Semicolon", '"': "Quote", "<": "Comma",
    ">": "Period", "?": "Slash", "~": "Backquote",
  };
  if (shifted[ch]) return { code: shifted[ch], shift: true };
  return null;
}

/**
 * Type text into a SPICE guest.
 *
 * spice-html5 builds its scancode from a real `KeyboardEvent.code`, so
 * synthetic events through its own handler are more reliable than
 * hand-rolling the scancode table.
 */
async function typeSpiceText(text: string): Promise<void> {
  const target = document.getElementById("spice-screen") ?? document.body;
  for (const ch of text) {
    const mapped = spiceCodeFor(ch);
    if (!mapped) continue;
    const init: KeyboardEventInit = {
      code: mapped.code,
      key: ch,
      shiftKey: mapped.shift,
      bubbles: true,
      cancelable: true,
    };
    target.dispatchEvent(new KeyboardEvent("keydown", init));
    target.dispatchEvent(new KeyboardEvent("keyup", init));
    await new Promise((r) => setTimeout(r, 8));
  }
}

async function attachSpice(
  stage: HTMLElement,
  wsUrl: string,
  onStatus: (m: string | null) => void,
): Promise<ConsoleHandle | null> {
  onStatus("connecting…");
  try {
    const spice = await import("@spice-project/spice-html5/src/main.js");
    const screen = document.createElement("div");
    screen.id = "spice-screen";
    screen.className = "spice-screen";
    // Do NOT force width/height here: spice-html5 sets this element's height
    // to the guest's surface height when the primary surface arrives
    // (display.js). Pinning it to 100% fights that and leaves the canvas
    // mis-sized, which looks like a frozen display.
    stage.appendChild(screen);

    const sc = new (spice as any).SpiceMainConn({
      uri: wsUrl,
      screen_id: "spice-screen",
      onerror: () => onStatus("spice connection failed"),
      onsuccess: () => onStatus(null),
    });

    let zoom = 0;
    const applyZoom = () => {
      // spice-html5 renders into a canvas sized to the guest surface. Scale
      // it with a transform: object-fit does nothing for canvas content, and
      // changing width/height would fight the library's own sizing.
      const canvas = screen.querySelector("canvas");
      if (!canvas) return;
      canvas.style.transformOrigin = "center center";
      if (zoom === 0) {
        // Fit: scale down to the stage, never up past 1:1 so a small guest
        // stays crisp.
        const box = stage.getBoundingClientRect();
        const cw = canvas.width || canvas.clientWidth;
        const ch = canvas.height || canvas.clientHeight;
        if (!cw || !ch || !box.width || !box.height) return;
        const factor = Math.min(1, box.width / cw, box.height / ch);
        canvas.style.transform = factor < 1 ? `scale(${factor})` : "";
      } else {
        canvas.style.transform = `scale(${zoom})`;
      }
    };
    // The canvas only appears once the primary surface arrives, and it is
    // replaced on a guest resolution change, so watch instead of guessing a
    // delay. Re-fitting on every mutation also keeps zoom applied after the
    // library swaps the canvas out.
    const canvasWatcher = new MutationObserver(() => applyZoom());
    canvasWatcher.observe(screen, { childList: true, subtree: true });

    const handle: ConsoleHandle = {
      kind: "spice",
      // spice-html5 defines the VD_AGENT_CLIPBOARD constants but implements
      // no clipboard path, and SPICE negotiates its own encoding, so neither
      // control applies here. Typing still works: it is just key events.
      supports: { clipboard: false, zoom: true, preset: false, type: true },
      stop: () => {
        canvasWatcher.disconnect();
        try {
          sc.stop();
        } catch {
          /* ignore */
        }
      },
      onGuestClipboard: () => {},
      pasteToGuest: () => {},
      // Typing is plain key events, which SPICE handles like any other input.
      typeToGuest: (text) => typeSpiceText(text),
      setPreset: () => {},
      setZoom: (factor) => {
        zoom = factor;
        applyZoom();
      },
      getZoom: () => zoom,
      refit: applyZoom,
      sendCtrlAltDel: () => {
        try {
          // Takes no argument: it acts on spice-html5's own module-global
          // connection, so passing `sc` here was simply ignored.
          (spice as any).sendCtrlAltDel();
        } catch {
          /* ignore */
        }
      },
    };
    return handle;
  } catch (e) {
    onStatus(`spice error: ${e instanceof Error ? e.message : e}`);
    return null;
  }
}

// The browser build has no Tauri window controls; guard them so the desktop
// shell can wire its frameless titlebar.
export function isTauri(): boolean {
  return (window as any).__TAURI_INTERNALS__ !== undefined;
}

/// The current Tauri window, from the injected global (withGlobalTauri: true).
/// Synchronous, matching vm-man's wiring, so titlebar buttons and dragging
/// don't depend on a dynamic import resolving.
export function tauriWindow(): any | null {
  if (!isTauri()) return null;
  const tauri = (window as any).__TAURI__;
  if (tauri?.window?.getCurrentWindow) {
    try {
      return tauri.window.getCurrentWindow();
    } catch {
      return null;
    }
  }
  return null;
}
