// Themed dropdowns, ported from vm-man (ui/app-dialogs.js).
//
// The native <select> stays in the DOM as the form-data source (hidden by
// CSS), and a .select-trigger button plus a shared .select-popover present it.
// Because the select keeps owning the value, every existing
// `(el as HTMLSelectElement).value` read/write in the app works unchanged.

import { chevronSvg } from "./icons";

interface Enhanced extends HTMLSelectElement {
  _trigger?: HTMLButtonElement;
}

let popover: HTMLDivElement | null = null;
let openTrigger: HTMLButtonElement | null = null;

function getPopover(): HTMLDivElement {
  if (!popover) {
    popover = document.createElement("div");
    popover.className = "select-popover";
    popover.hidden = true;
    popover.setAttribute("role", "listbox");
  }
  // Re-attach when a full re-render replaced document.body's children and
  // detached the cached node; otherwise every dropdown silently stops opening.
  if (!popover.isConnected) document.body.appendChild(popover);
  return popover;
}

function optionLabel(select: HTMLSelectElement, value: string): string {
  const o = Array.from(select.options).find((x) => x.value === value);
  return o ? o.textContent ?? value : value;
}

/** Refresh a trigger's label/disabled state from its native select. */
export function syncSelect(select: HTMLSelectElement) {
  const t = (select as Enhanced)._trigger;
  if (!t) return;
  const label = t.querySelector(".select-value");
  if (label) label.textContent = optionLabel(select, select.value);
  t.disabled = select.disabled;
}

/** Re-sync every enhanced select under `root` (after options are replaced). */
export function syncSelects(root: ParentNode = document) {
  root.querySelectorAll("select").forEach((s) => syncSelect(s as HTMLSelectElement));
}

export function closeSelect() {
  const p = getPopover();
  p.hidden = true;
  p.textContent = "";
  openTrigger?.classList.remove("open");
  openTrigger = null;
}

function openSelect(select: HTMLSelectElement, trigger: HTMLButtonElement) {
  if (openTrigger === trigger) {
    closeSelect();
    return;
  }
  closeSelect();

  const p = getPopover();
  const r = trigger.getBoundingClientRect();
  p.style.left = `${Math.max(8, Math.min(r.left, window.innerWidth - 200))}px`;
  p.style.top = `${Math.min(r.bottom + 3, window.innerHeight - 50)}px`;
  p.style.minWidth = `${r.width}px`;

  for (const opt of Array.from(select.options)) {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "select-option" + (opt.value === select.value ? " active" : "");
    b.textContent = opt.textContent;
    b.disabled = opt.disabled;
    b.setAttribute("role", "option");
    b.setAttribute("aria-selected", opt.value === select.value ? "true" : "false");
    b.onclick = () => {
      select.value = opt.value;
      // Bubbling change keeps existing onchange handlers working.
      select.dispatchEvent(new Event("change", { bubbles: true }));
      syncSelect(select);
      closeSelect();
      trigger.focus();
    };
    p.appendChild(b);
  }

  p.hidden = false;
  trigger.classList.add("open");
  openTrigger = trigger;

  // Keyboard: move through options, Enter commits, Escape closes.
  const opts = Array.from(p.querySelectorAll<HTMLButtonElement>(".select-option:not(:disabled)"));
  const active = p.querySelector<HTMLButtonElement>(".select-option.active");
  (active ?? opts[0])?.focus();
  p.onkeydown = (e: KeyboardEvent) => {
    const i = opts.indexOf(document.activeElement as HTMLButtonElement);
    if (e.key === "ArrowDown") {
      e.preventDefault();
      opts[Math.min(i + 1, opts.length - 1)]?.focus();
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      opts[Math.max(i - 1, 0)]?.focus();
    } else if (e.key === "Home") {
      e.preventDefault();
      opts[0]?.focus();
    } else if (e.key === "End") {
      e.preventDefault();
      opts[opts.length - 1]?.focus();
    } else if (e.key === "Escape") {
      e.preventDefault();
      closeSelect();
      trigger.focus();
    }
  };
}

/** Build the themed trigger for one native select (idempotent). */
function enhance(select: Enhanced) {
  if (select._trigger?.isConnected) {
    syncSelect(select);
    return;
  }
  const trigger = document.createElement("button");
  trigger.type = "button";
  trigger.className = "select-trigger";
  trigger.setAttribute("aria-haspopup", "listbox");
  // Built from DOM nodes rather than innerHTML: the caret SVG is static, and
  // the label is assigned via textContent in syncSelect.
  const value = document.createElement("span");
  value.className = "select-value";
  const caret = document.createElement("span");
  caret.className = "select-caret ico";
  caret.append(chevronSvg());
  trigger.append(value, caret);
  select.insertAdjacentElement("afterend", trigger);
  select._trigger = trigger;
  syncSelect(select);

  trigger.onclick = (e) => {
    e.stopPropagation();
    openSelect(select, trigger);
  };
  trigger.onkeydown = (e: KeyboardEvent) => {
    if (e.key === "ArrowDown" || e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      openSelect(select, trigger);
    }
  };
  select.addEventListener("change", () => syncSelect(select));
}

/** Enhance every native select under `root`. Safe to call after re-renders.
 *  When `root` IS a select (its options were just replaced), re-sync it too —
 *  querySelectorAll only matches descendants, so it would otherwise keep a
 *  stale trigger label. */
export function enhanceSelects(root: ParentNode = document) {
  if ((root as Element).tagName === "SELECT") {
    enhance(root as Enhanced);
    syncSelect(root as HTMLSelectElement);
  }
  root.querySelectorAll("select").forEach((s) => enhance(s as Enhanced));
}

// Global dismiss, matching vm-man.
document.addEventListener("click", (e) => {
  if (!(e.target as Element).closest?.(".select-trigger, .select-popover")) closeSelect();
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") closeSelect();
});
// A scrolling pane would leave the fixed popover stranded.
window.addEventListener("resize", closeSelect);
document.addEventListener("scroll", closeSelect, true);
