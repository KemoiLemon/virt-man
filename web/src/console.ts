// Console tab: VNC via noVNC, SPICE via spice-html5. Both talk to the
// backend's TLS-pinned WebSocket proxy, so only the session cookie is needed.

import RFB from "@novnc/novnc";

let active: { stop: () => void } | null = null;

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

export function attachConsole(
  stage: HTMLElement,
  kind: string,
  wsUrl: string,
  onStatus: (msg: string | null) => void,
) {
  detachConsole();
  stage.innerHTML = "";

  if (kind === "spice") {
    attachSpice(stage, wsUrl, onStatus);
  } else {
    attachVnc(stage, wsUrl, onStatus);
  }
}

function attachVnc(stage: HTMLElement, wsUrl: string, onStatus: (m: string | null) => void) {
  onStatus("connecting…");
  try {
    const rfb = new RFB(stage, wsUrl, { credentials: {} });
    rfb.scaleViewport = true;
    rfb.resizeSession = false;
    rfb.addEventListener("connect", () => onStatus(null));
    rfb.addEventListener("disconnect", () => onStatus("disconnected"));
    rfb.addEventListener("credentialsrequired", () => onStatus("credentials required"));
    active = {
      stop: () => {
        try {
          rfb.disconnect();
        } catch {
          /* ignore */
        }
      },
    };
  } catch (e) {
    onStatus(`noVNC error: ${e instanceof Error ? e.message : e}`);
  }
}

async function attachSpice(
  stage: HTMLElement,
  wsUrl: string,
  onStatus: (m: string | null) => void,
) {
  onStatus("connecting…");
  try {
    const spice = await import("@spice-project/spice-html5/src/main.js");
    const screen = document.createElement("div");
    screen.id = "spice-screen";
    screen.className = "spice-screen";
    screen.style.width = "100%";
    screen.style.height = "100%";
    stage.appendChild(screen);

    const sc = new (spice as any).SpiceMainConn({
      uri: wsUrl,
      screen_id: "spice-screen",
      onerror: () => onStatus("spice connection failed"),
      onsuccess: () => onStatus(null),
    });
    active = {
      stop: () => {
        try {
          sc.stop();
        } catch {
          /* ignore */
        }
      },
    };
  } catch (e) {
    onStatus(`spice error: ${e instanceof Error ? e.message : e}`);
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
