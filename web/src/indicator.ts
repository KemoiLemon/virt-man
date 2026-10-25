// Selection indicators that slide: the bar beside the selected sidebar row and
// the line under the active tab.
//
// Both lists are rebuilt from a string on every navigation (and the sidebar on
// every poll), so the selected item is a brand-new element each time and a CSS
// transition has nothing to animate. The bar is therefore its own element, put
// back after each rebuild and animated from the position the last one had to
// the new one: it appears to travel instead of jumping. The position is remembered
// here, outside the DOM that gets thrown away. The move stretches: see `place`.

interface Spot { start: number; size: number }

/// Where each bar was last drawn, by what it belongs to.
const last = new Map<string, Spot>();

/// Ensure `container` holds one bar for `active`, sliding from where the bar
/// for `key` last was. `vertical` bars run down a sidebar; the others run along
/// a tab strip.
function place(container: HTMLElement, active: HTMLElement | null, key: string, vertical: boolean) {
  let bar = container.querySelector<HTMLElement>(":scope > .sel-bar");
  if (!active) {
    bar?.remove();
    last.delete(key);
    return;
  }
  const target: Spot = vertical
    ? { start: active.offsetTop, size: active.offsetHeight }
    : { start: active.offsetLeft, size: active.offsetWidth };
  if (!bar) {
    bar = document.createElement("div");
    bar.className = `sel-bar ${vertical ? "v" : "h"}`;
    bar.setAttribute("aria-hidden", "true");
    container.appendChild(bar);
  }
  const [pos, len] = vertical ? ["top", "height"] as const : ["left", "width"] as const;
  bar.style[pos] = `${target.start}px`;
  bar.style[len] = `${target.size}px`;

  const from = last.get(key);
  last.set(key, target);
  if (!from || (from.start === target.start && from.size === target.size)) return;
  if (typeof matchMedia !== "function" || matchMedia("(prefers-reduced-motion: reduce)").matches) return;
  // Stretch: the edge that leads the move gets there first, so the bar grows
  // to span both places, then the trailing edge catches up and it shrinks to
  // the new item. Keyframes rather than a transition, because a transition
  // moves both edges together.
  const fromEnd = from.start + from.size;
  const toEnd = target.start + target.size;
  const down = target.start >= from.start;
  const mid = down
    ? { start: from.start, size: toEnd - from.start }
    : { start: target.start, size: fromEnd - target.start };
  bar.animate?.(
    {
      [pos]: [`${from.start}px`, `${mid.start}px`, `${target.start}px`],
      [len]: [`${from.size}px`, `${mid.size}px`, `${target.size}px`],
      offset: [0, 0.4, 1],
    },
    { duration: 200, easing: "ease-in-out" },
  );
}

/// A tab strip is identified by its labels, so moving between two different
/// strips (a VM's tabs, then a network's) does not slide from the wrong place.
function tabKey(bar: HTMLElement): string {
  return "tabs:" + [...bar.querySelectorAll(":scope > .tab")]
    .map((t) => t.textContent?.trim()).join("|");
}

export function syncIndicators() {
  const tree = document.getElementById("tree");
  if (tree) place(tree, tree.querySelector<HTMLElement>(".tree-row.sel"), "tree", true);
  document.querySelectorAll<HTMLElement>(".tabbar").forEach((bar) => {
    // An exit copy (.nav-ghost) holds the old page's bar, with the same labels
    // and so the same key as the live one. Tracking it moved the remembered
    // position back to the old tab, and the live line then replayed its slide.
    if (bar.closest(".nav-ghost")) return;
    place(bar, bar.querySelector<HTMLElement>(":scope > .tab.active"), tabKey(bar), false);
  });
}

/// Keep the bars in step with whatever the app renders. Cheap on purpose: the
/// poll rewrites the tree every second, and a frame's worth of mutations is
/// handled by one pass.
export function initIndicators() {
  let queued = false;
  const run = () => {
    if (queued) return;
    queued = true;
    requestAnimationFrame(() => { queued = false; syncIndicators(); });
  };
  new MutationObserver(run).observe(document.body, { childList: true, subtree: true });
  window.addEventListener("resize", run);
  run();
}
