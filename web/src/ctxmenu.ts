// Right-click context menu.
//
// The browser's own menu is replaced app-wide: a right-click inside the
// resource tree opens the actions for whatever row was under the cursor.
// Built from DOM nodes (like the themed listbox) so labels are assigned via
// textContent, never interpolated into markup.

import { visualHeight } from "./touch";

export interface CtxItem {
  label: string;
  /** Icon markup from `ic`, already an SVG string. */
  icon?: string;
  /** Rendered in the danger colour (destructive actions). */
  danger?: boolean;
  /** Present but unclickable — e.g. Start on a running guest. */
  disabled?: boolean;
  /** Draw a divider above this item. */
  separator?: boolean;
  run: () => void;
}

let menu: HTMLDivElement | null = null;

/** Whether a context menu is currently visible. */
export function contextMenuOpen(): boolean {
  return menu !== null;
}

/**
 * Turn an icon's SVG markup into a real element without touching innerHTML.
 * `ic.*` returns a wrapper `<span class="ico"><svg>…</svg></span>`; only the
 * inner <svg> is imported, because `.ico > svg` is a direct-child rule that
 * would not match a nested wrapper. The markup is a trusted static string
 * from icons.ts.
 */
function iconElement(markup: string): Element | null {
  const parsed = new DOMParser().parseFromString(markup, "image/svg+xml");
  const root = parsed.documentElement;
  if (!root || root.nodeName === "parsererror") return null;
  const svg = root.nodeName.toLowerCase() === "svg" ? root : root.querySelector("svg");
  if (!svg) return null;
  return document.importNode(svg, true) as Element;
}

export function closeContextMenu() {
  menu?.remove();
  menu = null;
}

/**
 * Open the menu at viewport coordinates. Re-opens close the previous menu, so
 * right-clicking from one row straight to another never stacks menus.
 */
export function openContextMenu(x: number, y: number, items: CtxItem[]) {
  closeContextMenu();
  if (!items.length) return;

  const el = document.createElement("div");
  el.className = "ctxmenu";
  el.setAttribute("role", "menu");

  for (const item of items) {
    if (item.separator) {
      const hr = document.createElement("div");
      hr.className = "ctxmenu-sep";
      el.append(hr);
    }
    const b = document.createElement("button");
    b.type = "button";
    b.className = `ctxmenu-item${item.danger ? " danger" : ""}`;
    b.setAttribute("role", "menuitem");
    if (item.icon) {
      const ico = document.createElement("span");
      ico.className = "ico ctxmenu-ic";
      const svg = iconElement(item.icon);
      if (svg) ico.append(svg);
      b.append(ico);
    }
    const label = document.createElement("span");
    label.className = "ctxmenu-label";
    label.textContent = item.label;
    b.append(label);
    if (item.disabled) {
      b.disabled = true;
    } else {
      b.onclick = () => {
        // Close before running: an action may open a modal, and leaving the
        // menu on top of it would trap the pointer.
        closeContextMenu();
        item.run();
      };
    }
    el.append(b);
  }

  // Measured after insert so the menu can be flipped away from the edges.
  el.style.left = "-9999px";
  el.style.top = "-9999px";
  document.body.append(el);
  menu = el;

  const r = el.getBoundingClientRect();
  const left = Math.max(4, Math.min(x, window.innerWidth - r.width - 4));
  // Bounded by the box that is actually visible, not innerHeight: on a phone
  // the keyboard does not change innerHeight, and the home bar overlays the
  // page rather than shortening it.
  const top = Math.max(4, Math.min(y, visualHeight() - r.height - 4));
  el.style.left = `${left}px`;
  el.style.top = `${top}px`;

  el.querySelector<HTMLButtonElement>(".ctxmenu-item:not(:disabled)")?.focus();
}

// Global dismissals. Escape is handled first by the app's key handler, which
// asks contextMenuOpen() so one press closes only this layer.
document.addEventListener("click", (e) => {
  if (menu && !(e.target as Element)?.closest?.(".ctxmenu")) closeContextMenu();
});
window.addEventListener("resize", closeContextMenu);
window.addEventListener("blur", closeContextMenu);
document.addEventListener("scroll", () => closeContextMenu(), true);
