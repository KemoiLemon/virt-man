// Update the DOM in place instead of replacing it.
//
// `setHtml` used to throw a pane's children away and build them again on every
// poll. That cancels what the browser is doing with the old nodes: hover and
// press states, CSS transitions and animations (a status colour could never
// fade, a selection bar had nowhere to slide from), scroll offsets, and focus.
// Morphing walks the old and the new tree together and changes only what
// differs, so a node that is still there is still the same node.
//
// The rest of the app was written for replacement, in two ways this has to
// honour:
//  - It binds handlers after every render. A kept node would end up with each
//    handler twice (`addEventListener`) or with a stale one (`onclick =`), so a
//    kept node has its handlers cleared first. Callers re-bind, as they always
//    did.
//  - It reads form values as the template wrote them. A kept control is reset
//    to the new template's value, as a replaced one would have been, except the
//    one the user is typing in, which keeps its value and its focus.

type Bound = [string, EventListenerOrEventListenerObject, boolean | AddEventListenerOptions | undefined];

const bound = new WeakMap<EventTarget, Bound[]>();
const rawAdd = EventTarget.prototype.addEventListener;
/// Set around an `addEventListener` that must not be tracked.
let untracked = false;

// Remember which listeners were added to elements, so a kept node can be wiped
// of them. Only elements are tracked: document and window listeners are
// app-wide and were never tied to a render.
EventTarget.prototype.addEventListener = function (
  this: EventTarget,
  type: string,
  fn: EventListenerOrEventListenerObject | null,
  opts?: boolean | AddEventListenerOptions,
) {
  if (!untracked && fn && typeof Element !== "undefined" && this instanceof Element) {
    const list = bound.get(this) ?? [];
    list.push([type, fn, opts]);
    bound.set(this, list);
  }
  return rawAdd.call(this, type, fn, opts);
} as typeof rawAdd;

/// A listener that outlives re-renders of the node it sits on (a themed
/// select's own change hook): not tracked, so morphing never strips it.
export function addPermanentListener(t: EventTarget, type: string, fn: EventListener) {
  untracked = true;
  try {
    t.addEventListener(type, fn);
  } finally {
    untracked = false;
  }
}

const HANDLER_PROPS = [
  "onclick", "ondblclick", "onchange", "oninput", "onkeydown", "onkeyup", "onpointerdown",
  "onpointerup", "onpointermove", "oncontextmenu", "onsubmit", "onfocus", "onblur",
  "onmouseenter", "onmouseleave", "onscroll", "onwheel", "ondragstart", "ondrop", "ondragover",
] as const;

function clearHandlers(el: Element) {
  const e = el as unknown as Record<string, unknown>;
  for (const p of HANDLER_PROPS) if (e[p]) e[p] = null;
  const list = bound.get(el);
  if (list) {
    for (const [t, fn, o] of list) el.removeEventListener(t, fn, o);
    bound.delete(el);
  }
}

/// Nodes the app puts into a rendered container itself, which the template
/// knows nothing about: a themed select's button and the sliding selection bar.
function owned(n: Node): boolean {
  return n.nodeType === 1 && (n as Element).matches(".select-trigger, .sel-bar");
}

function dropNode(from: Element, n: Node) {
  const trig = (n as unknown as { _trigger?: Element })._trigger;
  trig?.remove();
  from.removeChild(n);
}

function syncAttrs(o: Element, w: Element) {
  for (const a of [...o.attributes]) if (!w.hasAttribute(a.name)) o.removeAttribute(a.name);
  for (const a of [...w.attributes]) if (o.getAttribute(a.name) !== a.value) o.setAttribute(a.name, a.value);
}

function syncControl(o: Element, w: Element) {
  if (o === document.activeElement) return;
  if (o instanceof HTMLInputElement && w instanceof HTMLInputElement) {
    if (o.checked !== w.checked) o.checked = w.checked;
    if (o.value !== w.value) o.value = w.value;
  } else if (o instanceof HTMLTextAreaElement && w instanceof HTMLTextAreaElement) {
    if (o.value !== w.value) o.value = w.value;
  } else if (o instanceof HTMLSelectElement && w instanceof HTMLSelectElement) {
    if (o.selectedIndex !== w.selectedIndex) o.selectedIndex = w.selectedIndex;
  }
}

function patch(o: Element, w: Element) {
  clearHandlers(o);
  syncAttrs(o, w);
  morphChildren(o, w);
  syncControl(o, w);
}

/// Make `from`'s children match `to`'s, reusing nodes wherever the two trees
/// agree on what is at that position.
export function morphChildren(from: Element, to: Element) {
  const olds = [...from.childNodes].filter((n) => !owned(n));
  const news = [...to.childNodes];
  for (let i = 0; i < news.length; i++) {
    const o = olds[i];
    const w = news[i];
    if (!o) {
      from.appendChild(w);
    } else if (o.nodeType === 3 && w.nodeType === 3) {
      if (o.nodeValue !== w.nodeValue) o.nodeValue = w.nodeValue;
    } else if (
      o.nodeType === 1 && w.nodeType === 1 &&
      (o as Element).namespaceURI === (w as Element).namespaceURI &&
      o.nodeName === w.nodeName
    ) {
      patch(o as Element, w as Element);
    } else {
      const trig = (o as unknown as { _trigger?: Element })._trigger;
      trig?.remove();
      from.replaceChild(w, o);
    }
  }
  for (let i = news.length; i < olds.length; i++) dropNode(from, olds[i]);
}
