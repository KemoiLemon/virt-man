// VNC console: noVNC in the page, over a WebSocket to the web server.
//
// The web server splices that socket to the daemon, which splices it to the
// guest's VNC display on loopback (`daemon/src/api/console.rs` `stream`). The
// RFB client, rendering, input, scaling and touch gestures are all noVNC's;
// this file only mounts it, reports its state and offers the few controls the
// toolbar has (fit / 1:1, Ctrl+Alt+Del, typing text, a phone's missing keys).
//
// SPICE is not RFB and has no usable browser client, so those guests are handed
// a virt-viewer file instead of being rendered here.

import { isCoarsePointer } from "./touch";

export interface ConsoleHandle {
  kind: string;
  /** 0 fits the guest to the stage; anything else is shown at 1:1. */
  getZoom(): number;
  setZoom(z: number): void;
  refit(): void;
  sendCtrlAltDel(): Promise<void>;
  /** Type a string into the guest as synthetic key events. */
  typeText(text: string): Promise<void>;
  /** Close the connection and remove the viewer. */
  stop(): void;
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

/// The keysym for one typed character: Latin-1 is its own code point, anything
/// else is the X11 Unicode range, and a newline is Return.
export function charKeysym(ch: string): number {
  if (ch === "\n" || ch === "\r") return 0xff0d;
  if (ch === "\t") return 0xff09;
  const cp = ch.codePointAt(0)!;
  return cp < 0x100 ? cp : 0x01000000 + cp;
}

/// The WebSocket URL for a console base such as
/// `/proxy/{conn}/domains/{name}/console`.
export function streamUrl(base: string, loc: { protocol: string; host: string }): string {
  return `${loc.protocol === "https:" ? "wss" : "ws"}://${loc.host}${base}/stream`;
}

/**
 * Attach the viewer to `stage` for one VM.
 *
 * `base` is the console endpoint prefix, e.g.
 * `/proxy/{conn}/domains/{name}/console`. Returns null when the guest is not
 * VNC; `onStatus` carries the reason, and later the reason a live console
 * dropped.
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

  // Loaded here, not at start-up: it is the largest piece of the app and only
  // a VNC console tab needs it.
  const { default: RFB } = await import("@novnc/novnc");

  const screen = document.createElement("div");
  screen.className = "vnc-screen";
  stage.appendChild(screen);
  if (!stage.hasAttribute("tabindex")) stage.tabIndex = 0;

  let rfb: InstanceType<typeof RFB>;
  try {
    rfb = new RFB(screen, streamUrl(base, location), { shared: true });
  } catch (e) {
    screen.remove();
    onStatus((e as Error).message);
    return null;
  }
  rfb.scaleViewport = true;
  rfb.background = "transparent";

  let live = true;
  rfb.addEventListener("connect", () => onStatus(null));
  rfb.addEventListener("disconnect", (e) => {
    if (!live) return;
    onStatus((e as CustomEvent).detail?.clean ? "Console disconnected." : "Console connection lost.");
  });
  rfb.addEventListener("securityfailure", (e) => {
    onStatus((e as CustomEvent).detail?.reason || "Console security handshake failed.");
  });
  // Only security type None is expected from QEMU here; a password prompt is
  // a guest definition this app did not write.
  rfb.addEventListener("credentialsrequired", () => {
    onStatus("This guest's VNC server asks for a password, which is not supported.");
    rfb.disconnect();
  });

  let zoom = 0;

  // ---- on-screen keys
  //
  // A touch keyboard has no Escape, Tab or arrows, and without those a guest
  // shell is unusable. `.mobile-only` hides the row on a desktop.
  const KEY_ROW: [string, string][] = [
    ["Esc", "Escape"], ["Tab", "Tab"], ["Ctrl", "Control"], ["Alt", "Alt"],
    ["←", "ArrowLeft"], ["↑", "ArrowUp"], ["↓", "ArrowDown"], ["→", "ArrowRight"],
  ];
  /// A tap has no up event, so Ctrl and Alt latch: tap to press, tap to release.
  const sticky = new Set<string>();
  const keys = isCoarsePointer() ? document.createElement("div") : null;
  if (keys) {
    keys.className = "console-keys mobile-only";
    for (const [label, name] of KEY_ROW) {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "btn console-key";
      b.textContent = label;
      b.onclick = () => {
        const k = keysymFor(name);
        if (name === "Control" || name === "Alt") {
          const on = !sticky.has(name);
          if (on) sticky.add(name);
          else sticky.delete(name);
          b.classList.toggle("on", on);
          rfb.sendKey(k, null, on);
        } else {
          rfb.sendKey(k, null);
        }
        rfb.focus();
      };
      keys.append(b);
    }
    stage.insertAdjacentElement("afterend", keys);
  }

  const handle: ConsoleHandle = {
    kind,
    getZoom: () => zoom,
    setZoom: (z) => {
      zoom = z;
      rfb.scaleViewport = z === 0;
    },
    refit: () => {},
    sendCtrlAltDel: async () => rfb.sendCtrlAltDel(),
    typeText: async (text) => {
      for (const ch of text) {
        const k = charKeysym(ch);
        rfb.sendKey(k, null, true);
        rfb.sendKey(k, null, false);
      }
    },
    stop: () => {
      live = false;
      keys?.remove();
      rfb.disconnect();
      screen.remove();
    },
  };
  current = handle;
  return handle;
}
