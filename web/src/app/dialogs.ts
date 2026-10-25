// Toasts and the themed replacements for the browser's modal dialogs. No
// native prompt/confirm/alert is ever used (see AGENTS.md "No native OS chrome").
import { $, setHtml } from "./dom";
import { enterLayer } from "../focus";
import { closeSvg, ic } from "../icons";
import { esc } from "../xml";

// Toasts stack bottom-right in a column instead of overlapping at the
// bottom-centre, and each one is dismissible.
export function toast(msg: string, kind: "ok" | "bad" = "ok") {
  let dock = document.getElementById("toast-dock");
  if (!dock) {
    dock = document.createElement("div");
    dock.id = "toast-dock";
    document.body.appendChild(dock);
  }
  const el = document.createElement("div");
  el.className = `toast ${kind}`;
  const text = document.createElement("span");
  text.className = "toast-msg";
  text.textContent = msg;
  const close = document.createElement("button");
  close.className = "toast-close";
  close.type = "button";
  close.setAttribute("aria-label", "Dismiss");
  close.append(closeSvg());
  el.append(text, close);
  dock.appendChild(el);
  const dismiss = () => {
    el.classList.remove("show");
    setTimeout(() => el.remove(), 200);
  };
  close.onclick = dismiss;
  requestAnimationFrame(() => el.classList.add("show"));
  // A failure stays until dismissed: it carries the reason (which limit, which
  // permission), and four seconds is not long enough to read one -- the
  // refusals were being missed entirely. Successes still fade on their own.
  if (kind !== "bad") setTimeout(dismiss, 4000);
}

export function openModal(title: string, body: string, foot: string) {
  const el = document.createElement("div");
  el.className = "modal-back open";
  setHtml(el, `
    <div class="modal">
      <div class="modal-head"><span>${title}</span><button class="x">${ic.close()}</button></div>
      <div class="modal-body">${body}</div>
      <div class="modal-foot">${foot}</div>
    </div>`);
  document.body.appendChild(el);
  // Every dialog is closed by calling `el.remove()`, so the exit animation
  // lives here instead of at each call site. The dialog is inert while it
  // fades, so it takes no clicks or focus; with no matchMedia (jsdom) or a
  // reduced-motion preference it goes at once.
  const removeNow = el.remove.bind(el);
  let closing = false;
  el.remove = () => {
    if (closing) return;
    const animate = el.isConnected && typeof matchMedia === "function"
      && !matchMedia("(prefers-reduced-motion: reduce)").matches;
    if (!animate) return removeNow();
    closing = true;
    el.inert = true;
    el.classList.add("closing");
    setTimeout(removeNow, 140);
  };
  (el.querySelector(".x") as HTMLElement).onclick = () => el.remove();
  el.onclick = (e) => { if (e.target === el) el.remove(); };
  // The global Escape handler dispatches this; askText/askConfirm add their
  // own listener to resolve their promise, which also removes the element.
  el.addEventListener("modal-escape", () => el.remove());
  // Deferred: callers fill in fields and wire handlers after this returns.
  const opener = document.activeElement as HTMLElement | null;
  queueMicrotask(() => { if (el.isConnected) enterLayer(el, opener); });
  return el;
}

/// Themed replacement for window.prompt — the native dialog ignores the app
/// theme and looks alien inside the frameless window.
export function askText(title: string, label: string, value = "", placeholder = ""): Promise<string | null> {
  return new Promise((resolve) => {
    let done = false;
    const finish = (v: string | null) => {
      if (done) return;
      done = true;
      el.remove();
      resolve(v);
    };
    const el = openModal(
      title,
      `<label class="field"><span>${esc(label)}</span>
         <input id="ask-input" value="${esc(value)}" placeholder="${esc(placeholder)}"></label>`,
      `<button class="btn" id="ask-cancel">Cancel</button>
       <button class="btn primary" id="ask-ok">OK</button>`,
    );
    const input = $("#ask-input", el);
    $("#ask-ok", el).onclick = () => finish(input.value);
    $("#ask-cancel", el).onclick = () => finish(null);
    (el.querySelector(".x") as HTMLElement).onclick = () => finish(null);
    el.onclick = (e: Event) => { if (e.target === el) finish(null); };
    el.addEventListener("modal-escape", () => finish(null));
    input.onkeydown = (e: KeyboardEvent) => {
      if (e.key === "Enter") finish(input.value);
      if (e.key === "Escape") finish(null);
    };
    input.focus();
    input.select();
  });
}

/// A multi-line sibling of askText. Separate rather than a parameter on it:
/// askText's single `<input>` is what makes Enter submit and select-on-open
/// work, and both would have to be undone for a textarea.
export function askMultiline(title: string, label: string, hint: string): Promise<string | null> {
  return new Promise((resolve) => {
    let done = false;
    const finish = (v: string | null) => {
      if (done) return;
      done = true;
      el.remove();
      resolve(v);
    };
    const el = openModal(
      title,
      // The hint is a live description of what the action does with the text,
      // not narration about the field.
      `<label class="field"><span>${esc(label)}</span></label>
       <textarea id="ask-text" rows="8" spellcheck="false" placeholder=""></textarea>
       <div class="muted" style="font-size:11px">${esc(hint)}</div>`,
      `<button class="btn" id="ask-cancel">Cancel</button>
       <button class="btn primary" id="ask-ok">Type It</button>`,
    );
    const ta = $("#ask-text", el) as HTMLTextAreaElement;
    // Ctrl/Cmd+Enter submits; a bare Enter must stay a newline, or a pasted
    // multi-line snippet could never be entered.
    ta.onkeydown = (e: KeyboardEvent) => {
      if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) finish(ta.value);
      if (e.key === "Escape") finish(null);
    };
    $("#ask-ok", el).onclick = () => finish(ta.value);
    $("#ask-cancel", el).onclick = () => finish(null);
    (el.querySelector(".x") as HTMLElement).onclick = () => finish(null);
    el.onclick = (e: Event) => { if (e.target === el) finish(null); };
    el.addEventListener("modal-escape", () => finish(null));
    ta.focus();
  });
}

/// Themed replacement for window.confirm.
export function askConfirm(title: string, message: string, okLabel = "Confirm", danger = false): Promise<boolean> {
  return new Promise((resolve) => {
    let done = false;
    const finish = (v: boolean) => {
      if (done) return;
      done = true;
      el.remove();
      resolve(v);
    };
    const el = openModal(
      title,
      `<p class="hint" style="font-size:12px;color:var(--fg)">${esc(message)}</p>`,
      `<button class="btn" id="ask-no">Cancel</button>
       <button class="btn ${danger ? "danger" : "primary"}" id="ask-yes">${esc(okLabel)}</button>`,
    );
    $("#ask-yes", el).onclick = () => finish(true);
    $("#ask-no", el).onclick = () => finish(false);
    (el.querySelector(".x") as HTMLElement).onclick = () => finish(false);
    el.onclick = (e: Event) => { if (e.target === el) finish(false); };
    el.addEventListener("modal-escape", () => finish(false));
  });
}
