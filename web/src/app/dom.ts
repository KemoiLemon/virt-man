// DOM primitives every view uses: the one HTML injection point, the page and
// tab slide animations, the `$` lookup and the theme toggle. Importing this
// module also installs the tab-slide click handler.
import DOMPurify from "dompurify";
import { enhanceSelects } from "../listbox";
import { morphChildren } from "../morph";
import { state } from "./state";

export const $ = (sel: string, root: ParentNode = document): any =>
  root.querySelector(sel);

/// Run a CSS animation class on `el` from the start: take the class off, force
/// a reflow so the browser forgets it, put it back, and remove it when the
/// animation is over so it can be replayed.
const replayTimer = new WeakMap<Element, Map<string, number>>();
export function replay(el: Element, cls: string, ms: number) {
  el.classList.remove(cls);
  void (el as HTMLElement).offsetWidth;
  el.classList.add(cls);
  // One removal timer per element and class: replaying quickly must cancel the
  // last one, or it strips the class from the new run and cuts it short.
  const timers = replayTimer.get(el) ?? new Map<string, number>();
  replayTimer.set(el, timers);
  window.clearTimeout(timers.get(cls));
  timers.set(cls, window.setTimeout(() => el.classList.remove(cls), ms));
}

/// Choosing a different sidebar row slides the page in from the direction of
/// travel: a row further down brings the page up from below, a row further up
/// brings it down from above, after the old page has slid out. Keyed on what is selected, not its position, so a
/// guest appearing above the selection (the poll) does not look like a move.
let lastSel: { key: string; idx: number; admin: boolean } | null = null;
export function noteTreeSel(tree: Element, admin: boolean, content: HTMLElement | null = document.getElementById("content")) {
  const sel = tree.querySelector<HTMLElement>(".tree-row.sel");
  if (!sel) return;
  const idx = [...tree.querySelectorAll(".tree-row")].indexOf(sel);
  const key = JSON.stringify({ ...sel.dataset });
  const prev = lastSel;
  lastSel = { key, idx, admin };
  if (!prev || !content || prev.admin !== admin || prev.key === key) return;
  const down = idx > prev.idx;
  // The page being left slides out the other way first: a copy of it laid over
  // the pane, since the real node is about to be repainted. It gets the real
  // pane's scrollbar and scroll offset so it lines up exactly.
  // A move still playing is left to finish: restarting it, or cutting its exit
  // copy short, snaps the page back to the start. The new page just takes the
  // place of the old one inside the motion already under way.
  if (content.classList.contains("nav-up") || content.classList.contains("nav-down")) return;
  const r = content.getBoundingClientRect();
  const ghost = content.cloneNode(true) as HTMLElement;
  // Descendant ids stay: some layout rules key on them (`#admin-area > .grid`
  // adds a 16px top margin), and a copy without them jumps up by that much the
  // moment it appears. The copy is appended after the real page, so an id
  // lookup still finds the real element first.
  ghost.removeAttribute("id");
  ghost.classList.remove("nav-up", "nav-down", "swap-left", "swap-right", "sk-reveal");
  ghost.classList.add("nav-ghost", down ? "out-up" : "out-down");
  Object.assign(ghost.style, { left: `${r.left}px`, top: `${r.top}px`, width: `${r.width}px`, height: `${r.height}px` });
  ghost.style.overflowY = getComputedStyle(content).overflowY;
  document.body.appendChild(ghost);
  ghost.scrollTop = content.scrollTop;
  window.setTimeout(() => ghost.remove(), 200);
  content.classList.remove("nav-up", "nav-down");
  replay(content, down ? "nav-down" : "nav-up", 500);
}

/// How long the old page takes to leave; the new one's entrance waits for it.
export const EXIT_MS = 180;

/// The old tab's page slides out the opposite way before the new one comes in
/// (the same two beats as a sidebar move): a copy of the pane laid over it and
/// clipped to what is under the tab bar. The bar is only ever drawn once, the
/// real one: the copy's own bar is made invisible, since a second 1px border
/// riding the copy's edge is what read as the tab line moving twice.
export function slideOutBelow(bar: HTMLElement, out: "out-left" | "out-right") {
  const box = bar.closest<HTMLElement>("#content, .admin-main");
  // A slide still running is left to finish: a copy of it would replay its
  // entrance from the first frame on top of the real one.
  if (!box || document.querySelector(".nav-ghost") || box.querySelector(".tab-in-right, .tab-in-left, .sk-reveal")) return;
  const r = box.getBoundingClientRect();
  const ghost = box.cloneNode(true) as HTMLElement;
  ghost.removeAttribute("id");
  ghost.classList.remove("nav-up", "nav-down", "swap-left", "swap-right", "sk-reveal");
  ghost.classList.add("nav-ghost", out);
  ghost.querySelectorAll<HTMLElement>(".tabbar").forEach((b) => (b.style.visibility = "hidden"));
  Object.assign(ghost.style, {
    left: `${r.left}px`, top: `${r.top}px`, width: `${r.width}px`, height: `${r.height}px`,
    clipPath: `inset(${Math.ceil(bar.getBoundingClientRect().bottom - r.top) + 3}px 0 0 0)`,
  });
  ghost.style.overflowY = getComputedStyle(box).overflowY;
  // Inside the admin window, or the window (above the page layer) would hide it.
  (box.closest(".modal-back") ?? document.body).appendChild(ghost);
  ghost.scrollTop = box.scrollTop;
  window.setTimeout(() => ghost.remove(), EXIT_MS + 20);
}

/// Switching tabs slides the new page in sideways, from the side of the tab
/// that was chosen. Only what is under the tab bar moves, and only what is new:
/// the observer starts before the tab's own handler runs and slides the nodes
/// it paints (or, for a body that loads, the nodes that arrive in it). Sliding
/// what was already there would fade out the old page just before it is
/// replaced.
document.addEventListener("click", (e) => {
  const tab = (e.target as Element | null)?.closest?.("#content .tabbar .tab, .admin-main .tabbar .tab") as HTMLElement | null;
  const bar = tab?.closest<HTMLElement>(".tabbar");
  if (!tab || !bar || tab.classList.contains("active")) return;
  const tabs = [...bar.querySelectorAll(".tab")];
  const from = tabs.findIndex((t) => t.classList.contains("active"));
  if (from < 0) return;
  const cls = tabs.indexOf(tab) > from ? "tab-in-right" : "tab-in-left";
  slideOutBelow(bar, cls === "tab-in-right" ? "out-left" : "out-right");
  // The entrance waits for the exit, counted from the click, not from whenever
  // the data lands: content that arrives late has nothing left to wait for.
  const clickedAt = Date.now();
  const wait = () => Math.max(0, EXIT_MS - (Date.now() - clickedAt));
  // The admin pane is patched in place, so no new nodes arrive for the observer
  // below to slide; renderAdmin slides what is under the bar once it has painted.
  if (tab.closest(".admin-main")) {
    state.adminTabDir = cls === "tab-in-right" ? 1 : -1;
    state.adminTabAt = clickedAt;
    return;
  }
  const host = bar.parentElement!;
  const slide = (n: Element) => {
    // Already sliding, itself or through a parent: let it finish rather than
    // restart it or stack a second slide on the same motion.
    if (n.closest(".tab-in-right, .tab-in-left")) return;
    // The skeleton's own rise-in (.sk-reveal) would take over the moment this
    // slide ends and replay as a second animation, so the slide replaces it.
    n.classList.remove("tab-in-right", "tab-in-left", "sk-reveal");
    n.parentElement?.classList.remove("sk-reveal");
    n.querySelectorAll(".sk-reveal").forEach((r) => r.classList.remove("sk-reveal"));
    (n as HTMLElement).style.animationDelay = `${wait()}ms`;
    replay(n, cls, 700);
    window.setTimeout(() => ((n as HTMLElement).style.animationDelay = ""), 800);
  };
  const obs = new MutationObserver((muts) => {
    const nb = bar.isConnected ? bar : host.isConnected ? host.querySelector(":scope > .tabbar") : bar.closest("#content, .admin-main")?.querySelector(".tabbar") ?? null;
    if (!nb) return;
    const below = new Set<Element>();
    for (let n = nb.nextElementSibling; n; n = n.nextElementSibling) below.add(n);
    let done = false;
    for (const x of muts.flatMap((m) => [...m.addedNodes])) {
      if (!(x instanceof HTMLElement) || x.classList.contains("skeleton")) continue;
      // A new page section slides if it has content; an empty one is a body
      // still loading, and what arrives in it is what slides.
      if (below.has(x) ? (x.childElementCount > 0 || !!x.textContent?.trim()) && !x.querySelector(":scope > .skeleton") : x.parentElement && below.has(x.parentElement)) {
        slide(x);
        done = true;
      }
    }
    if (done) obs.disconnect();
  });
  obs.observe(host.isConnected ? host : (document.querySelector(".admin-main") ?? document.getElementById("content"))!, { childList: true, subtree: true });
  window.setTimeout(() => obs.disconnect(), 3000);
}, true);

/// The path each container was last painted at; see `setHtml`.
const painted = new WeakMap<Element, string>();

/// Single HTML injection point. Every template's dynamic parts are escaped via
/// esc() at the call site; DOMPurify is the sanitizing backstop the XSS rule
/// requires.
export function setHtml(el: Element, html: string) {
  // A poll repaints the whole pane, so a table that is its own horizontal
  // scroller would jump back to column 1 every second. Keep each table's
  // offset across a repaint of the same view. Only the content pane consumes
  // the navigation reset, so a dialog repaint cannot swallow it mid-navigation.
  const newView = state.tableScrollReset && (el as HTMLElement).id === "content";
  if (newView) state.tableScrollReset = false;
  const scrollLeft = newView
    ? []
    : [...el.querySelectorAll("table.grid")].map((t) => t.scrollLeft);
  const hadSkeleton = !!el.querySelector(".skeleton");
  const clean = DOMPurify.sanitize(html);
  // The same view painted again (the poll, an edit) is patched in place, so
  // what the browser is doing with the nodes survives; a different view
  // (another path) starts from scratch, since nothing in it corresponds.
  // pi-lens-ignore: no-inner-html, no-inner-html-js
  if (el.firstChild && painted.get(el) === location.pathname) {
    const next = el.cloneNode(false) as Element;
    // pi-lens-ignore: no-inner-html, no-inner-html-js
    next.innerHTML = clean;
    morphChildren(el, next);
  } else {
    // pi-lens-ignore: no-inner-html, no-inner-html-js
    el.innerHTML = clean;
  }
  painted.set(el, location.pathname);
  // Not inside a tab slide: both move the same page, and the rise would start
  // over the moment the slide's class is taken off.
  if (hadSkeleton && !el.querySelector(".skeleton") && !el.closest(".tab-in-right, .tab-in-left")) replay(el, "sk-reveal", 900);
  [...el.querySelectorAll("table.grid")].forEach((t, i) => {
    if (scrollLeft[i]) t.scrollLeft = scrollLeft[i];
  });
  // Every render path funnels through here, so this is the one place that has
  // to swap native <select> elements for the themed listbox.
  enhanceSelects(el);
  // iOS capitalises the first letter of a field and autocorrects as you type,
  // which quietly mangles hostnames, tokens, IP addresses and pool names. Done
  // here, after sanitising, because this is the single point every field in the
  // app is rendered through -- login, setup, Add Server and every modal and
  // form included. `autocomplete` is deliberately left alone, so password
  // managers and the login flow keep working.
  for (const f of el.querySelectorAll("input, textarea")) {
    f.setAttribute("autocapitalize", "off");
    f.setAttribute("autocorrect", "off");
    f.setAttribute("spellcheck", "false");
  }
}

export function initTheme() {
  const t = localStorage.getItem("virt-man-theme") ?? "dark";
  document.documentElement.setAttribute("data-theme", t);
}
export function toggleTheme() {
  const next = document.documentElement.getAttribute("data-theme") === "dark" ? "light" : "dark";
  document.documentElement.setAttribute("data-theme", next);
  localStorage.setItem("virt-man-theme", next);
}
