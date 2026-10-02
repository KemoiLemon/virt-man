// A container's text console in the browser.
//
// The daemon exposes the container's pty as a long-polled byte stream
// (`daemon/src/api/ct_console.rs`): `connect`, then `read?after=<offset>`
// parks until there are bytes past the offset, and `write` sends keystrokes.
// There is deliberately no poll interval here: the next read is issued as soon
// as the last one answers, exactly like the VNC frame loop.
//
// xterm.js does the terminal emulation (a VT100 renderer is not worth
// writing). Its colours come from the theme's CSS variables, read once at
// attach, so the terminal follows light/dark like everything else and no hex
// lives outside the theme blocks.

import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { fitSize } from "./console-geometry";

let current: { stop: () => void } | null = null;

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

/// Attach a terminal to `stage` for the guest at `base` (`/proxy/<c>/containers/<ct>`).
/// Replaces any terminal already attached.
export async function attachTextConsole(
  stage: HTMLElement,
  base: string,
  status: (msg: string | null) => void,
): Promise<void> {
  detachTextConsole();
  let stopped = false;
  const term = new Terminal({
    fontFamily: getComputedStyle(stage).fontFamily,
    fontSize: 13,
    cursorBlink: true,
    convertEol: false,
    theme: themeColors(stage),
    scrollback: 5000,
  });
  const stop = () => {
    stopped = true;
    term.dispose();
    ro?.disconnect();
    void post(`${base}/console/disconnect`).catch(() => undefined);
  };
  current = { stop };

  let offset = 0;
  try {
    const r = await post<{ offset: number; joined: boolean }>(`${base}/console/connect`);
    // A second tab joins the session already open: start from what is
    // buffered rather than from nothing, so it shows the current prompt.
    offset = r.joined ? Math.max(0, r.offset - 64 * 1024) : 0;
  } catch (e) {
    status((e as Error).message);
    current = null;
    term.dispose();
    return;
  }
  if (stopped) return;
  status(null);
  stage.replaceChildren();
  term.open(stage);

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
  // The daemon's pty must match xterm's geometry. libvirt opens the LXC
  // console as a pty at 0x0 and never sizes it, so the shell's line editor
  // redraws the same bottom line instead of scrolling and long lines wrap
  // nowhere near the browser. Report every change; the daemon writes it with
  // TIOCSWINSZ. `lastSize` starts impossible, so the first fit is always sent
  // even when it equals xterm's default 80x24 -- otherwise the pty stays 0x0.
  let lastSize: [number, number] | null = null;
  const reportSize = (cols: number, rows: number) => {
    void post(`${base}/console/resize`, { cols, rows }).catch(() => undefined);
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
  const ro = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(fit);
  ro?.observe(stage);
  fit();
  stage.onclick = () => term.focus();

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
      await post(`${base}/console/write`, { data: b64.encode(buf) });
    } catch (e) {
      status((e as Error).message);
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
      const r = await fetch(`${base}/console/read?after=${offset}`, { credentials: "same-origin" });
      const data = await r.json().catch(() => ({}));
      if (stopped) return;
      if (!r.ok) throw new Error((data as { error?: string }).error ?? r.statusText);
      const d = data as { data: string; offset: number; closed: boolean; error: string | null; lost: boolean };
      if (d.data) term.write(b64.decode(d.data));
      offset = d.offset;
      if (d.closed) {
        status(d.error ?? "The console closed. Reopen the tab to reconnect.");
        return;
      }
    } catch (e) {
      if (stopped) return;
      status((e as Error).message);
      return;
    }
  }
}

/// Close the terminal, if one is open. Called when the view is left.
export function detachTextConsole() {
  current?.stop();
  current = null;
}
