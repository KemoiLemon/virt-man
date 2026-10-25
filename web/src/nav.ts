// The navigation core: which view is on screen, how it maps to a URL, and how
// a renderer claims the content pane. The view renderers themselves are
// reached through `hooks`, because they import this module. Importing it also
// installs the chart-range and popstate listeners.
import { cacheFirst } from "./api";
import type { ContainerSummary, DomainSummary } from "./api";
import { detachConsole } from "./console";
import { $, setHtml } from "./dom";
import { bindChartHover, setRange } from "./history";
import { hooks } from "./hooks";
import { parsePath, routeToPath, type CtTab, type Route } from "./route";
import { skeleton } from "./skeleton";
import { state, treeCts, treeNets, treePools, treeVms } from "./state";
import { nameOf, uuidOf } from "./util";

/// Tear down a live console: its WebSocket and the stage resize observer.
export function closeConsole() {
  state.consoleResizeObserver?.disconnect();
  state.consoleResizeObserver = null;
  detachConsole();
  state.terminal?.detachTextConsole();
}

/// Navigate away from whatever is on screen. Every path that changes the
/// selected VM or view goes through here, so a console session can never be
/// left streaming in the background.
export function leaveCurrentView() {
  if (state.vmTab === "console") closeConsole();
  // A shell belongs to the page that opened it: leaving the server closes it,
  // whether the user switched tabs, opened a guest, or went to the admin page.
  // The renderConn guard covers the tab-bar clicks that do not come through
  // here.
  if (state.connTab === "shell") state.terminal?.detachTextConsole();
}

/// Never wrap a view that shows live statistics (CPU, memory, rates): a cached
/// figure is a wrong figure, and a chart that jumps on page open looks broken.
/// Those views wait on their skeleton for a fresh read.
///
/// Open a view fast: paint from the web server's last known answers, then
/// re-read for real if any of them were cached. A remote daemon costs a round
/// trip per read, so without this every page sat on its skeleton until the
/// daemon answered. Only for entering a view: the 1s poll wants fresh data,
/// and a view that holds user input must not be painted twice (it would drop
/// what they typed in between).
export async function fastLoad(load: () => Promise<unknown>) {
  let stale = true;
  try {
    stale = await cacheFirst(load);
  } catch {
    // The cached pass failed; the real read below reports the error.
  }
  if (stale) await load();
}

/// The view on screen, as a route. Derived from the selection state rather
/// than stored beside it, so the two cannot disagree.
export function currentRoute(): Route {
  if (!state.selectedConn) return { view: "dc", tab: state.dcTab };
  // URLs name a guest, network or pool by its UUID, which never changes or
  // repeats; the selection itself stays the libvirt name, which every daemon
  // route accepts too. A resource the lists have not loaded yet keeps its
  // name in the URL until they do.
  const c = state.selectedConn;
  if (state.selectedVm && state.ctMode) {
    return { view: "ct", conn: c, ct: uuidOf(treeCts[c], state.selectedVm), tab: state.vmTab as CtTab };
  }
  if (state.selectedVm) return { view: "vm", conn: c, vm: uuidOf(treeVms[c], state.selectedVm), tab: state.vmTab };
  if (state.selectedNet) return { view: "net", conn: c, net: uuidOf(treeNets[c], state.selectedNet) };
  if (state.selectedPool) return { view: "pool", conn: c, pool: uuidOf(treePools[c], state.selectedPool) };
  return { view: "conn", conn: c, tab: state.connTab };
}

/// The daemon path for the open guest: `/domains/<vm>` or `/containers/<ct>`.
export function guestBase(conn: string, name: string, ct = state.ctMode): string {
  return `/proxy/${conn}/${ct ? "containers" : "domains"}/${encodeURIComponent(name)}`;
}

/// The list the open guest belongs to.
export function guestList(conn: string, ct = state.ctMode): (DomainSummary | ContainerSummary)[] | undefined {
  return ct ? treeCts[conn] : treeVms[conn];
}

/// Put the current view in the address bar, so a reload returns to it. Called
/// by every view renderer, which is the one place all navigation passes
/// through: the tree, cards, tabs, the context menu and the palette included.
/// A new view pushes a history entry (Back works); re-rendering the same view
/// (the 1s poll) does not.
export function syncUrl() {
  const path = routeToPath(currentRoute(), state.connections);
  if (location.pathname === path) return;
  state.tableScrollReset = true;
  // A real navigation scrolled the drawer away with the page it belonged to.
  // Closing it HERE and nowhere else is deliberate: this function is the one
  // funnel every way of navigating passes through (tree, cards, tabs, context
  // menu, palette, Back/Forward), and the early return above -- which exists so
  // the 1s poll adds no history -- is also what keeps that poll from shutting a
  // drawer the user just opened. A closer in each call site would go stale the
  // first time someone adds a way to navigate.
  closeDrawer();
  // Restoring a URL (boot, Back/Forward) may normalise it -- a deleted node
  // falls back to "/". That must replace the entry: pushing would add a
  // duplicate at boot and discard the forward history on Back.
  if (state.restoringRoute) history.replaceState(null, "", path);
  else history.pushState(null, "", path);
}

// ----------------------------------------------------------- phone shell
// The sidebar is an off-canvas drawer below 820px; mobile.css does the layout
// and this is the only state that drives it.

export function toggleDrawer(open: boolean) {
  document.body.classList.toggle("nav-open", open);
}

export function closeDrawer() {
  toggleDrawer(false);
}

/// Adopt the view named by the URL. Used at start-up and on Back/Forward.
export function applyRoute(r: Route) {
  leaveCurrentView();
  clearResourceSelection();
  hooks.closeAdmin();
  state.selectedConn = r.view === "dc" || r.view === "admin" ? null : r.conn;
  if (r.view === "admin") {
    // An old /admin link: the window over the datacenter. A non-admin just
    // gets the datacenter.
    state.dcTab = "dashboard";
    if (hooks.canAdmin()) state.pendingAdmin = { section: r.section, tab: r.tab };
    return;
  }
  if (r.view === "dc") state.dcTab = r.tab;
  else if (r.view === "conn") state.connTab = r.tab === "shell" && !hooks.canAdmin() ? "dashboard" : r.tab;
  else if (r.view === "vm") { state.selectedVm = nameOf(treeVms[r.conn], r.vm); state.vmTab = r.tab; }
  else if (r.view === "ct") { state.selectedVm = nameOf(treeCts[r.conn], r.ct); state.ctMode = true; state.vmTab = r.tab; }
  else if (r.view === "net") state.selectedNet = nameOf(treeNets[r.conn], r.net);
  else state.selectedPool = nameOf(treePools[r.conn], r.pool);
}

/// Render whatever the selection state names. The view renderers do the
/// actual work (and sync the URL).
export function renderCurrent() {
  hooks.renderTree();
  // The renderers call syncUrl synchronously, before their first await, so
  // the flag covers exactly their URL write.
  state.restoringRoute = true;
  try {
    renderView();
  } finally {
    state.restoringRoute = false;
  }
  if (state.pendingAdmin) {
    const p = state.pendingAdmin;
    state.pendingAdmin = null;
    hooks.openAdmin(p.section, p.tab);
  }
}

export function renderView() {
  if (state.selectedConn && hooks.isOffline(state.selectedConn)) return hooks.renderOffline();
  if (!state.selectedConn) hooks.renderDc();
  else if (state.selectedVm) void hooks.renderVm();
  else if (state.selectedNet) { showPageSkeleton(); void fastLoad(() => hooks.renderNetPage()); }
  else if (state.selectedPool) { showPageSkeleton(); void hooks.renderPoolPage(); }
  else hooks.renderConn();
}

// Chart range dropdowns: one delegated handler, since the dashboards re-render
// every second. Only the charts' own views carry the bar, so re-running the
// current view's poll body repaints exactly them.
document.addEventListener("change", (e) => {
  const sel = (e.target as Element | null)?.closest?.(".range-sel") as HTMLSelectElement | null;
  if (!sel || !setRange(sel.value)) return;
  if (!state.selectedConn) void hooks.loadDcDashboard();
  else if (state.selectedVm) void hooks.renderVmBody(state.selectedVm);
  else void hooks.loadDashboard();
});
bindChartHover();

window.addEventListener("popstate", () => {
  if (!state.user) return;
  applyRoute(parsePath(location.pathname, state.connections));
  renderCurrent();
});

/// Whether the given guest's tab is still what the pane shows. A tab body that
/// awaits must check before painting, or a late answer overwrites the view the
/// user switched to (same rule as `stillCurrent` for whole-pane renders).
export function stillCurrentVm(conn: string, vm: string, tab: typeof state.vmTab): boolean {
  return state.selectedConn === conn && state.selectedVm === vm && state.vmTab === tab && !!$("#vm-body");
}

/// Replace the pane with a placeholder while a resource page's first read is
/// in flight, so a click answers at once instead of leaving the previous page
/// up until the daemon replies.
export function showPageSkeleton() {
  const content = $("#content");
  if (content) setHtml(content, skeleton("page"));
}

/// Claim ownership of the content pane. Call at the top of anything that
/// paints it after an await, then check `stillCurrent(gen)` just before the
/// paint.
export function beginRender(): number {
  return ++state.renderGen;
}

export function stillCurrent(gen: number): boolean {
  return gen === state.renderGen;
}

/// Clear whichever resource the content pane is showing. Every path that
/// leaves a resource page must run this: the poll re-renders whichever of
/// state.selectedVm/state.selectedNet/state.selectedPool is still set, so a stale selection
/// resurrects that page about a second after the user navigates away.
export function clearResourceSelection() {
  state.selectedVm = null;
  state.ctMode = false;
  state.selectedNet = null;
  state.selectedPool = null;
}
