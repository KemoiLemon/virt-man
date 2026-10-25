// A text terminal in the browser, for a container's console and for a shell on
// the daemon's host.
//
// Both are the same long-polled byte stream: `connect` opens a session,
// `read?after=<offset>` parks until there are bytes past the offset, `write`
// sends keystrokes, `resize` carries the geometry, and `disconnect` drops the
// session. There is deliberately no poll interval here: the next read is
// issued as soon as the last one answers, exactly like the VNC frame loop.
//
// xterm.js does the terminal emulation (a VT100 renderer is not worth
// writing). Its colours come from the theme's CSS variables, read once at
// attach, so the terminal follows light/dark like everything else and no hex
// lives outside the theme blocks.
//
// A shell session belongs to the page that opened it, so `pagehide` beacons a
// disconnect: a closed tab must not leave a login shell running.

import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { fitSize } from "./console-geometry";

let current: { stop: () => void } | null = null;
/// Bumped by every attach and detach, so an attach can tell it was superseded.
let attachSeq = 0;

const b64 = {
  encode(bytes: Uint8Array): string {
    let s = "";
    for (const b of bytes) s += String.fromCharCode(b);
    return btoa(s);
  },
  decode(s: string): Uint8Array {
    const bin = atob(s);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  },
};

function themeColors(el: Element) {
  const css = getComputedStyle(el);
  const v = (name: string) => css.getPropertyValue(name).trim() || undefined;
  return {
    background: v("--code-bg"),
    foreground: v("--fg"),
    cursor: v("--accent"),
    selectionBackground: v("--accent-bg"),
  };
}

async function post<T>(url: string, body?: unknown): Promise<T> {
  const r = await fetch(url, {
    method: "POST",
    credentials: "same-origin",
    headers: body === undefined ? {} : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const data = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error((data as { error?: string }).error ?? r.statusText);
  return data as T;
}

/// Best-effort disconnect during unload. A normal fetch can be cancelled as
/// the page tears down; sendBeacon is delivered by the browser, and being
/// same-origin it carries the session cookie.
function beacon(url: string) {
  if (navigator.sendBeacon?.(url)) return;
  void fetch(url, {
    method: "POST",
    credentials: "same-origin",
    keepalive: true,
  }).catch(() => undefined);
}

/// Mount a terminal against a live session: `<base>/read`, `<base>/write`,
/// `<base>/resize`, dropped with `disconnectUrl`. `startOffset` is where the
/// read loop begins (a joined console starts from what it already buffered).
async function mountTerminal(
  stage: HTMLElement,
  base: string,
  status: (msg: string | null) => void,
  startOffset: number,
  disconnectUrl: string,
  reconnecting = false,
): Promise<"stopped" | "ended"> {
  let stopped = false;
  let ro: ResizeObserver | null = null;
  const term = new Terminal({
    fontFamily: getComputedStyle(stage).fontFamily,
    fontSize: 13,
    cursorBlink: true,
    convertEol: false,
    theme: themeColors(stage),
    scrollback: 5000,
  });

  const onPageHide = () => {
    if (stopped) return;
    stopped = true;
    term.dispose();
    ro?.disconnect();
    beacon(disconnectUrl);
  };
  const stop = () => {
    if (stopped) return;
    stopped = true;
    window.removeEventListener("pagehide", onPageHide);
    term.dispose();
    ro?.disconnect();
    void post(disconnectUrl).catch(() => undefined);
  };
  window.addEventListener("pagehide", onPageHide);

  let offset = startOffset;
  status(null);
  stage.replaceChildren();
  term.open(stage);
  // The status line went with the stage's children, so a later failure is
  // written into the terminal itself, where it can be seen.
  const say = (m: string) => term.write(`\r\n\x1b[31m${m}\x1b[0m\r\n`);

  // Size the terminal to the stage: xterm has no fit without its addon, and
  // the arithmetic is two lines.
  // One character in the terminal's own font, measured in the page: xterm's
  // cell size is not public API, and the font is fixed-width, so a span of
  // ten characters divided by ten is the cell.
  const probe = document.createElement("span");
  probe.textContent = "MMMMMMMMMM";
  probe.style.cssText = `position:absolute;visibility:hidden;white-space:pre;font-size:13px;font-family:${term.options.fontFamily}`;
  const cell = () => {
    stage.append(probe);
    const r = probe.getBoundingClientRect();
    probe.remove();
    return r.width > 0 ? { width: r.width / 10, height: r.height } : { width: 8, height: 17 };
  };
  // The pty must match xterm's geometry. The daemon starts a shell pty at the
  // browser's default; either way the shell's line editor redraws the same
  // bottom line instead of scrolling when the two disagree. Report every
  // change; `lastSize` starts impossible, so the first fit is always sent even
  // when it equals xterm's default 80x24.
  let lastSize: [number, number] | null = null;
  const reportSize = (cols: number, rows: number) => {
    void post(`${base}/resize`, { cols, rows }).catch(() => undefined);
  };
  const fit = () => {
    const c = cell();
    const size = fitSize(stage.clientWidth, stage.clientHeight, c);
    if (size.cols !== term.cols || size.rows !== term.rows) term.resize(size.cols, size.rows);
    if (!lastSize || size.cols !== lastSize[0] || size.rows !== lastSize[1]) {
      lastSize = [size.cols, size.rows];
      reportSize(size.cols, size.rows);
    }
  };
  ro = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(fit);
  ro?.observe(stage);
  fit();
  stage.onclick = () => term.focus();
  current = { stop };

  // Input: queued and sent one request at a time, so keystrokes arrive in
  // order however fast they are typed.
  const enc = new TextEncoder();
  let queue: Uint8Array[] = [];
  let sending = false;
  const flush = async () => {
    if (sending || !queue.length) return;
    sending = true;
    const total = queue.reduce((n, q) => n + q.length, 0);
    const buf = new Uint8Array(total);
    let at = 0;
    for (const q of queue) { buf.set(q, at); at += q.length; }
    queue = [];
    try {
      await post(`${base}/write`, { data: b64.encode(buf) });
    } catch (e) {
      say((e as Error).message);
    } finally {
      sending = false;
      if (queue.length) void flush();
    }
  };
  term.onData((d) => {
    queue.push(enc.encode(d));
    void flush();
  });

  // Output: one parked read after another.
  while (!stopped) {
    try {
      const r = await fetch(`${base}/read?after=${offset}`, { credentials: "same-origin" });
      const data = await r.json().catch(() => ({}));
      if (stopped) return "stopped";
      if (!r.ok) throw new Error((data as { error?: string }).error ?? r.statusText);
      const d = data as { data: string; offset: number; closed: boolean; error: string | null; lost: boolean };
      if (d.data) term.write(b64.decode(d.data));
      offset = d.offset;
      if (d.closed) {
        say(`${d.error ?? "The session closed."}${reconnecting ? " Reconnecting…" : ""}`);
        return "ended";
      }
    } catch (e) {
      if (stopped) return "stopped";
      say(`${(e as Error).message}${reconnecting ? " Reconnecting…" : ""}`);
      return "ended";
    }
  }
  return "stopped";
}

/// Attach a terminal to a container's text console for the guest at `base`
/// (`/proxy/<c>/containers/<ct>`). Replaces any terminal already attached.
/// Resolves once the session is mounted, or `false` when it could not open.
export async function attachTextConsole(
  stage: HTMLElement,
  base: string,
  status: (msg: string | null) => void,
): Promise<boolean> {
  detachTextConsole();
  let r: { offset: number; joined: boolean };
  try {
    // No size yet: the stage is measured once xterm is mounted, and the first
    // fit reports it through `resize` before the shell draws anything.
    r = await post<{ offset: number; joined: boolean }>(`${base}/console/connect`, {});
  } catch (e) {
    status((e as Error).message);
    return false;
  }
  // A second tab joins the session already open: start from what is buffered
  // rather than from nothing, so it shows the current prompt.
  const offset = r.joined ? Math.max(0, r.offset - 64 * 1024) : 0;
  void mountTerminal(stage, `${base}/console`, status, offset, `${base}/console/disconnect`);
  return true;
}

/// Open a login shell on the daemon's host. The daemon runs the host's
/// `login(1)` in the pty, so the terminal *is* the login UI: it shows the real
/// `login:` / `Password:` prompt, and the password never passes through this
/// code. `base` is `/proxy/<conn>/shell`. Replaces any terminal attached.
/// Reconnects when the session ends on its own; resolves when the view is left,
/// or `false` when the first connect was refused.
export async function attachDaemonShell(
  stage: HTMLElement,
  base: string,
  status: (msg: string | null) => void,
): Promise<boolean> {
  detachTextConsole();
  const mine = ++attachSeq;
  // Superseded: the tab was re-rendered or left, or a second attach started.
  // Mounting then would put a second terminal on the same stage or on one that
  // is gone.
  const gone = () => mine !== attachSeq || !stage.isConnected;
  let delay = 500;
  for (let first = true; ; first = false) {
    let r: { id: string; offset: number };
    try {
      r = await post<{ id: string; offset: number }>(`${base}/connect`, {});
    } catch (e) {
      // The first refusal is the user's to read; after a session has run, keep
      // trying quietly (the daemon may be restarting).
      if (first) {
        status((e as Error).message);
        return false;
      }
      await new Promise((ok) => setTimeout(ok, (delay = Math.min(delay * 2, 8000))));
      if (gone()) return true;
      continue;
    }
    const session = `${base}/${encodeURIComponent(r.id)}`;
    if (gone()) {
      void post(`${session}/disconnect`).catch(() => undefined);
      return false;
    }
    const started = Date.now();
    const out = await mountTerminal(stage, session, status, r.offset ?? 0, `${session}/disconnect`, true);
    if (out === "stopped" || gone()) return true;
    // The session ended on its own (the shell exited, or the daemon dropped
    // it): start a fresh login, like a getty. A session that dies at once backs
    // off, so a broken `login` is retried gently instead of in a tight loop.
    current?.stop();
    current = null;
    delay = Date.now() - started > 5000 ? 500 : Math.min(delay * 2, 8000);
    await new Promise((ok) => setTimeout(ok, delay));
    if (gone()) return true;
  }
}

/// Close the terminal, if one is open. Called when the view is left.
export function detachTextConsole() {
  attachSeq++;
  current?.stop();
  current = null;
}
