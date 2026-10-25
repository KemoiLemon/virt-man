import { api } from "./api";
import type { User } from "./api";
import { adminTitle, adminTree, renderAdminSection, type AdminCtx } from "./admin";
import { ic } from "./icons";
import { skeleton } from "./skeleton";
import { holdFocus } from "./focus";
import { ADMIN_TABS, type AdminSection, type AdminTab } from "./route";
import { esc } from "./xml";
import { state } from "./state";
import { relTime } from "./util";
import { askConfirm, openModal, toast } from "./dialogs";
import { adminDeleteResource, deleteConnection } from "./actions";
import { hooks } from "./hooks";
import { $, EXIT_MS, replay, setHtml } from "./dom";
import { openHardwareEditor } from "./hardware-editor";
import { openConnectionDialog } from "./connection-dialog";
import { openCreatePool } from "./create-resource";
import { openCreateNetwork } from "./network-form";
import { canAdmin, loadConnections, renderApp } from "./boot";

// ----------------------------------------------------------- admin window

/// Open the admin window at `section` (or where it was last). It grows out of
/// the admin button over whatever page is open, which stays where it is.
export function openAdmin(section: AdminSection = state.adminSection ?? "dashboard", tab?: AdminTab) {
  if (!canAdmin()) return;
  // Staying in a section keeps its tab; entering one starts at its first.
  if (tab) state.adminTab = tab;
  else if (section !== state.adminSection || !state.adminTab) state.adminTab = ADMIN_TABS[section]?.[0];
  state.adminSection = section;
  if (!state.adminEl) createAdminWindow();
  renderAdminNav();
  renderAdmin();
  syncAdminButton();
}

function createAdminWindow() {
  const el = openModal(
    `<span class="dhead-ic">${ic.admin(18)}</span><span id="admin-title"></span><span class="dhead-actions" id="admin-head"></span>`,
    `<nav class="admin-nav" aria-label="Administration"></nav>
     <div class="detail admin-main"><div id="admin-area"></div></div>`,
    "",
  );
  el.classList.add("admin-back");
  const win = el.querySelector<HTMLElement>(".modal")!;
  win.classList.add("admin-win");
  // Grow from the button. offset* is layout, unaffected by the animation's own
  // scale, which getBoundingClientRect would read.
  const btn = $("#admin-btn");
  if (btn) {
    const b = btn.getBoundingClientRect();
    win.style.transformOrigin = `${b.left + b.width / 2 - win.offsetLeft}px ${b.top + b.height / 2 - win.offsetTop}px`;
    replay(btn, "turn", 450);
  }
  const remove = el.remove;
  el.remove = () => {
    if (state.adminEl === el) {
      state.adminEl = null;
      state.adminSection = null;
      state.adminGen++;
      syncAdminButton();
    }
    remove();
  };
  state.adminEl = el;
  state.adminIdx = null; // a fresh window has no previous section to slide from
  state.adminEnter = null;
}

export function closeAdmin() {
  state.adminEl?.remove();
}

/// The admin button stays where it is and shows whether the window is open.
export function syncAdminButton() {
  const b = $("#admin-btn");
  if (!b) return;
  b.title = state.adminSection ? "Close Administration" : "Administration";
  b.setAttribute("aria-label", b.title);
  b.classList.toggle("on", !!state.adminSection);
  setHtml(b, ic.admin());
}

function renderAdminNav() {
  const nav = state.adminEl?.querySelector<HTMLElement>(".admin-nav");
  if (!nav || !state.adminSection) return;
  const restore = holdFocus(nav, ".tree-row.sel");
  setHtml(nav, adminTree(state.adminSection));
  restore();
  // Which way the move went, for the entrance. Only a change of section in an
  // open window moves; a repaint of the same one is patched in place.
  const idx = [...nav.querySelectorAll(".tree-row")].findIndex((r) => r.classList.contains("sel"));
  if (state.adminIdx !== null && state.adminIdx !== idx) state.adminEnter = idx > state.adminIdx ? 1 : -1;
  state.adminIdx = idx;
  nav.querySelectorAll<HTMLElement>("[data-admin-section]").forEach((row) => {
    row.onclick = () => openAdmin(row.dataset.adminSection as AdminSection);
  });
}

const adminCtx: AdminCtx = {
  setHtml,
  $,
  openModal,
  askConfirm,
  toast,
  showSecret,
  relTime,
  me: () => state.user,
  connections: () => state.connections,
  reloadConnections: () => loadConnections(),
  openConnectionDialog: (id) => openConnectionDialog(id),
  deleteConnection: (id) => deleteConnection(id),
  openResource: (kind, conn, name) => {
    closeAdmin();
    if (kind === "vm") hooks.selectVmRow(conn, name);
    else if (kind === "ct") hooks.selectCtRow(conn, name);
    else if (kind === "network") hooks.selectNetRow(conn, name);
    else hooks.selectPoolRow(conn, name);
  },
  go: (section, tab) => openAdmin(section, tab),
  createVm: (conn, owner, after) => void openHardwareEditor({ conn, forUser: { owner, after } }),
  createNetwork: (conn, owner, after) => openCreateNetwork(conn, { owner, after }),
  createCt: (conn, owner, after) => void hooks.openCreateCt(conn, { owner, after }),
  createPool: (conn, after) => openCreatePool(conn, after),
  deleteResource: (kind, conn, pool, name, after) => void adminDeleteResource(kind, conn, pool, name, after),
  tab: () => state.adminTab,
  refreshMe: async () => {
    state.user = await api.get<User>("/auth/me");
    await loadConnections();
    if (!canAdmin()) closeAdmin();
    renderApp();
  },
  head: (html) => {
    const slot = $("#admin-head");
    if (slot) setHtml(slot, html);
  },
  stillCurrent: (gen) => gen === state.adminGen && !!state.adminEl?.isConnected,
};

let shownAdmin: AdminSection | null = null;
export function renderAdmin() {
  if (!state.adminSection || !state.adminEl) return;
  const gen = ++state.adminGen;
  // Moving between sections or tabs keeps the old section on screen until the
  // new one is ready, then patches it in; rebuilding blanked the pane for the
  // length of the read.
  const title = state.adminEl.querySelector("#admin-title");
  if (title) title.textContent = adminTitle(state.adminSection);
  // The old section's buttons must not outlive it: they would act on the
  // wrong page until the new one paints its own.
  if (shownAdmin !== state.adminSection) state.adminEl.querySelector("#admin-head")?.replaceChildren();
  shownAdmin = state.adminSection;
  const area = state.adminEl.querySelector<HTMLElement>("#admin-area")!;
  // Stays pending until a slide plays: a render superseded mid-read (a quick
  // second click, a re-click of the same row) must hand the slide on, not eat it.
  const enter = state.adminEnter;
  const main = area.closest<HTMLElement>(".admin-main")!;
  // A new section shows its outline at once and slides in once it has loaded,
  // however long the read takes. The slide is one Web Animation that replaces
  // any still running, so quick clicks cannot stack or fight over classes.
  if (enter) {
    setHtml(area, skeleton("table"));
    main.scrollTop = 0;
  }
  const tabDir = enter ? null : state.adminTabDir;
  // Starts on the first paint of real content, in the same microtask checkpoint
  // as the paint, so no frame shows the page at its final place before the slide
  // begins. (Waiting for the painter to finish let one through whenever it kept
  // awaiting after painting.)
  let played = false;
  const play = () => {
    obs.disconnect();
    if (played || gen !== state.adminGen) return;
    played = true;
    if (enter) state.adminEnter = null;
    if (tabDir) state.adminTabDir = null;
    if (!(enter || tabDir) || !main.animate) return;
    if (typeof matchMedia === "function" && matchMedia("(prefers-reduced-motion: reduce)").matches) return;
    // The skeleton's rise-in (setHtml adds it as the content replaces the
    // outline) would run beside the slide on the same transform; the slide wins.
    area.classList.remove("sk-reveal");
    const frames = (from: string) => [{ opacity: 0, transform: from }, { opacity: 1, transform: "none" }];
    const opts = { duration: 280, easing: "cubic-bezier(.2, .8, .2, 1)" };
    const tabOpts = { ...opts, duration: 380, delay: Math.max(0, EXIT_MS - (Date.now() - state.adminTabAt)), fill: "both" as const };
    if (enter) {
      main.getAnimations().forEach((a) => a.cancel());
      main.animate(frames(`translateY(${enter * 26}px)`), opts);
      return;
    }
    // A tab switch slides only what is under the tab bar, from the tab's side.
    const bar = area.querySelector(".tabbar");
    const moved = [...(bar?.parentElement?.children ?? [])].filter((n) => n !== bar && bar!.compareDocumentPosition(n) & Node.DOCUMENT_POSITION_FOLLOWING);
    if (!moved.length) return;
    main.style.overflowX = "hidden";
    window.setTimeout(() => (main.style.overflowX = ""), EXIT_MS + tabOpts.duration + 50);
    moved.forEach((n) => { n.getAnimations().forEach((a) => a.cancel()); n.animate(frames(`translateX(${tabDir! * 26}px)`), tabOpts); });
  };
  const obs = new MutationObserver(() => { if (!area.querySelector(".skeleton")) play(); });
  if (enter || tabDir) obs.observe(area, { childList: true, subtree: true, characterData: true });
  void renderAdminSection(adminCtx, area, state.adminSection, gen).then(play);
}

/// Present a secret that the server will never show again.
export function showSecret(title: string, secret: string) {
  const el = openModal(
    title,
    `<code class="cmd secret-value" id="sv">${esc(secret)}</code>`,
    `<button class="btn" id="sv-copy">${ic.copy(14)} Copy</button>
     <button class="btn primary" id="sv-done">Done</button>`,
  );
  $("#sv-done", el).onclick = () => el.remove();
  $("#sv-copy", el).onclick = async () => {
    try {
      await navigator.clipboard.writeText(secret);
      toast("Copied");
    } catch {
      toast("Could not copy — select the text instead", "bad");
    }
  };
}
