// Themed autocomplete for free-text inputs.
//
// The listbox in `listbox.ts` turns a <select> into a themed dropdown; IP
// fields are plain inputs (any address is valid), so they need a different
// control: type freely, but pick from what the daemon actually knows about.
// Reuses the `.select-popover` / `.select-option` styling and z-index so the
// two dropdowns look and stack identically, including inside modals.
//
// Native `<datalist>` is deliberately not used: its popup is OS chrome and
// ignores the app theme.

let popover: HTMLDivElement | null = null;
let activeInput: HTMLInputElement | null = null;
let matches: string[] = [];
let highlighted = 0;

function getPopover(): HTMLDivElement {
  if (!popover) {
    popover = document.createElement("div");
    popover.className = "select-popover";
    popover.hidden = true;
    popover.setAttribute("role", "listbox");
  }
  // A full re-render replaces body's children and detaches the cached node.
  if (!popover.isConnected) document.body.appendChild(popover);
  return popover;
}

/** Whether a suggestion list is currently showing. */
export function suggestOpen(): boolean {
  return activeInput !== null;
}

export function closeSuggest() {
  if (popover) {
    popover.hidden = true;
    popover.textContent = "";
  }
  activeInput?.classList.remove("suggest-open");
  activeInput = null;
  matches = [];
  highlighted = 0;
}

/** Candidates matching what has been typed: prefix matches first, then any
 *  substring match, so "16" finds 192.168.167.x and 10.0.16.x in that order. */
function filter(list: string[], typed: string): string[] {
  const q = typed.trim();
  if (!q) return list;
  const prefix = list.filter((v) => v.startsWith(q));
  const rest = list.filter((v) => !v.startsWith(q) && v.includes(q));
  return [...prefix, ...rest];
}

function paint() {
  // Resolve through getPopover(): after a full re-render the cached node is
  // detached, and writing to it would paint into nothing.
  const p = getPopover();
  if (!activeInput) return;
  p.textContent = "";
  if (!matches.length) {
    closeSuggest();
    return;
  }
  const r = activeInput.getBoundingClientRect();
  p.style.left = `${Math.max(8, Math.min(r.left, window.innerWidth - 200))}px`;
  p.style.top = `${Math.min(r.bottom + 3, window.innerHeight - 50)}px`;
  p.style.minWidth = `${r.width}px`;

  for (const [i, value] of matches.entries()) {
    const b = document.createElement("button");
    b.type = "button";
    b.className = `select-option${i === highlighted ? " active" : ""}`;
    b.textContent = value;
    b.setAttribute("role", "option");
    b.setAttribute("aria-selected", i === highlighted ? "true" : "false");
    // Keep focus in the input: a blur would close the list before the click
    // lands, and mousedown-preventDefault is what stops that.
    b.onmousedown = (e) => e.preventDefault();
    b.onclick = () => commit(value);
    p.append(b);
  }
  p.hidden = false;
}

function commit(value: string) {
  const input = activeInput;
  if (!input) return;
  input.value = value;
  closeSuggest();
  // Existing handlers read `.value` on save; `change` keeps any listener in
  // step without the control having to know about them.
  input.dispatchEvent(new Event("change", { bubbles: true }));
  // Deliberately no input.focus() here: the focus listener reopens the list,
  // so refocusing would pop the dropdown straight back open on the value the
  // user just chose. The option's mousedown preventDefault already keeps the
  // caret in the input, so focus never left.
}

/**
 * Offer `candidates` while the user types in `input`.
 *
 * `candidates` is a getter because the list is usually fetched after the form
 * is painted; it is consulted on every keystroke, so a late-arriving list
 * starts working with no re-render.
 */
export function attachSuggest(input: HTMLInputElement, candidates: () => string[]) {
  // The browser's own autofill popup would fight with this one.
  input.autocomplete = "off";
  input.spellcheck = false;

  const refresh = () => {
    const list = candidates();
    matches = filter(list, input.value);
    highlighted = 0;
    activeInput = input;
    if (!matches.length) {
      closeSuggest();
      return;
    }
    paint();
  };

  input.addEventListener("focus", refresh);
  input.addEventListener("input", () => {
    // Every keystroke reopens: typing over a committed value should suggest
    // again rather than leaving the list closed.
    activeInput = input;
    refresh();
  });

  input.addEventListener("keydown", (e: KeyboardEvent) => {
    if (!suggestOpen() || activeInput !== input) return;
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!matches.length) return;
      const step = e.key === "ArrowDown" ? 1 : -1;
      highlighted = (highlighted + step + matches.length) % matches.length;
      paint();
    } else if (e.key === "Enter" || e.key === "Tab") {
      // Only swallow Enter/Tab when the user is actively navigating the list,
      // so plain typing and form submission still work.
      if (matches[highlighted] && matches[highlighted] !== input.value) {
        e.preventDefault();
        commit(matches[highlighted]);
      } else {
        closeSuggest();
      }
    } else if (e.key === "Escape") {
      // One level: close the suggestions, and do not let the same press reach
      // the app's Escape handler and dismiss the enclosing modal.
      e.preventDefault();
      e.stopPropagation();
      closeSuggest();
    }
  });

  // Tabbing away leaves the list behind otherwise. The option's mousedown
  // preventDefault means a click never triggers this first.
  input.addEventListener("blur", () => {
    if (activeInput === input) closeSuggest();
  });
}

// Global dismissals, mirroring the listbox. Scrolling a pane strands a fixed
// popover, so close on scroll — but not when scrolling the popover itself.
document.addEventListener("click", (e) => {
  if (!activeInput) return;
  const t = e.target as Element | null;
  if (t === activeInput || t?.closest?.(".select-popover")) return;
  closeSuggest();
});
window.addEventListener("resize", closeSuggest);
document.addEventListener("scroll", (e) => {
  if ((e.target as Element)?.closest?.(".select-popover")) return;
  closeSuggest();
}, true);
