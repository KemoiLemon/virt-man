// Keyboard focus: Tab / Shift+Tab stay inside whatever layer is on top.
//
// The browser only knows about the page. Without this, Tab walks out of an
// open dialog into the sidebar behind it, and closing the dialog drops focus
// on <body> so the next Tab starts again from the top of the page. One
// document-level handler covers every layer — modals, the context menu and the
// console — so no dialog needs focus code of its own.

const FOCUSABLE =
  'a[href], button, input, select, textarea, [tabindex], [contenteditable="true"]';

/// Focusable, visible elements inside `root`, in tab order.
export function focusables(root: ParentNode): HTMLElement[] {
  return [...root.querySelectorAll<HTMLElement>(FOCUSABLE)].filter((el) => {
    if ((el as HTMLButtonElement).disabled) return false;
    if (el.tabIndex < 0) return false;
    if (el.closest("[hidden], [inert]")) return false;
    // A <select> is hidden behind its themed trigger; the trigger takes focus.
    if (el.tagName === "SELECT" && getComputedStyle(el).display === "none") return false;
    return true;
  });
}

/// Next element to focus when Tab (or Shift+Tab) is pressed inside `root`, or
/// null to let the browser move normally. Wraps at both ends, and pulls focus
/// back in when it has escaped the layer.
export function trapTarget(root: ParentNode, active: Element | null, back: boolean): HTMLElement | null {
  const items = focusables(root);
  if (!items.length) return null;
  const first = items[0];
  const last = items[items.length - 1];
  const inside = !!active && (root as Node).contains(active);
  if (!inside) return back ? last : first;
  if (back && active === first) return last;
  if (!back && active === last) return first;
  return null;
}

/// The layer that currently owns the keyboard: the context menu, else the
/// topmost modal, else nothing (the page itself).
export function topLayer(): HTMLElement | null {
  const menu = document.querySelector<HTMLElement>(".ctxmenu");
  if (menu) return menu;
  const modals = document.querySelectorAll<HTMLElement>(".modal-back.open");
  return modals[modals.length - 1] ?? null;
}

/// Focus the first field of a freshly opened layer (or its first control),
/// and give focus back to `opener` when the layer closes. The caller captures
/// `opener` when the layer is created: by the time this runs, a dialog may
/// already have focused its own field.
export function enterLayer(layer: HTMLElement, opener: HTMLElement | null) {
  const body = layer.querySelector(".modal-body") ?? layer;
  const target =
    focusables(body).find((el) => el.matches("input, textarea, .select-trigger")) ??
    focusables(layer).find((el) => !el.classList.contains("x")) ??
    focusables(layer)[0];
  // A dialog that already placed focus inside itself (askText selects its
  // text) knows better; only step in when focus is still outside.
  if (!layer.contains(document.activeElement)) target?.focus();
  // Restore on close. A layer is closed by removing it, so watch its parent.
  const parent = layer.parentNode;
  if (!parent || !opener || opener === document.body) return;
  const watch = new MutationObserver(() => {
    if (layer.isConnected) return;
    watch.disconnect();
    // Only if focus went nowhere useful; never steal it from a newer layer.
    const now = document.activeElement;
    if ((!now || now === document.body) && opener.isConnected) opener.focus();
  });
  watch.observe(parent, { childList: true });
}

document.addEventListener("keydown", (e) => {
  if (e.key !== "Tab" || e.ctrlKey || e.altKey || e.metaKey) return;
  // A widget that handles Tab itself (the Ctrl+K palette moves its selection)
  // has already claimed the key.
  if (e.defaultPrevented) return;
  // The guest owns the keyboard while its console is focused; Tab belongs to it.
  if ((document.activeElement as Element | null)?.closest?.(".console-stage")) return;
  const layer = topLayer();
  if (!layer) return;
  const next = trapTarget(layer, document.activeElement, e.shiftKey);
  if (next) {
    e.preventDefault();
    next.focus();
  }
});

// Clickable cards and rows that are not buttons (dashboard pool cards, network
// rows, guest and server cards) open on Enter or Space like a button would.
// They get tabindex=0 where they are rendered; this makes the key do the click.
document.addEventListener("keydown", (e) => {
  if (e.key !== "Enter" && e.key !== " ") return;
  const el = e.target as HTMLElement | null;
  if (!el?.matches?.("[data-open], tr.row-open, [data-dc-dash-conn], .admin-card-link")) return;
  e.preventDefault();
  el.click();
});

// ---------------------------------------------------------------- arrow keys
//
// Tab moves between groups; arrows move within one. Each group below is a list
// of like items: the right-click menu, the sidebar tree, a tab bar, the
// hardware list, a grid of resource cards. Up/Down (and Left/Right on a
// horizontal tab bar) step through it, Home/End jump to the ends, and it wraps.
// Fields keep their arrows: a caret in an input, a value in a <select>, and the
// guest console, which owns every key.

interface ArrowGroup {
  /** Container holding the group's items. */
  root: string;
  /** The items, relative to the root. */
  item: string;
  /** Which arrows step: vertical lists use Up/Down, tab bars Left/Right. */
  axis: "v" | "h" | "both";
}

export const ARROW_GROUPS: ArrowGroup[] = [
  { root: ".ctxmenu", item: ".ctxmenu-item", axis: "v" },
  { root: "#tree", item: ".tree-row", axis: "v" },
  { root: ".tabbar", item: ".tab", axis: "both" },
  { root: ".hw-side", item: ".hw-item", axis: "v" },
  { root: ".card-grid", item: "[data-open]", axis: "both" },
  { root: ".create-resource-grid", item: ".create-resource-card", axis: "both" },
];

const STEP: Record<string, { axis: "v" | "h"; d: number }> = {
  ArrowDown: { axis: "v", d: 1 },
  ArrowUp: { axis: "v", d: -1 },
  ArrowRight: { axis: "h", d: 1 },
  ArrowLeft: { axis: "h", d: -1 },
};

/// The element to focus for an arrow/Home/End key pressed on `active`, or null
/// when the key is not ours (not in a group, or the wrong axis for it).
export function arrowTarget(active: Element | null, key: string): HTMLElement | null {
  if (!active) return null;
  for (const g of ARROW_GROUPS) {
    const root = active.closest(g.root);
    if (!root || !active.matches(g.item)) continue;
    const items = [...root.querySelectorAll<HTMLElement>(g.item)].filter(
      (el) => !(el as HTMLButtonElement).disabled && !el.closest("[hidden]"),
    );
    const i = items.indexOf(active as HTMLElement);
    if (i < 0 || !items.length) return null;
    if (key === "Home") return items[0];
    if (key === "End") return items[items.length - 1];
    const step = STEP[key];
    if (!step || (g.axis !== "both" && g.axis !== step.axis)) return null;
    return items[(i + step.d + items.length) % items.length];
  }
  return null;
}

document.addEventListener("keydown", (e) => {
  if (e.defaultPrevented || e.ctrlKey || e.altKey || e.metaKey || e.shiftKey) return;
  if (!(e.key in STEP) && e.key !== "Home" && e.key !== "End") return;
  const active = document.activeElement;
  if (active?.closest?.("input, textarea, select, .console-stage, [contenteditable='true']")) return;
  const next = arrowTarget(active, e.key);
  if (next) {
    e.preventDefault();
    next.focus();
  }
});

/// Keep keyboard focus on a list that is about to re-render. Selecting a tree
/// row or a tab rebuilds that list, which destroys the focused element and
/// drops focus on <body> -- so the next arrow press did nothing. Call before
/// the rebuild; the returned function, called after it, moves focus to the
/// list's new selected item. It does nothing unless focus was inside the list,
/// so a poll-driven re-render never steals focus from somewhere else.
export function holdFocus(root: Element | null, selected: string): () => void {
  if (!root || !root.contains(document.activeElement)) return () => {};
  return () => {
    const now = document.activeElement;
    if (now && now !== document.body && now.isConnected) return;
    root.querySelector<HTMLElement>(selected)?.focus();
  };
}
