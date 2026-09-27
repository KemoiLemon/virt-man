// Tayne: the Ctrl+K command palette.
//
// One overlay reused for every invocation. Callers hand over the commands
// that make sense in the current context; the palette filters them as you
// type and runs the highlighted one on Enter. Arrows and Tab move, Escape
// closes, and a click runs — all built from DOM nodes so nothing passes
// through innerHTML and no sanitisation is needed.

export interface TayneCommand {
  id: string;
  label: string;
  /** Secondary text on the right, e.g. the owning server's name. */
  hint?: string;
  /** Icon markup from `ic` (a trusted static SVG string from icons.ts). */
  icon?: string;
  run: () => void;
}

const S = {
  back: null as HTMLDivElement | null,
  input: null as HTMLInputElement | null,
  list: null as HTMLDivElement | null,
  commands: [] as TayneCommand[],
  matches: [] as TayneCommand[],
  selected: 0,
  open: false,
};

function ensure() {
  if (S.back?.isConnected) return;

  const back = document.createElement("div");
  back.className = "tayne-back";

  const box = document.createElement("div");
  box.className = "tayne";

  const input = document.createElement("input");
  input.className = "tayne-input";
  input.type = "text";
  input.placeholder = "Search commands…";
  input.setAttribute("aria-label", "Command palette");
  input.setAttribute("spellcheck", "false");
  input.setAttribute("autocomplete", "off");

  const list = document.createElement("div");
  list.className = "tayne-list";
  list.setAttribute("role", "listbox");

  box.append(input, list);
  back.append(box);
  document.body.append(back);

  back.addEventListener("mousedown", (e) => {
    if (e.target === back) closeTayne();
  });
  input.addEventListener("input", render);
  input.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      move(1);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      move(-1);
    } else if (e.key === "Enter") {
      e.preventDefault();
      runSelected();
    } else if (e.key === "Escape") {
      e.preventDefault();
      closeTayne();
    } else if (e.key === "Tab") {
      e.preventDefault();
      move(e.shiftKey ? -1 : 1);
    }
  });

  S.back = back;
  S.input = input;
  S.list = list;
}

/// The row icon, built without innerHTML: only the inner <svg> is imported,
/// because `.ico > svg` is a direct-child rule (same as the context menu).
function iconEl(markup: string): HTMLSpanElement {
  const wrap = document.createElement("span");
  wrap.className = "ico tayne-ic";
  const doc = new DOMParser().parseFromString(markup, "image/svg+xml");
  const svg = doc.documentElement?.nodeName.toLowerCase() === "svg"
    ? doc.documentElement
    : doc.querySelector("svg");
  if (svg) wrap.append(document.importNode(svg, true));
  return wrap;
}

function highlight() {
  S.list?.querySelectorAll<HTMLButtonElement>(".tayne-row").forEach((r, i) => {
    r.classList.toggle("sel", i === S.selected);
  });
}

function render() {
  const q = (S.input?.value ?? "").trim().toLowerCase();
  S.matches = S.commands.filter((c) =>
    !q || `${c.label} ${c.hint ?? ""} ${c.id}`.toLowerCase().includes(q),
  );
  S.selected = Math.min(Math.max(S.selected, 0), Math.max(S.matches.length - 1, 0));

  if (!S.list) return;
  const list = S.list;
  list.textContent = "";

  S.matches.forEach((c, i) => {
    const row = document.createElement("button");
    row.type = "button";
    row.className = "tayne-row" + (i === S.selected ? " sel" : "");
    row.setAttribute("role", "option");

    if (c.icon) row.append(iconEl(c.icon));

    const label = document.createElement("span");
    label.className = "tayne-label";
    label.textContent = c.label;
    row.append(label);

    if (c.hint) {
      const hint = document.createElement("span");
      hint.className = "tayne-hint";
      hint.textContent = c.hint;
      row.append(hint);
    }

    // Hover tracks the selection so Enter runs what is under the cursor.
    row.addEventListener("mousemove", () => {
      S.selected = i;
      highlight();
    });
    row.addEventListener("click", () => {
      closeTayne();
      c.run();
    });
    list.append(row);
  });

  if (!S.matches.length) {
    const empty = document.createElement("div");
    empty.className = "tayne-empty";
    empty.textContent = "No matching commands";
    list.append(empty);
  }
}

function move(delta: number) {
  const n = S.matches.length;
  if (!n) return;
  S.selected = (S.selected + delta + n) % n;
  highlight();
  const row = S.list?.querySelectorAll<HTMLButtonElement>(".tayne-row")[S.selected];
  // Keep the selected row in view. Cosmetic only: jsdom (the test runtime)
  // has no scroll implementation, so tolerate its absence.
  try {
    row?.scrollIntoView({ block: "nearest" });
  } catch {
    /* ignore */
  }
}

function runSelected() {
  const cmd = S.matches[S.selected];
  if (!cmd) return;
  closeTayne();
  cmd.run();
}

export function openTayne(commands: TayneCommand[]) {
  ensure();
  S.commands = commands;
  S.matches = commands;
  S.selected = 0;
  if (S.input) S.input.value = "";
  S.back?.classList.add("open");
  S.open = true;
  render();
  S.input?.focus();
}

export function closeTayne(): boolean {
  if (!S.open) return false;
  S.open = false;
  S.back?.classList.remove("open");
  return true;
}

export function tayneOpen(): boolean {
  return S.open;
}
